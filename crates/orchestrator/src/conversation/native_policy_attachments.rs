//! Native main-model Auto Mode and proactivity policy attachments.
//!
//! This is separate from `native_classifier_context`: Native renders the
//! `auto_mode` row for the primary model from its live permission mode, actual
//! tool list, and main-loop prompt model. The classifier prompt is not a
//! substitute for this user-facing system instruction.

use super::ConversationOrchestrator;
use crate::request_context::{rendered_system_message, PromptIngressAttachment};
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ConversationMessage, MessageId};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::{Arc, RwLock};
use tokio::sync::watch;

const BASH_FIRST_ENV: &str = "CLAUDE_CODE_THRIFTY_SONIC";
const BASH_FIRST_STEER_ENV: &str = "CLAUDE_CODE_COZY_TEAPOT";
const MODEL_CAPABILITIES_ENV: &str = "CLAUDE_CODE_MODEL_CAPABILITIES";
const AUTO_MODE_PRIOR_ASSISTANT_CONTEXT_ENV: &str = "CLAUDE_CODE_AUTO_MODE_PRIOR_ASSISTANT_CONTEXT";
const CHILD_SESSION_ENV: &str = "LINGXI_CHILD_SESSION";
const BASH_FIRST_FEATURE: &str = "tengu_thrifty_sonic";
const PROACTIVITY_SELECTOR_FEATURE: &str = "tengu_proactivity_selector";
const DISABLE_PROACTIVITY_ENV: &str = "CLAUDE_CODE_DISABLE_PROACTIVITY";
const AUTO_MODE_TITLE: &str = "Auto Mode Active";

const PROACTIVITY_WITHDRAWN_TEXT: &str = "The proactivity setting announced earlier in this conversation no longer applies. Follow your base instructions about asking the user questions, committing, pushing, and pull requests.";

/// Native's session-scoped Bash-first assignment resolved by the host from
/// model capabilities, cohort assignment, and client data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeBashFirstSessionAssignment {
    Forced,
    None,
    Cohort,
}

/// Current Native proactivity level accepted by `set_proactivity_level`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeProactivityLevel {
    Ask,
    Default,
    Proactive,
}

impl NativeProactivityLevel {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Default => "default",
            Self::Proactive => "proactive",
        }
    }

    pub fn parse_wire(value: &str) -> Option<Self> {
        match value {
            "ask" => Some(Self::Ask),
            "default" => Some(Self::Default),
            "proactive" => Some(Self::Proactive),
            _ => None,
        }
    }
}

impl Default for NativeProactivityLevel {
    fn default() -> Self {
        Self::Default
    }
}

/// Live eligibility inputs for Native's proactivity selector. These are kept
/// separate from `selector_gate`: Native can have its host selector on while
/// bare/attachments-disabled sessions remain excluded from model attachments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeProactivityGates {
    pub attachments_enabled: bool,
    pub selector_feature_enabled: bool,
    pub disabled_by_environment: bool,
}

impl NativeProactivityGates {
    /// Resolve current host gates from the same session inputs used by the
    /// model-facing prompt attachment path.
    #[must_use]
    pub fn current(bare: bool) -> Self {
        Self {
            attachments_enabled: !bare
                && !native_js_env_truthy(std::env::var_os(branding::SIMPLE_ENV).as_deref())
                && !native_js_env_truthy(
                    std::env::var_os(branding::DISABLE_ATTACHMENTS_ENV).as_deref(),
                ),
            selector_feature_enabled: telemetry::feature_flags::flag_bool(
                PROACTIVITY_SELECTOR_FEATURE,
                true,
            ),
            disabled_by_environment: std::env::var_os(DISABLE_PROACTIVITY_ENV)
                .as_deref()
                .is_some_and(|value| !value.is_empty()),
        }
    }
}

/// Snapshot of the session's Native selector and level state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProactivitySnapshot {
    pub opted_in: bool,
    pub selector_gate: bool,
    pub selector_gate_ever_on: bool,
    pub attachments_enabled: bool,
    pub level: NativeProactivityLevel,
    pub permission_baseline_mode: Option<String>,
}

/// Public response body for a successful `set_proactivity_level` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeProactivityResponse {
    pub level: NativeProactivityLevel,
    pub permission_mode: String,
}

#[derive(Debug, Clone)]
struct NativeProactivityStateInner {
    snapshot: NativeProactivitySnapshot,
    gates: NativeProactivityGates,
    session_metadata: lingxi_core::host::NativeProactivitySessionMetadata,
}

/// Session-owned source for `applyProactivityOptInFromInitialize` and
/// `set_proactivity_level`. Clones share current app state and a change
/// subscription so Desktop, Headless, and prompt construction observe the same
/// explicit opt-in and level.
#[derive(Clone)]
pub struct NativeProactivityState {
    inner: Arc<RwLock<NativeProactivityStateInner>>,
    changed: watch::Sender<NativeProactivitySnapshot>,
}

impl Default for NativeProactivityState {
    fn default() -> Self {
        let gates = NativeProactivityGates {
            attachments_enabled: true,
            selector_feature_enabled: true,
            disabled_by_environment: false,
        };
        let initial = NativeProactivitySnapshot {
            opted_in: false,
            selector_gate: false,
            selector_gate_ever_on: false,
            attachments_enabled: gates.attachments_enabled,
            level: NativeProactivityLevel::Default,
            permission_baseline_mode: None,
        };
        let (changed, _receiver) = watch::channel(initial.clone());
        Self {
            inner: Arc::new(RwLock::new(NativeProactivityStateInner {
                snapshot: initial,
                gates,
                session_metadata: lingxi_core::host::NativeProactivitySessionMetadata::default(),
            })),
            changed,
        }
    }
}

impl NativeProactivityState {
    /// Subscribe to session selector/level changes. Call `refresh_gates` on
    /// source changes; each request also refreshes them before rendering.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<NativeProactivitySnapshot> {
        self.changed.subscribe()
    }

    /// Re-evaluate the source gates and notify subscribers if the effective
    /// selector or attachment eligibility changed.
    pub fn refresh_gates(&self, gates: NativeProactivityGates) -> NativeProactivitySnapshot {
        let snapshot = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.gates = gates;
            inner.snapshot.attachments_enabled = gates.attachments_enabled;
            inner.snapshot.selector_gate = inner.snapshot.opted_in
                && gates.selector_feature_enabled
                && !gates.disabled_by_environment;
            if inner.snapshot.selector_gate {
                inner.snapshot.selector_gate_ever_on = true;
            }
            inner.snapshot.clone()
        };
        self.publish(snapshot.clone());
        snapshot
    }

    /// Apply Native's initialize field. Missing/non-boolean input has no
    /// effect; a false reinitialize is an early no-op, while a regular false
    /// clears opt-in without resetting the app-state level or permission mode.
    pub fn apply_initialize_opt_in(
        &self,
        value: Option<bool>,
        reinitialize: bool,
    ) -> NativeProactivitySnapshot {
        let snapshot = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(opted_in) = value.filter(|value| !reinitialize || *value) {
                inner.snapshot.opted_in = opted_in;
                inner.snapshot.selector_gate = opted_in
                    && inner.gates.selector_feature_enabled
                    && !inner.gates.disabled_by_environment;
                if inner.snapshot.selector_gate {
                    inner.snapshot.selector_gate_ever_on = true;
                }
                inner.session_metadata.internal = Some(proactivity_internal_metadata(opted_in));
                if inner.snapshot.selector_gate && inner.snapshot.attachments_enabled {
                    inner.session_metadata.external =
                        Some(proactivity_external_level(inner.snapshot.level));
                } else {
                    inner.session_metadata.external = Some(proactivity_external_clear());
                }
            }
            inner.snapshot.clone()
        };
        self.publish(snapshot.clone());
        snapshot
    }

    /// Return a coherent live snapshot after the caller refreshed host gates.
    #[must_use]
    pub fn snapshot(&self) -> NativeProactivitySnapshot {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .snapshot
            .clone()
    }

    /// Return Native's internal opt-in and external level mirror for the
    /// optional worker-state callback. This is not appended to the local JSONL.
    #[must_use]
    pub fn session_metadata(&self) -> lingxi_core::host::NativeProactivitySessionMetadata {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_metadata
            .clone()
    }

    /// Restore from an explicit Native worker-state carrier. The external
    /// level is accepted only when its internal opt-in bit is exactly `true`;
    /// local transcript replay must not call this method. Hydration does not
    /// run a permission-mode transition.
    pub fn restore_session_metadata(
        &self,
        metadata: lingxi_core::host::NativeProactivitySessionMetadata,
        gates: NativeProactivityGates,
    ) -> NativeProactivitySnapshot {
        use lingxi_core::types::utf16_json::JsonValueView;

        let opted_in = metadata
            .internal
            .as_ref()
            .and_then(|value| value.at_pointer("/proactivity_opt_in"))
            .and_then(JsonValueView::as_bool)
            == Some(true);
        let level = if opted_in {
            metadata
                .external
                .as_ref()
                .and_then(|value| value.at_pointer("/proactivity/level"))
                .and_then(JsonValueView::as_str)
                .and_then(NativeProactivityLevel::parse_wire)
                .unwrap_or_default()
        } else {
            NativeProactivityLevel::Default
        };
        let selector_gate =
            opted_in && gates.selector_feature_enabled && !gates.disabled_by_environment;
        let session_metadata = if opted_in {
            metadata
        } else {
            lingxi_core::host::NativeProactivitySessionMetadata {
                internal: metadata.internal,
                external: metadata.external.map(|_| proactivity_external_clear()),
            }
        };
        let snapshot = NativeProactivitySnapshot {
            opted_in,
            selector_gate,
            selector_gate_ever_on: selector_gate,
            attachments_enabled: gates.attachments_enabled,
            level,
            permission_baseline_mode: None,
        };
        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.gates = gates;
            inner.snapshot = snapshot.clone();
            inner.session_metadata = session_metadata;
        }
        self.publish(snapshot.clone());
        snapshot
    }

    /// Clear conversation-owned opt-in and level when a runtime switches to a
    /// new session; current host gates remain live.
    pub fn reset(&self, gates: NativeProactivityGates) -> NativeProactivitySnapshot {
        let snapshot = NativeProactivitySnapshot {
            opted_in: false,
            selector_gate: false,
            selector_gate_ever_on: false,
            attachments_enabled: gates.attachments_enabled,
            level: NativeProactivityLevel::Default,
            permission_baseline_mode: None,
        };
        {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner.gates = gates;
            inner.snapshot = snapshot.clone();
            inner.session_metadata = lingxi_core::host::NativeProactivitySessionMetadata::default();
        }
        self.publish(snapshot.clone());
        snapshot
    }

    /// Validate a public level change against Native's attachment and selector
    /// gates without mutating the current level.
    pub fn validate_level_change(&self) -> Result<(), String> {
        let snapshot = self.snapshot();
        if !snapshot.attachments_enabled {
            return Err("Cannot set proactivity: levels are not enabled for this session — the session is excluded (bare, or attachments disabled)".into());
        }
        if !snapshot.selector_gate {
            return Err("Cannot set proactivity: levels are not enabled for this session (the host must send proactivity: true on initialize, with no kill switch set)".into());
        }
        Ok(())
    }

    /// Read the Native baseline used to restore the ordinary permission mode
    /// when a level no longer selects a more permissive mode.
    #[must_use]
    pub fn permission_baseline_mode(&self, current_mode: &str) -> String {
        self.snapshot()
            .permission_baseline_mode
            .unwrap_or_else(|| current_mode.to_owned())
    }

    /// Commit a level after its permission-mode transition succeeds.
    pub fn commit_level(
        &self,
        level: NativeProactivityLevel,
        previous_mode: &str,
    ) -> NativeProactivitySnapshot {
        let snapshot = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            inner
                .snapshot
                .permission_baseline_mode
                .get_or_insert_with(|| previous_mode.to_owned());
            inner.snapshot.level = level;
            inner.session_metadata.external = Some(proactivity_external_level(level));
            inner.snapshot.clone()
        };
        self.publish(snapshot.clone());
        snapshot
    }

    fn publish(&self, snapshot: NativeProactivitySnapshot) {
        self.changed.send_if_modified(|current| {
            if *current == snapshot {
                false
            } else {
                *current = snapshot;
                true
            }
        });
    }
}

fn native_js_env_truthy(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Live facts the host can authoritatively source from its current session.
/// `None` means the host has no corresponding value; policy generation keeps
/// the Native default or omits an active announcement rather than guessing.
#[derive(Debug, Clone, Default)]
pub struct NativePolicyHostFacts {
    /// Native `jK(HJ(mainLoopModel))` result for the model facing this request.
    pub simple_system_prompt: Option<bool>,
    /// Native `S(We(HJ(et())))` model classification used by `Igs` fallback.
    pub model_is_simple: Option<bool>,
    /// Native host's `bashFirstSessionAssignment()` result.
    pub bash_first_assignment: Option<NativeBashFirstSessionAssignment>,
    /// Resolved `tengu_thrifty_sonic` flag value, used only for Cohort.
    pub bash_first_cohort_enabled: Option<bool>,
    /// Native host `bashFirstSteerVariant()` output, when available.
    pub bash_first_steer_variant: Option<String>,
    /// Host `priorAssistantContext` fact used by `O6t`.
    pub prior_assistant_context: Option<bool>,
    /// Native `agentId`, when the main model belongs to a child agent.
    pub agent_id: Option<String>,
    /// Native `xc()` team/agent execution context, when the host has one.
    pub is_team_agent_context: Option<bool>,
    /// Native `host.proactivity.selectorGate()`; unset sessions start gated off.
    pub proactivity_selector_gate: Option<bool>,
    /// Native `getProactivityLevel()` value. Required to emit an active row.
    pub proactivity_level: Option<NativeProactivityLevel>,
    /// Whether Native attachment rendering is enabled for this session.
    pub attachments_enabled: Option<bool>,
}

fn source_object(fields: Vec<(&str, Source)>) -> Source {
    Source::object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
    .expect("Native policy payload has unique field names")
}

fn source_bool(value: bool) -> Source {
    Source::from_serde_value(&serde_json::Value::Bool(value))
        .expect("boolean source projection is valid")
}

fn source_null() -> Source {
    Source::parse("null").expect("null is valid JSON")
}

fn proactivity_internal_metadata(opted_in: bool) -> Source {
    source_object(vec![(
        "proactivity_opt_in",
        if opted_in {
            source_bool(true)
        } else {
            source_null()
        },
    )])
}

fn proactivity_external_level(level: NativeProactivityLevel) -> Source {
    let state = source_object(vec![("level", Source::from(level.as_wire()))]);
    source_object(vec![("proactivity", state)])
}

fn proactivity_external_clear() -> Source {
    source_object(vec![("proactivity", source_null())])
}

fn truthy(source: &Source) -> bool {
    if let Some(value) = source.at_pointer("") {
        if value.is_null() {
            return false;
        }
        if let Some(value) = value.as_bool() {
            return value;
        }
        if let Some(value) = source.string_at("") {
            return !value.is_empty();
        }
        if let Some(value) = value.as_number() {
            return value != 0.0 && !value.is_nan();
        }
        // JavaScript arrays and objects are truthy, including empty ones.
        return true;
    }
    false
}

fn truthy_field(source: &Source, pointer: &str) -> bool {
    source
        .subprojection(pointer)
        .ok()
        .is_some_and(|value| truthy(&value))
}

fn latest_auto_mode<'a>(
    history: &[ConversationMessage],
    attachments: &'a HashMap<MessageId, Source>,
) -> Option<&'a Source> {
    for message in history.iter().rev() {
        let Some(payload) = attachments.get(&message.id()) else {
            continue;
        };
        match payload.string_at("/type") {
            Some("auto_mode") => return Some(payload),
            Some("auto_mode_exit") => return None,
            _ => {}
        }
    }
    None
}

fn has_native_auto_mode_toolset(tools: &[String]) -> bool {
    tools.iter().any(|name| name == "Bash")
        && tools.iter().any(|name| name == "Edit" || name == "Write")
}

fn bash_first_value(tools: &[String], facts: &NativePolicyHostFacts) -> Source {
    let env_override = std::env::var_os(BASH_FIRST_ENV)
        .map(|value| Source::from(value.to_string_lossy().into_owned()));
    bash_first_value_with_environment(tools, facts, env_override)
}

fn bash_first_value_with_environment(
    tools: &[String],
    facts: &NativePolicyHostFacts,
    env_override: Option<Source>,
) -> Source {
    if !has_native_auto_mode_toolset(tools) {
        return source_bool(false);
    }
    if let Some(value) = env_override {
        // Native returns the original process.env string, and JavaScript's
        // subsequent truthiness check treats every non-empty string as true.
        return value;
    }
    let enabled = match facts.bash_first_assignment {
        Some(NativeBashFirstSessionAssignment::Forced) => true,
        Some(NativeBashFirstSessionAssignment::Cohort) => facts
            .bash_first_cohort_enabled
            .unwrap_or_else(|| telemetry::feature_flags::flag_bool(BASH_FIRST_FEATURE, false)),
        Some(NativeBashFirstSessionAssignment::None) | None => false,
    };
    source_bool(enabled)
}

/// Resolve the local model-catalog part of Native's `bashFirstSessionAssignment`.
///
/// Native `h_(model, capability)` first honors `CLAUDE_CODE_MODEL_CAPABILITIES`,
/// then served/baked model capabilities. LingXi has the same process override
/// parser and a baked catalog, but no served capability or client-data source.
/// The missing remote facts remain unknown unless a real host provider supplies
/// them through `bash_first_assignment`.
fn native_model_capability(
    model: &str,
    capability: &str,
    model_capabilities: Option<&str>,
) -> Option<bool> {
    lingxi_core::host::model_capabilities::capability_override(
        model,
        capability,
        model_capabilities,
    )
    .or_else(|| {
        lingxi_core::host::model_capabilities::capabilities_for_loose(model)
            .contains(&capability)
            .then_some(true)
    })
}

/// Native `tGn()` suppresses the Fable 5.1 / Opus 5.5 model-capability path in
/// local-agent and remote-cowork entrypoints. LingXi's local orchestrator has
/// no remote-cowork async context; its process entrypoint and child-session
/// alias are the only corresponding current host facts.
fn native_bash_first_model_capability_gate_disabled_for(
    entrypoint: Option<&str>,
    child_session: bool,
) -> bool {
    !child_session
        && matches!(
            entrypoint,
            Some("local-agent" | "local_agent" | "remote_cowork" | "remote_cowork_trigger")
        )
}

fn native_bash_first_model_capability_gate_disabled() -> bool {
    native_bash_first_model_capability_gate_disabled_for(
        std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref(),
        env_is_js_truthy(CHILD_SESSION_ENV),
    )
}

/// Resolve only the model-derived part of Native's session assignment. Client
/// data can still explicitly force or disable this through a host-supplied
/// assignment; absent that remote source, the current model directory and the
/// Native process override are authoritative.
fn native_bash_first_assignment_for_model(
    model: Option<&str>,
    model_capabilities: Option<&str>,
    model_capability_gate_disabled: bool,
) -> Option<NativeBashFirstSessionAssignment> {
    let model = model.filter(|model| !model.is_empty())?;
    let thrifty_sonic = native_model_capability(model, "thrifty_sonic", model_capabilities);
    if thrifty_sonic == Some(false) {
        return Some(NativeBashFirstSessionAssignment::None);
    }
    if thrifty_sonic == Some(true) {
        return Some(NativeBashFirstSessionAssignment::Forced);
    }
    if !model_capability_gate_disabled
        && (native_model_capability(model, "fable_5_1_prompt_bundle", model_capabilities)
            == Some(true)
            || native_model_capability(model, "opus_5_5_prompt_bundle", model_capabilities)
                == Some(true))
    {
        return Some(NativeBashFirstSessionAssignment::Forced);
    }
    if lingxi_core::host::model_capabilities::normalize_model_id(model) == "claude-opus-5" {
        return Some(NativeBashFirstSessionAssignment::Cohort);
    }
    // A missing local model predicate does not prove that Native's current
    // account client-data map has no `tengu_thrifty_sonic` assignment.
    None
}

/// Native `S(We(HJ(mainLoopModel)))` model fallback for `Igs()`'s steer
/// variant. This is the Opus 5.5 prompt-bundle capability, not the separate
/// short system-prompt predicate.
fn native_bash_first_model_is_simple(
    model: Option<&str>,
    model_capabilities: Option<&str>,
    model_capability_gate_disabled: bool,
) -> Option<bool> {
    let model = model.filter(|model| !model.is_empty())?;
    Some(
        !model_capability_gate_disabled
            && native_model_capability(model, "opus_5_5_prompt_bundle", model_capabilities)
                == Some(true),
    )
}

fn bash_first_steer_value(previous: Option<&Source>, facts: &NativePolicyHostFacts) -> Source {
    if let Some(previous) = previous {
        if let Ok(value) = previous.subprojection("/bashFirstSteer") {
            if !value
                .at_pointer("")
                .is_some_and(|projection| projection.is_null())
            {
                return value;
            }
        }
        // lPr uses `previous.bashFirstSteer ?? "strict"` when a previous
        // auto_mode row exists; it does not consult Igs in this branch.
        return Source::from("strict");
    }
    if let Some(value) = std::env::var_os(BASH_FIRST_STEER_ENV) {
        // Native uses nullish coalescing here, so even an empty environment
        // string remains a present source value.
        return Source::from(value.to_string_lossy().into_owned());
    }
    if let Some(variant) = facts.bash_first_steer_variant.as_deref() {
        return Source::from(variant);
    }
    // Native's host fallback uses model simplicity before the remote variant,
    // then defaults to strict. The host provider supplies a resolved variant
    // whenever its client-data source is available.
    Source::from(if facts.model_is_simple == Some(true) {
        "relaxed"
    } else {
        "strict"
    })
}

fn auto_mode_consent_flow_value(bypass: bool, facts: &NativePolicyHostFacts) -> Source {
    if bypass
        || facts.agent_id.is_some()
        || facts.is_team_agent_context.unwrap_or(false)
        || env_is_js_truthy(CHILD_SESSION_ENV)
    {
        return source_bool(false);
    }
    if let Some(value) = std::env::var_os(AUTO_MODE_PRIOR_ASSISTANT_CONTEXT_ENV) {
        // Native `Nso().value` preserves the environment source value; it is
        // not normalized to a boolean before attachment serialization.
        return Source::from(value.to_string_lossy().into_owned());
    }
    source_bool(facts.prior_assistant_context.unwrap_or(false))
}

fn env_is_js_truthy(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn auto_mode_payload(
    permission_mode: Option<&str>,
    simple_prompt_model: bool,
    tools: &[String],
    previous: Option<&Source>,
    facts: &NativePolicyHostFacts,
) -> Option<Source> {
    let bypass = permission_mode == Some("bypassPermissions");
    if permission_mode != Some("auto") && !bypass {
        return None;
    }

    // Native lPr derives this from the actual main-loop prompt model (jK), not
    // classifier context or the stored raw model selection.
    let steer_only = bypass || simple_prompt_model;
    let bash_first = bash_first_value(tools, facts);
    let bash_first_enabled = truthy(&bash_first);
    if steer_only && !bash_first_enabled {
        return None;
    }
    if previous.is_some_and(|previous| !(truthy_field(previous, "/steerOnly") && !steer_only)) {
        return None;
    }

    let mut fields = vec![
        ("type", Source::from("auto_mode")),
        (
            "autoModeConsentFlow",
            auto_mode_consent_flow_value(bypass, facts),
        ),
        ("bashFirst", bash_first),
    ];
    if bash_first_enabled {
        fields.push(("bashFirstSteer", bash_first_steer_value(previous, facts)));
    }
    fields.push(("steerOnly", source_bool(steer_only)));
    fields.push(("bypass", source_bool(bypass)));
    Some(source_object(fields))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProactivityState {
    level: String,
    ask_user_question: bool,
    enter_plan_mode: bool,
    schedule_wakeup: bool,
    cron_create: bool,
}

fn parse_proactivity_state(payload: &Source) -> Option<ProactivityState> {
    let level = payload.string_at("/level")?;
    if !matches!(level, "ask" | "default" | "proactive") {
        return None;
    }
    Some(ProactivityState {
        level: level.to_owned(),
        ask_user_question: payload.at_pointer("/tools/askUserQuestion")?.as_bool()?,
        enter_plan_mode: payload.at_pointer("/tools/enterPlanMode")?.as_bool()?,
        schedule_wakeup: payload.at_pointer("/tools/scheduleWakeup")?.as_bool()?,
        cron_create: payload.at_pointer("/tools/cronCreate")?.as_bool()?,
    })
}

fn latest_valid_proactivity(
    history: &[ConversationMessage],
    attachments: &HashMap<MessageId, Source>,
) -> Option<ProactivityState> {
    for message in history.iter().rev() {
        let Some(payload) = attachments.get(&message.id()) else {
            continue;
        };
        if payload.string_at("/type") != Some("proactivity") {
            continue;
        }
        if payload.string_at("/status") == Some("withdrawn") {
            return None;
        }
        if let Some(state) = parse_proactivity_state(payload) {
            return Some(state);
        }
    }
    None
}

fn proactivity_payload(
    previous: Option<&ProactivityState>,
    facts: &NativePolicyHostFacts,
    tools: &[String],
) -> Option<Source> {
    let selector_enabled = facts.attachments_enabled.unwrap_or(true)
        && facts.proactivity_selector_gate.unwrap_or(false);
    if !selector_enabled {
        return previous.map(|_| {
            source_object(vec![
                ("type", Source::from("proactivity")),
                ("status", Source::from("withdrawn")),
            ])
        });
    }
    let Some(level) = facts.proactivity_level else {
        // A gate without the Native host's current level cannot make a valid
        // active row; retain the previous announcement until the host supplies
        // that source rather than inventing a level.
        return None;
    };
    let state = ProactivityState {
        level: level.as_wire().to_owned(),
        ask_user_question: tools.iter().any(|name| name == "AskUserQuestion"),
        enter_plan_mode: tools.iter().any(|name| name == "EnterPlanMode"),
        schedule_wakeup: tools.iter().any(|name| name == "ScheduleWakeup"),
        cron_create: tools.iter().any(|name| name == "CronCreate"),
    };
    if previous == Some(&state) {
        return None;
    }
    Some(source_object(vec![
        ("type", Source::from("proactivity")),
        ("status", Source::from("active")),
        ("level", Source::from(state.level)),
        (
            "tools",
            source_object(vec![
                ("askUserQuestion", source_bool(state.ask_user_question)),
                ("enterPlanMode", source_bool(state.enter_plan_mode)),
                ("scheduleWakeup", source_bool(state.schedule_wakeup)),
                ("cronCreate", source_bool(state.cron_create)),
            ]),
        ),
    ]))
}

fn model_attachment(payload: Source, body: String) -> Option<PromptIngressAttachment> {
    let message = ConversationMessage::user_meta(MessageId::new(), body);
    let rendered = rendered_system_message(&message)?;
    Some(PromptIngressAttachment {
        message,
        payload,
        rendered,
    })
}

fn render_proactivity_withdrawal() -> String {
    PROACTIVITY_WITHDRAWN_TEXT.to_owned()
}

fn render_proactivity_active(level: &str, tools: &ProactivityState) -> String {
    let intensity = match level {
        "ask" => "low",
        "default" => "medium",
        "proactive" => "high",
        _ => unreachable!("NativeProactivityLevel only has the three source levels"),
    };
    let heading = format!(
        "## Proactivity: {intensity}\n\nThe user has set how much initiative they want you to take in this session. This supersedes any earlier proactivity setting. It does not override plan mode, the current permission mode, or the rules about destructive and hard-to-reverse actions."
    );
    match level {
        "ask" => format!(
            "{heading}\n\n- Confirm before changing anything, including file edits: {} before implementing, and{} whenever intent, scope, or approach has more than one reasonable reading. File edits prompt the user for approval in this mode; don't route around that (no shell redirection or scripts to write files).\n- Skip both for a request that is small and unambiguous, or that turns out to need no changes — just do it, or say so.\n- Never run `git commit`, push to a remote, or create or update a pull request unless the user explicitly confirms it in this conversation. Finishing a task is not confirmation.\n- When a task is done, report back and stop; suggest next steps rather than starting them.",
            if tools.enter_plan_mode {
                "enter plan mode with EnterPlanMode and get the plan agreed"
            } else {
                "lay out your plan and get it agreed"
            },
            if tools.ask_user_question {
                " use AskUserQuestion"
            } else {
                " ask"
            }
        ),
        "default" => format!(
            "{heading}\n\n- {} only when you genuinely cannot proceed without an answer — a decision that is the user's to make and that you cannot resolve from the request, the code, or sensible defaults. Otherwise make the reasonable call, state it, and continue.\n- Make the edits and run the checks the task needs without stopping to ask. Do not create git commits, push to a remote, or create or update pull requests unless the user explicitly asks or confirms.",
            if tools.ask_user_question {
                "Use AskUserQuestion"
            } else {
                "Ask the user"
            }
        ),
        "proactive" => {
            let mut scheduling = Vec::new();
            if tools.schedule_wakeup {
                scheduling.push("a check-in with ScheduleWakeup".to_owned());
            }
            if tools.cron_create {
                scheduling.push("a recurring CronCreate job (the /loop skill)".to_owned());
            }
            let routine = "a routine via the /schedule skill when it is available and the follow-up should outlive this session";
            let schedule_advice = if scheduling.is_empty() {
                format!(
                    " Instead of stopping to wait on something slow (CI, a deploy, a long-running job, a reviewer), schedule {routine}, and pick the work back up when it fires."
                )
            } else {
                format!(
                    " Instead of stopping to wait on something slow (CI, a deploy, a long-running job, a reviewer), schedule {} — or {routine} — and pick the work back up when it fires.",
                    scheduling.join(" or ")
                )
            };
            format!(
                "{heading}\n\n- Do not ask questions{} unless the request is truly ambiguous and proceeding under any reading would waste significant work. Otherwise make the reasonable call, state it briefly, and proceed; the user will redirect you if needed.\n- Commit, push, and create or update pull requests without asking whenever that is the natural next step of the task. This overrides the general instructions to only commit, push, or open a pull request when the user explicitly asks. Still review what you are staging, keep secrets out of commits, and never force-push or rewrite shared history unprompted.\n- When a task finishes, put yourself in the user's shoes: what would they ask for next once they saw this result? Do it before being asked — run the tests and fix what fails, fix lint and type errors, update affected docs, tie off loose ends you created, and pick up the follow-up work the task exposed (an adjacent bug, a missing test, the same fix needed elsewhere). Leave anything that would change what the user asked for to them.\n- Work whose outcome arrives later stays yours until the outcome is in. When you add an A/B test or product experiment, landing the code is the start: schedule a check-in a day after it starts, using whichever of those will still fire then, and a prompt a fresh session could act on (if none will, tell the user what to check and when). At each check-in, verify that exposures and metric events are logging correctly and the split matches the configured allocation, fix what is broken, ramp, restart, or extend it as the data calls for, and schedule the next check-in. Where a step is blocked or needs access you lack, hand the user the exact change to make. Once the test has run its planned length and the result is statistically significant or clearly will not be, show the user the numbers and the arm you propose to ship; shipping it is their call.{schedule_advice}\n- Keep the user informed with brief statements of what you did and what you are doing next, rather than requests for permission.",
                if tools.ask_user_question {
                    " (including via AskUserQuestion)"
                } else {
                    ""
                },
            )
        }
        _ => unreachable!("NativeProactivityLevel only has the three source levels"),
    }
}

fn render_auto_mode(payload: &Source) -> String {
    let bash_first = truthy_field(payload, "/bashFirst");
    let consent_flow = truthy_field(payload, "/autoModeConsentFlow");
    let steer_only = truthy_field(payload, "/steerOnly");
    let bypass = truthy_field(payload, "/bypass");
    let relaxed = payload.string_at("/bashFirstSteer") == Some("relaxed");

    let heading = format!(
        "## {AUTO_MODE_TITLE}\n\nBias toward working without stopping for clarifying questions — when you'd normally pause to check, make the reasonable call and keep going; they'll redirect you if needed. If the user, a skill, or the shape of the task suggests they want you to ask (with AskUserQuestion or otherwise), do so. And even absent that signal, it's still fine to stop when you're genuinely blocked — unclear direction, missing input, a decision only they can make.\n\nBefore any command that could discard uncommitted work — `git checkout`/`restore`/`reset`/`clean`, `rm -rf` in the repo, restoring from a snapshot — run `git status` first and stash (with `-u` for untracked) or commit anything that's there. When staging or committing, review what's included (`git status` after a broad `git add`), and if you see anything suspicious that might reveal secrets — even if the filename looks innocuous — double-check the file's contents before pushing."
    );

    let consent = if consent_flow {
        "\n\nWhen the auto-mode classifier blocks an action (or you anticipate it would): first try an alternative that no rule blocks — a feature branch instead of the default branch, a synthetic or sanitized stand-in instead of real data, a narrower scope — and continue the task. Otherwise hold the ask and batch it with your other outstanding asks for when all your other parallel work is done or paused on subagents mid-flight. Raise every held ask before you end your turn or declare the task done — never silently drop one. Whenever you raise a consent ask — a single item or a batch — make each item a single concise sentence naming its action and, in **bold**, the item that makes it need consent; the user replies with which items they approve (or \"all of them\"). If you believe a block is wrong, ask that directly too (\"auto mode blocked X because Y — is that wrong?\").\n\nFor example:\n- blocked: push to main → pushed to a feature branch instead, carried on\n- blocked: real customer emails in a test fixture → generated synthetic ones, carried on\n- blocked: publish to the public registry, no alternative → held the ask, kept writing the docs\n- docs done, subagents still running → raised one batched ask, all held items together:\n  \"1. publish **the package to the public npm registry** — approve?\n  2. delete the **old production fixtures bucket** — approve? (or 'all of them')\""
    } else {
        ""
    };

    let shell_guidance = if relaxed {
        "You can do much of your work through the Bash tool when it is the simpler route: read files with cat, head, or sed -n, search with grep and find, and make small, mechanical file changes with sed, heredocs, or short scripts instead of the dedicated Read, Edit, or Write tools. The choice is yours: prefer Edit or Write when a shell edit would be fragile, such as exact or multi-line replacements, or sed/awk flags that differ between GNU and BSD/macOS."
    } else {
        "Do your work through the Bash tool wherever it can accomplish the job: read files with cat, head, or sed -n, search with grep and find, and make file changes with sed, heredocs, or short scripts, rather than using the dedicated Read, Edit, or Write tools. Fall back to a dedicated tool only when Bash genuinely cannot do the job."
    };
    if bypass {
        return format!("While bypass permissions mode is active:\n\n{shell_guidance}");
    }
    if steer_only {
        return format!("While auto mode is active:\n\n{shell_guidance}");
    }
    let mut body = format!("{heading}{consent}");
    if bash_first {
        body.push_str("\n\n");
        body.push_str(shell_guidance);
    }
    body
}

fn auto_mode_exit_payload(previous: Option<&Source>) -> Option<Source> {
    let previous = previous?;
    let mut fields = vec![("type", Source::from("auto_mode_exit"))];
    if let Ok(value) = previous.subprojection("/bashFirst") {
        fields.push(("bashFirst", value));
    }
    if let Ok(value) = previous.subprojection("/steerOnly") {
        fields.push(("steerOnly", value));
    }
    Some(source_object(fields))
}

fn should_emit_auto_mode_exit(
    exit_pending: bool,
    has_agent_identity: bool,
    permission_mode: Option<&str>,
    active_auto_mode: bool,
    previous_auto_mode: Option<&Source>,
) -> bool {
    exit_pending
        && !has_agent_identity
        && permission_mode != Some("auto")
        && !active_auto_mode
        && previous_auto_mode.is_some()
}

fn render_auto_mode_exit(payload: &Source) -> String {
    let suffix = if truthy_field(payload, "/bashFirst") {
        " Resume using the dedicated tools for file reads, searches, and edits."
    } else {
        ""
    };
    if truthy_field(payload, "/steerOnly") {
        format!("## Exited Auto Mode\n\nYou have exited auto mode.{suffix}")
    } else {
        format!(
            "## Exited Auto Mode\n\nYou have exited auto mode. The user may now want to interact more directly. You should ask clarifying questions when the approach is ambiguous rather than making assumptions.{suffix}"
        )
    }
}

impl ConversationOrchestrator {
    /// Native uses the same bare/simple/attachments-disabled gate for its
    /// model-facing policy announcements and settings deltas.
    #[must_use]
    pub(crate) fn native_prompt_attachments_enabled(&self) -> bool {
        NativeProactivityGates::current(self.config.bare).attachments_enabled
    }

    /// Return the current proactivity state after sampling the live Host gates.
    #[must_use]
    pub fn current_native_proactivity_snapshot(&self) -> NativeProactivitySnapshot {
        self.native_proactivity_state
            .refresh_gates(NativeProactivityGates::current(self.config.bare))
    }

    /// Whether the current Host gates allow a client to request explicit
    /// session opt-in, independent of the current opted-in selector state.
    #[must_use]
    pub fn native_proactivity_opt_in_available(&self) -> bool {
        let gates = NativeProactivityGates::current(self.config.bare);
        self.native_proactivity_state.refresh_gates(gates);
        gates.attachments_enabled
            && gates.selector_feature_enabled
            && !gates.disabled_by_environment
    }

    /// Restore an explicit host-provided Native worker-state mirror. The
    /// internal opt-in bit is the sole authority; this does not infer opt-in
    /// from a local transcript row or transition permission mode.
    pub fn restore_native_proactivity_session_metadata(
        &self,
        metadata: lingxi_core::host::NativeProactivitySessionMetadata,
    ) -> NativeProactivitySnapshot {
        self.native_proactivity_state
            .restore_session_metadata(metadata, NativeProactivityGates::current(self.config.bare))
    }

    pub(crate) fn reset_native_proactivity_session(&self) {
        self.native_proactivity_state
            .reset(NativeProactivityGates::current(self.config.bare));
    }

    /// Apply the stream-json / host initialize opt-in through the shared
    /// session-owned proactivity state.
    pub fn apply_native_proactivity_initialize(
        &self,
        value: Option<bool>,
        reinitialize: bool,
    ) -> NativeProactivitySnapshot {
        self.native_proactivity_state
            .refresh_gates(NativeProactivityGates::current(self.config.bare));
        self.native_proactivity_state
            .apply_initialize_opt_in(value, reinitialize)
    }

    /// Refresh and validate the live gates before decoding the remaining
    /// `set_proactivity_level` fields, preserving Native's gate-first error
    /// ordering for malformed public requests.
    pub fn validate_native_proactivity_level_change(&self) -> Result<(), String> {
        self.native_proactivity_state
            .refresh_gates(NativeProactivityGates::current(self.config.bare));
        self.native_proactivity_state.validate_level_change()
    }

    /// Apply Native's `set_proactivity_level` state and corresponding
    /// permission-mode transition through the current host's live permission
    /// gate. Private account-cloud opt-in is a separate Native capability and
    /// is deliberately not synthesized by this API.
    pub async fn set_native_proactivity_level(
        &self,
        level: &str,
        offer_bypass: Option<bool>,
    ) -> Result<NativeProactivityResponse, String> {
        self.validate_native_proactivity_level_change()?;
        let level = NativeProactivityLevel::parse_wire(level).ok_or_else(|| {
            format!(
                "Cannot set proactivity: unrecognized level {level} (expected one of ask, default, proactive)"
            )
        })?;
        let current_mode = self
            .permission_mode()
            .unwrap_or_else(|| "default".to_owned());
        let baseline_mode = self
            .native_proactivity_state
            .permission_baseline_mode(&current_mode);
        let selector_ever_on = self
            .native_proactivity_state
            .snapshot()
            .selector_gate_ever_on;
        let permission_mode = self
            .perms
            .set_permission_mode_for_proactivity(
                level.as_wire(),
                offer_bypass,
                &baseline_mode,
                selector_ever_on,
            )
            .await?;
        let snapshot = self
            .native_proactivity_state
            .commit_level(level, &current_mode);
        Ok(NativeProactivityResponse {
            level: snapshot.level,
            permission_mode: permission_mode
                .or_else(|| self.permission_mode())
                .unwrap_or(current_mode),
        })
    }

    /// Return Native's live primary-model Auto Mode announcement and any
    /// required withdrawal of an earlier proactivity announcement.
    pub(crate) async fn native_policy_prompt_ingress_attachments(
        &self,
        prior_history: &[ConversationMessage],
    ) -> Vec<PromptIngressAttachment> {
        let gates = NativeProactivityGates::current(self.config.bare);
        if !gates.attachments_enabled {
            return Vec::new();
        }
        let proactivity = self.native_proactivity_state.refresh_gates(gates);
        let (permission_mode, main_loop_model) =
            (self.permission_mode(), self.tools.main_loop_model());
        let tools: Vec<String> = self
            .filtered_available_tools()
            .await
            .into_iter()
            .map(|tool| tool.name().to_owned())
            .collect();
        let attachments = self
            .transcript
            .model_reminder_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut facts = self
            .native_policy_host_facts_provider
            .as_ref()
            .map(|provider| provider())
            .unwrap_or_default();
        let model_capabilities = std::env::var(MODEL_CAPABILITIES_ENV).ok();
        let model_capability_gate_disabled = native_bash_first_model_capability_gate_disabled();
        facts.bash_first_assignment = facts.bash_first_assignment.or_else(|| {
            native_bash_first_assignment_for_model(
                main_loop_model.as_deref(),
                model_capabilities.as_deref(),
                model_capability_gate_disabled,
            )
        });
        facts.model_is_simple = facts.model_is_simple.or_else(|| {
            native_bash_first_model_is_simple(
                main_loop_model.as_deref(),
                model_capabilities.as_deref(),
                model_capability_gate_disabled,
            )
        });
        facts.attachments_enabled = Some(proactivity.attachments_enabled);
        facts.proactivity_selector_gate = Some(proactivity.selector_gate);
        facts.proactivity_level = Some(proactivity.level);
        let mut rows = Vec::with_capacity(3);
        let previous_proactivity = latest_valid_proactivity(prior_history, &attachments);
        if let Some(payload) = proactivity_payload(previous_proactivity.as_ref(), &facts, &tools) {
            let body = if payload.string_at("/status") == Some("active") {
                parse_proactivity_state(&payload)
                    .map(|state| render_proactivity_active(&state.level, &state))
                    .unwrap_or_else(render_proactivity_withdrawal)
            } else {
                render_proactivity_withdrawal()
            };
            if let Some(row) = model_attachment(payload, body) {
                rows.push(row);
            }
        }

        let previous_auto_mode = latest_auto_mode(prior_history, &attachments);
        let simple_system_prompt = facts
            .simple_system_prompt
            .unwrap_or_else(|| tool_api::dh_simple_system_prompt(main_loop_model.as_deref()));
        let auto_mode = auto_mode_payload(
            permission_mode.as_deref(),
            simple_system_prompt,
            &tools,
            previous_auto_mode,
            &facts,
        );
        if let Some(payload) = auto_mode {
            let body = render_auto_mode(&payload);
            if let Some(row) = model_attachment(payload, body) {
                rows.push(row);
            }
        }

        let has_agent_identity = facts
            .agent_id
            .as_deref()
            .is_some_and(|agent_id| !agent_id.is_empty());
        let exit_pending = if has_agent_identity {
            false
        } else {
            self.perms.take_native_auto_mode_exit_attachment_pending()
        };
        let active_auto_mode = self
            .perms
            .permission_policy_snapshot()
            .is_some_and(|snapshot| snapshot.active_auto_mode);
        if should_emit_auto_mode_exit(
            exit_pending,
            has_agent_identity,
            permission_mode.as_deref(),
            active_auto_mode,
            previous_auto_mode,
        ) {
            if let Some(payload) = auto_mode_exit_payload(previous_auto_mode) {
                let body = render_auto_mode_exit(&payload);
                if let Some(row) = model_attachment(payload, body) {
                    rows.push(row);
                }
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(source: &str) -> Source {
        Source::parse(source).unwrap()
    }

    fn attachment(message_id: MessageId, payload: &str) -> (ConversationMessage, Source) {
        let message = ConversationMessage::user_meta(message_id, "attachment".into());
        (message, json(payload))
    }

    #[test]
    fn auto_mode_requires_native_mode_tools_and_main_model_gates() {
        let tools = vec!["Bash".to_owned(), "Edit".to_owned()];
        let facts = NativePolicyHostFacts::default();
        assert!(auto_mode_payload(Some("default"), false, &tools, None, &facts).is_none());
        assert!(auto_mode_payload(Some("auto"), true, &["Edit".into()], None, &facts).is_none());
        assert!(auto_mode_payload(Some("auto"), true, &tools, None, &facts).is_none());

        let payload = auto_mode_payload(Some("auto"), false, &tools, None, &facts)
            .expect("full model gets the primary Auto Mode announcement");
        assert_eq!(payload.string_at("/type"), Some("auto_mode"));
        assert_eq!(
            payload.at_pointer("/steerOnly").unwrap().as_bool(),
            Some(false)
        );
        assert_eq!(
            payload.at_pointer("/bypass").unwrap().as_bool(),
            Some(false)
        );
        assert_eq!(
            payload
                .at_pointer("/autoModeConsentFlow")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert_eq!(
            payload.at_pointer("/bashFirst").unwrap().as_bool(),
            Some(false)
        );
        assert!(payload.at_pointer("/bashFirstSteer").is_none());
    }

    #[test]
    fn auto_mode_only_reannounces_the_native_steer_only_transition() {
        let tools = vec!["Bash".to_owned(), "Write".to_owned()];
        let facts = NativePolicyHostFacts {
            bash_first_assignment: Some(NativeBashFirstSessionAssignment::Forced),
            ..NativePolicyHostFacts::default()
        };
        let previous = json(
            r#"{"type":"auto_mode","steerOnly":true,"bashFirst":true,"bashFirstSteer":"relaxed"}"#,
        );
        assert!(auto_mode_payload(Some("auto"), true, &tools, Some(&previous), &facts).is_none());
        let changed = auto_mode_payload(Some("auto"), false, &tools, Some(&previous), &facts)
            .expect("leaving steer-only mode refreshes the full main-model guidance");
        assert_eq!(changed.string_at("/bashFirstSteer"), Some("relaxed"));
        assert_eq!(
            changed.at_pointer("/steerOnly").unwrap().as_bool(),
            Some(false)
        );
    }

    #[test]
    fn bash_first_uses_raw_environment_then_exact_host_assignment() {
        let tools = vec!["Bash".into(), "Edit".into()];
        let facts = NativePolicyHostFacts {
            bash_first_assignment: Some(NativeBashFirstSessionAssignment::None),
            ..NativePolicyHostFacts::default()
        };
        let raw = bash_first_value_with_environment(&tools, &facts, Some(Source::from("false")));
        assert_eq!(raw.string_at(""), Some("false"));
        assert!(truthy(&raw), "JavaScript treats the raw string as truthy");

        let forced = NativePolicyHostFacts {
            bash_first_assignment: Some(NativeBashFirstSessionAssignment::Forced),
            ..NativePolicyHostFacts::default()
        };
        assert!(truthy(&bash_first_value_with_environment(
            &tools, &forced, None
        )));

        let cohort = NativePolicyHostFacts {
            bash_first_assignment: Some(NativeBashFirstSessionAssignment::Cohort),
            bash_first_cohort_enabled: Some(true),
            ..NativePolicyHostFacts::default()
        };
        assert!(truthy(&bash_first_value_with_environment(
            &tools, &cohort, None
        )));
        assert!(!truthy(&bash_first_value_with_environment(
            &tools, &facts, None
        )));
        assert!(!truthy(&bash_first_value_with_environment(
            &["Bash".into()],
            &forced,
            Some(Source::from("true"))
        )));
    }

    #[test]
    fn bash_first_assignment_uses_current_model_catalog_and_capability_overrides() {
        use NativeBashFirstSessionAssignment::{Cohort, Forced, None as NoAssignment};

        let local_catalog_cases = [
            ("claude-fable-5-1", Forced),
            ("us.anthropic.claude-opus-5-5-20260728-v1:0", Forced),
            ("claude-opus-5", Cohort),
        ];
        for (model, expected) in local_catalog_cases {
            assert_eq!(
                native_bash_first_assignment_for_model(Some(model), None, false),
                Some(expected),
                "Native model assignment for {model}"
            );
        }
        assert_eq!(
            native_bash_first_assignment_for_model(Some("claude-sonnet-5-5"), None, false),
            None,
            "a model without a local predicate may still have a served client-data assignment"
        );
        assert_eq!(
            native_bash_first_assignment_for_model(None, None, false),
            None,
            "without a current prompt model the local catalog gives no fact"
        );

        assert_eq!(
            native_bash_first_assignment_for_model(
                Some("claude-fable-5-1"),
                Some("claude-fable-5-1=-fable_5_1_prompt_bundle"),
                false,
            ),
            None,
            "a local capability denial removes the baked force, but client data remains unknown"
        );
        assert_eq!(
            native_bash_first_assignment_for_model(
                Some("claude-fable-5-1"),
                Some("claude-fable-5-1=-thrifty_sonic"),
                false,
            ),
            Some(NoAssignment),
            "Native's explicit thrifty_sonic=false exits before forced model gates"
        );
        assert_eq!(
            native_bash_first_assignment_for_model(
                Some("claude-sonnet-5-5"),
                Some("claude-sonnet-5-5=thrifty_sonic"),
                false,
            ),
            Some(Forced),
            "a Native capability override can force Bash-first for another model"
        );
        assert_eq!(
            native_bash_first_assignment_for_model(Some("claude-fable-5-1"), None, true),
            None,
            "Native's remote-cowork/local-agent context gate suppresses catalog forcing"
        );
        assert_eq!(
            native_bash_first_assignment_for_model(Some("claude-opus-5"), None, true),
            Some(Cohort),
            "the Opus 5 cohort predicate follows the two forced-capability gates"
        );

        let cohort_disabled = NativePolicyHostFacts {
            bash_first_assignment: Some(Cohort),
            bash_first_cohort_enabled: Some(false),
            ..NativePolicyHostFacts::default()
        };
        assert!(!truthy(&bash_first_value_with_environment(
            &["Bash".into(), "Edit".into()],
            &cohort_disabled,
            None,
        )));
    }

    #[test]
    fn bash_first_steer_simple_model_uses_the_opus_55_bundle_capability() {
        assert_eq!(
            native_bash_first_model_is_simple(Some("claude-opus-5-5"), None, false),
            Some(true)
        );
        assert_eq!(
            native_bash_first_model_is_simple(
                Some("claude-opus-5-5"),
                Some("claude-opus-5-5=-opus_5_5_prompt_bundle"),
                false,
            ),
            Some(false),
            "Native model capability overrides can disable the relaxed steer fallback"
        );
        assert_eq!(
            native_bash_first_model_is_simple(Some("claude-opus-5"), None, false),
            Some(false),
            "the separate Opus 5 cohort model is not the Opus 5.5 steer variant"
        );
        assert_eq!(
            native_bash_first_model_is_simple(Some("claude-opus-5-5"), None, true),
            Some(false)
        );
    }

    #[test]
    fn bash_first_model_capability_context_gate_matches_native_entrypoints() {
        for entrypoint in [
            "local-agent",
            "local_agent",
            "remote_cowork",
            "remote_cowork_trigger",
        ] {
            assert!(native_bash_first_model_capability_gate_disabled_for(
                Some(entrypoint),
                false
            ));
            assert!(
                !native_bash_first_model_capability_gate_disabled_for(Some(entrypoint), true),
                "Native's entrypoint gate does not apply once the host marks this as a child session"
            );
        }
        assert!(!native_bash_first_model_capability_gate_disabled_for(
            Some("cli"),
            false
        ));
        assert!(!native_bash_first_model_capability_gate_disabled_for(
            None, false
        ));
    }

    #[test]
    fn prior_bash_first_steer_null_uses_native_strict_default() {
        let prior = json(r#"{"type":"auto_mode","bashFirstSteer":null}"#);
        let facts = NativePolicyHostFacts {
            bash_first_steer_variant: Some("relaxed".into()),
            ..NativePolicyHostFacts::default()
        };
        assert_eq!(
            bash_first_steer_value(Some(&prior), &facts).string_at(""),
            Some("strict")
        );
    }

    #[test]
    fn auto_mode_renderer_keeps_native_title_and_ask_user_question_name() {
        let tools = vec!["Bash".into(), "Edit".into()];
        let payload = auto_mode_payload(
            Some("auto"),
            false,
            &tools,
            None,
            &NativePolicyHostFacts::default(),
        )
        .unwrap();
        let body = render_auto_mode(&payload);
        assert!(body.starts_with("## Auto Mode Active\n"));
        assert!(body.contains("AskUserQuestion or otherwise"));
        assert!(!body.contains("with ask or otherwise"));
    }

    #[test]
    fn proactivity_withdrawal_requires_the_latest_native_valid_active_state() {
        let active_id = MessageId::new();
        let active = attachment(
            active_id,
            r#"{"type":"proactivity","status":"active","level":"proactive","tools":{"askUserQuestion":true,"enterPlanMode":false,"scheduleWakeup":true,"cronCreate":false}}"#,
        );
        let attachments = HashMap::from([(active_id, active.1)]);
        let history = vec![active.0];
        let state = latest_valid_proactivity(&history, &attachments).unwrap();
        let withdrawal =
            proactivity_payload(Some(&state), &NativePolicyHostFacts::default(), &[]).unwrap();
        assert_eq!(withdrawal.string_at("/type"), Some("proactivity"));
        assert_eq!(withdrawal.string_at("/status"), Some("withdrawn"));
        assert_eq!(render_proactivity_withdrawal(), PROACTIVITY_WITHDRAWN_TEXT);

        let invalid_id = MessageId::new();
        let invalid = attachment(
            invalid_id,
            r#"{"type":"proactivity","status":"active","level":"aggressive","tools":{"askUserQuestion":true,"enterPlanMode":false,"scheduleWakeup":true,"cronCreate":false}}"#,
        );
        let attachments = HashMap::from([
            (active_id, attachments[&active_id].clone()),
            (invalid_id, invalid.1),
        ]);
        assert!(latest_valid_proactivity(&[history[0].clone(), invalid.0], &attachments).is_some());

        let withdrawn_id = MessageId::new();
        let withdrawn = attachment(
            withdrawn_id,
            r#"{"type":"proactivity","status":"withdrawn"}"#,
        );
        let attachments = HashMap::from([
            (active_id, attachments[&active_id].clone()),
            (withdrawn_id, withdrawn.1),
        ]);
        assert!(
            latest_valid_proactivity(&[history[0].clone(), withdrawn.0], &attachments).is_none()
        );
        assert!(proactivity_payload(None, &NativePolicyHostFacts::default(), &[]).is_none());
    }

    #[test]
    fn proactivity_emits_current_typed_tools_and_native_rendering_when_gate_is_enabled() {
        let facts = NativePolicyHostFacts {
            proactivity_selector_gate: Some(true),
            proactivity_level: Some(NativeProactivityLevel::Proactive),
            ..NativePolicyHostFacts::default()
        };
        let tools = vec!["AskUserQuestion".into(), "ScheduleWakeup".into()];
        let payload = proactivity_payload(None, &facts, &tools).unwrap();
        assert_eq!(payload.string_at("/type"), Some("proactivity"));
        assert_eq!(payload.string_at("/status"), Some("active"));
        assert_eq!(payload.string_at("/level"), Some("proactive"));
        assert_eq!(
            payload
                .at_pointer("/tools/askUserQuestion")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            payload
                .at_pointer("/tools/enterPlanMode")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert_eq!(
            payload
                .at_pointer("/tools/scheduleWakeup")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            payload.at_pointer("/tools/cronCreate").unwrap().as_bool(),
            Some(false)
        );
        let state = parse_proactivity_state(&payload).unwrap();
        let body = render_proactivity_active("proactive", &state);
        assert!(body.starts_with("## Proactivity: high\n"));
        assert!(body.contains("Do not ask questions"));
        assert!(body.contains(
            "schedule a check-in with ScheduleWakeup — or a routine via the /schedule skill when it is available and the follow-up should outlive this session — and pick the work back up when it fires."
        ));
        assert!(proactivity_payload(Some(&state), &facts, &tools).is_none());

        let default_state = ProactivityState {
            level: "default".to_owned(),
            ..state.clone()
        };
        let default_body = render_proactivity_active("default", &default_state);
        assert!(default_body.starts_with("## Proactivity: medium\n"));
        assert!(default_body.contains("Use AskUserQuestion only when"));

        let disabled = NativePolicyHostFacts::default();
        let withdrawal = proactivity_payload(Some(&state), &disabled, &tools).unwrap();
        assert_eq!(withdrawal.string_at("/status"), Some("withdrawn"));
    }

    #[test]
    fn auto_mode_exit_preserves_prior_field_presence_and_renders_native_variants() {
        let prior = json(r#"{"type":"auto_mode","bashFirst":null,"steerOnly":false}"#);
        let exit = auto_mode_exit_payload(Some(&prior)).unwrap();
        assert_eq!(exit.string_at("/type"), Some("auto_mode_exit"));
        assert!(exit.at_pointer("/bashFirst").unwrap().is_null());
        assert_eq!(
            exit.at_pointer("/steerOnly").unwrap().as_bool(),
            Some(false)
        );
        assert!(render_auto_mode_exit(&exit).contains("interact more directly"));
        assert!(!render_auto_mode_exit(&exit).contains("Resume using"));

        let prior = json(r#"{"type":"auto_mode","bashFirst":true,"steerOnly":true}"#);
        let exit = auto_mode_exit_payload(Some(&prior)).unwrap();
        assert!(render_auto_mode_exit(&exit)
            .starts_with("## Exited Auto Mode\n\nYou have exited auto mode."));
        assert!(render_auto_mode_exit(&exit)
            .ends_with(" Resume using the dedicated tools for file reads, searches, and edits."));
    }

    #[test]
    fn managed_plan_kill_emits_auto_exit_only_after_native_auto_is_inactive() {
        let previous = json(r#"{"type":"auto_mode","bashFirst":true,"steerOnly":false}"#);
        assert!(should_emit_auto_mode_exit(
            true,
            false,
            Some("plan"),
            false,
            Some(&previous),
        ));
        assert!(auto_mode_exit_payload(Some(&previous)).is_some());

        assert!(
            !should_emit_auto_mode_exit(true, false, Some("plan"), true, Some(&previous)),
            "Native ef() suppresses the row while managed pre-plan Auto remains active"
        );
        assert!(
            !should_emit_auto_mode_exit(true, false, Some("auto"), false, Some(&previous)),
            "Native requires the visible permission mode to have left Auto"
        );
        assert!(
            !should_emit_auto_mode_exit(true, false, Some("plan"), false, None),
            "Native dPr requires a previous valid auto_mode row"
        );
        assert!(
            !should_emit_auto_mode_exit(true, true, Some("plan"), false, Some(&previous)),
            "agent-scoped sessions do not receive the main-model exit row"
        );
    }

    #[test]
    fn javascript_truthiness_preserves_raw_auto_mode_environment_values() {
        assert!(truthy(&Source::from("false")));
        assert!(truthy(&Source::from("0")));
        assert!(!truthy(&Source::from("")));
        assert!(truthy(&json("[]")));
        assert!(truthy(&json("{}")));
        assert!(!truthy(&source_bool(false)));
    }

    #[test]
    fn proactivity_initialize_and_feature_resampling_keep_selector_state_exact() {
        let state = NativeProactivityState::default();
        let updates = state.subscribe();
        let eligible = NativeProactivityGates {
            attachments_enabled: true,
            selector_feature_enabled: true,
            disabled_by_environment: false,
        };
        assert!(!state.refresh_gates(eligible).selector_gate);
        assert!(!state.apply_initialize_opt_in(None, false).selector_gate);
        assert!(
            state
                .apply_initialize_opt_in(Some(true), false)
                .selector_gate
        );
        assert!(updates.has_changed().unwrap());
        let ever_enabled = state.snapshot();
        assert!(ever_enabled.selector_gate_ever_on);
        let levelled = state.commit_level(NativeProactivityLevel::Proactive, "acceptEdits");
        assert_eq!(levelled.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            levelled.permission_baseline_mode.as_deref(),
            Some("acceptEdits")
        );
        let metadata_before_reinitialize = state.session_metadata();

        let disabled_feature = NativeProactivityGates {
            selector_feature_enabled: false,
            ..eligible
        };
        let current = state.refresh_gates(disabled_feature);
        assert!(!current.selector_gate);
        assert!(current.selector_gate_ever_on);

        // Native hC returns before s_/sp on a false reinitialize.
        let unchanged = state.apply_initialize_opt_in(Some(false), true);
        assert!(unchanged.opted_in);
        assert!(!unchanged.selector_gate);
        assert_eq!(unchanged.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            unchanged.permission_baseline_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(state.session_metadata(), metadata_before_reinitialize);

        let opted_out = state.apply_initialize_opt_in(Some(false), false);
        assert!(!opted_out.opted_in);
        assert!(opted_out.selector_gate_ever_on);
        assert_eq!(opted_out.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            opted_out.permission_baseline_mode.as_deref(),
            Some("acceptEdits"),
            "Native hC opt-out clears selector metadata without resetting appState level or mode baseline"
        );
        let metadata = state.session_metadata();
        assert_eq!(
            metadata
                .internal
                .as_ref()
                .and_then(|value| value.at_pointer("/proactivity_opt_in"))
                .map(|value| value.is_null()),
            Some(true)
        );
        assert_eq!(
            metadata
                .external
                .as_ref()
                .and_then(|value| value.at_pointer("/proactivity"))
                .map(|value| value.is_null()),
            Some(true)
        );

        state.refresh_gates(eligible);
        let reenabled = state.apply_initialize_opt_in(Some(true), false);
        assert!(reenabled.selector_gate);
        assert_eq!(reenabled.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            reenabled.permission_baseline_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(
            state
                .session_metadata()
                .external
                .as_ref()
                .and_then(|value| value.string_at("/proactivity/level")),
            Some("proactive")
        );
    }

    #[tokio::test]
    async fn initialize_opt_out_preserves_same_session_level_and_permission_mode() {
        use crate::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use lingxi_core::host::PermissionGate;
        use std::sync::Arc;

        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(
                permission::PermissionPolicy::from_rules(
                    permission::PermissionMode::Default,
                    Vec::new(),
                )
                .with_bypass_available(true),
            ),
            Arc::new(NoOpPermissionGate),
        ));
        gate.set_permission_mode("acceptEdits")
            .await
            .expect("set the ordinary baseline mode");
        let orch = ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            gate,
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );

        orch.apply_native_proactivity_initialize(Some(true), false);
        let response = orch
            .set_native_proactivity_level("proactive", None)
            .await
            .expect("set the active Native level");
        assert_eq!(response.permission_mode, "bypassPermissions");
        let before_opt_out = orch.current_native_proactivity_snapshot();
        let permission_before_opt_out = orch.permission_mode();
        assert_eq!(before_opt_out.level, NativeProactivityLevel::Proactive);

        let opted_out = orch.apply_native_proactivity_initialize(Some(false), false);
        assert!(!opted_out.opted_in);
        assert_eq!(opted_out.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            opted_out.permission_baseline_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(orch.permission_mode(), permission_before_opt_out);

        let reenabled = orch.apply_native_proactivity_initialize(Some(true), false);
        assert!(reenabled.opted_in);
        assert_eq!(reenabled.level, NativeProactivityLevel::Proactive);
        assert_eq!(
            reenabled.permission_baseline_mode.as_deref(),
            Some("acceptEdits")
        );
        assert_eq!(orch.permission_mode(), permission_before_opt_out);
    }

    #[test]
    fn worker_state_restore_requires_strict_internal_true_for_external_level() {
        let gates = NativeProactivityGates {
            attachments_enabled: true,
            selector_feature_enabled: true,
            disabled_by_environment: false,
        };
        let external = Source::parse(r#"{"proactivity":{"level":"proactive"}}"#).unwrap();
        for internal in [
            None,
            Some(Source::parse(r#"{"proactivity_opt_in":null}"#).unwrap()),
            Some(Source::parse(r#"{"proactivity_opt_in":false}"#).unwrap()),
            Some(Source::parse(r#"{"proactivity_opt_in":"true"}"#).unwrap()),
        ] {
            let state = NativeProactivityState::default();
            let restored = state.restore_session_metadata(
                lingxi_core::host::NativeProactivitySessionMetadata {
                    internal,
                    external: Some(external.clone()),
                },
                gates,
            );
            assert!(!restored.opted_in);
            assert!(!restored.selector_gate);
            assert_eq!(restored.level, NativeProactivityLevel::Default);
        }

        let state = NativeProactivityState::default();
        let restored = state.restore_session_metadata(
            lingxi_core::host::NativeProactivitySessionMetadata {
                internal: Some(Source::parse(r#"{"proactivity_opt_in":true}"#).unwrap()),
                external: Some(external),
            },
            gates,
        );
        assert!(restored.opted_in);
        assert!(restored.selector_gate);
        assert!(restored.selector_gate_ever_on);
        assert_eq!(restored.level, NativeProactivityLevel::Proactive);
    }

    #[test]
    fn proactivity_level_changes_require_live_selector_and_attachment_gates() {
        let state = NativeProactivityState::default();
        assert!(state.validate_level_change().is_err());
        let gates = NativeProactivityGates {
            attachments_enabled: true,
            selector_feature_enabled: true,
            disabled_by_environment: false,
        };
        state.refresh_gates(gates);
        state.apply_initialize_opt_in(Some(true), false);
        state.validate_level_change().unwrap();
        let changed = state.commit_level(NativeProactivityLevel::Proactive, "default");
        assert_eq!(changed.level, NativeProactivityLevel::Proactive);

        let excluded = state.refresh_gates(NativeProactivityGates {
            attachments_enabled: false,
            ..gates
        });
        assert!(excluded.selector_gate);
        assert!(state.validate_level_change().is_err());
    }

    #[test]
    fn native_attachment_env_gate_uses_javascript_string_truthiness() {
        assert!(native_js_env_truthy(Some(OsStr::new("false"))));
        assert!(native_js_env_truthy(Some(OsStr::new("0"))));
        assert!(!native_js_env_truthy(Some(OsStr::new(""))));
        assert!(!native_js_env_truthy(None));
    }
}
