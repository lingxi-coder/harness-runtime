//! `PolicyPermissionGate` — the enforcing gate (permission enforcement phase 2).
//!
//! Bridges the rule engine ([`PermissionPolicy::authorize`], 3-valued
//! Allow/Deny/Ask) onto the 2-valued [`PermissionGate::check`] contract the
//! orchestrator consults at its single dispatch chokepoint. It wraps a loaded
//! [`PermissionPolicy`] plus an INNER [`PermissionGate`] (the existing prompting
//! transport — `InteractivePromptingGate` / `TuiPermissionGate` /
//! `AdapterPermissionGate` / a no-op), and maps:
//!
//! - `authorize` → `Allow`  ⇒ [`PermissionDecision::Allow`].
//! - `authorize` → `Deny`   ⇒ [`PermissionDecision::Deny`] (reason rendered).
//! - `authorize` → `Ask`    ⇒ either AUTO-ALLOW when the tool is read-only /
//!   agent-local ([`tool_default`] is [`PromptDefault::AllowByDefault`]) — a
//!   phase-2 stand-in for claude-code's per-tool default-allow that avoids an
//!   ask-storm on every `Read`/`Glob`/etc. — or DELEGATE to `inner.check(...)`
//!   so the prompt surfaces through whatever transport the host wired.
//!
//! ## Scope
//! `authorize` matches TOOL-WIDE rules + mode for all tools, and per-tool FILE
//! PATH content matching for file tools (phase 3a — `Edit(src/**)` /
//! `Read(./secrets/**)`). `Bash`/`WebFetch` content matching (e.g.
//! `Bash(npm run *)`) is still tool-wide (the 3a-bash deferral). The subagent
//! path ([`RegistryToolInvoker`]) is now ALSO gated (phase 3b — closed the
//! bypass; subagent + teammate tool calls consult this same gate).
//!
//! ## `Plan` mode (Batch 3)
//! `Plan` mode now imposes a mutation backstop in
//! [`PermissionPolicy::authorize`]: a tool that is NOT on the read-only /
//! planning-safe allowlist ([`crate::mode_policy::is_plan_safe_tool`]) and that
//! matched no allow rule returns `Ask` tagged with `Plan`. That ask is a
//! `DenyByDefault` outcome for mutating tools (`Edit`/`Write`/`Bash`/…), so it
//! is DELEGATED to the inner prompt transport here — it is NOT short-circuited
//! to auto-allow. Plan-safe READ-ONLY tools (`Read`/`Grep`/`Glob`/`LSP`/…) do
//! NOT trip the backstop; they fall through to the generic mode-fallback ask
//! and, being [`PromptDefault::AllowByDefault`], are AUTO-ALLOWED here without a
//! prompt — so plan mode keeps read access frictionless while still gating
//! mutations. (`AcceptEdits`' edit auto-allow is a separate batch.) This gate
//! is built at boot only behind an OPT-IN toggle; the default remains the
//! always-allow `NoOpPermissionGate`.

use crate::classifier::{reason_allows_classifier, AutoModeClassifierVerdict};
use crate::defaults_per_tool::tool_default;
use crate::gate::{
    AutoModePrompt, HandoffReview, MatchedAskRule, PermissionAbort, PermissionCheckContext,
    PermissionDecision, PermissionDecisionSource, PermissionGate, PermissionOutcome,
    PermissionResolution, PromptDefault,
};
use crate::headless_gate::headless_deny_message;
use crate::layers::{
    apply_context_layers, fold_permission_layers, parse_permission_layers, FoldedPermissionContext,
    LayerFoldInputs, PermissionLayer,
};
use crate::mode::PermissionMode;
use crate::policy::PermissionPolicy;
use crate::result::{
    ClassifierKind, PermissionDecisionReason, PermissionMetadata, PermissionResult,
};
use crate::rule::{PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue};
use async_trait::async_trait;
use platform_api::permission_gate::PermissionRequestSource;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Live model/provider inputs read by the auto-mode permission gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveModelContext {
    /// Canonical model id (`wi()` / `getModel()`).
    pub model: String,
    /// Claude provider tag (`"firstParty"`, `"anthropicAws"`, or another
    /// non-first-party tag) used by `dUe`.
    pub provider: String,
}

/// A source of the LIVE main-loop model and provider, read by the auto
/// `set_permission_mode` gate to evaluate `One()` against the route the session
/// is CURRENTLY on. Mirrors `agent::DefaultModelProvider`: returns `None` when
/// the value cannot be read without blocking (a contended session lock), so the
/// caller fails OPEN rather than stall the control request.
pub type LiveModelProvider = Arc<dyn Fn() -> Option<LiveModelContext> + Send + Sync>;

/// Result of the optional auto-mode classifier pass. The interactive breaker
/// fallback carries its rewritten reason forward to the prompt transport;
/// keeping it in this value avoids a shared mutable side-channel.
enum AutoModeClassifierResult {
    NoDecision,
    Classified(PermissionResult),
    PromptFallback { decision_reason: String },
}

#[derive(Debug, Clone)]
struct LivePermissionState {
    allow_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    deny_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    ask_rules: HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    additional_working_dirs: crate::working_dirs::AdditionalWorkingDirs,
}

fn directory_update_null_byte_reason(update_type: &str, directory: &str) -> String {
    format!("{update_type} carries a directory containing a null byte: {directory}")
}

impl LivePermissionState {
    fn from_policy(policy: &PermissionPolicy) -> Self {
        Self {
            allow_rules: policy.allow_rules.clone(),
            deny_rules: policy.deny_rules.clone(),
            ask_rules: policy.ask_rules.clone(),
            additional_working_dirs: policy.additional_working_dirs.clone(),
        }
    }
}

/// Enforcing gate over a [`PermissionPolicy`] (see module docs).
pub struct PolicyPermissionGate {
    /// The loaded rule policy. `authorize` reads its allow/deny/ask buckets.
    policy: Arc<PermissionPolicy>,
    /// Prompt transport consulted only when `authorize` returns `Ask` for a
    /// non-read-only tool. Reusing an existing `PermissionGate` keeps all the
    /// parking/oneshot machinery in one place.
    inner: Arc<dyn PermissionGate>,
    /// LIVE mode override set by [`PermissionGate::set_permission_mode`]
    /// (the `set_permission_mode` control_request). `None` ⇒ use the policy's
    /// boot mode (`authorize`); `Some(mode)` ⇒ authorize under that mode on every
    /// check, mirroring claude-code reading `toolPermissionContext.mode` LIVE.
    /// Read briefly per check (the value is `Copy`, never held across an `await`).
    mode_override: std::sync::RwLock<Option<PermissionMode>>,
    /// An explicit acknowledgement from an interactive host for this session.
    /// This permits a live transition into `bypassPermissions` without making
    /// the mode available to unrelated sessions or weakening the settings
    /// killswitch.
    bypass_permissions_confirmed: std::sync::atomic::AtomicBool,
    /// LIVE tighten-only per-MCP-server permission-mode overrides from the
    /// control channel (`set_mcp_permission_mode_override`). Keys are the
    /// normalized MCP server token used in `mcp__<server>__<tool>` names.
    mcp_mode_overrides: std::sync::RwLock<std::collections::HashMap<String, PermissionMode>>,
    /// LIVE main-loop model source (claude-code `wi()`), filled post-orchestrator
    /// via [`Self::live_model_provider_handle`] so the auto `set_permission_mode`
    /// gate can evaluate `One()`'s model reason (`dUe(wi())`) against the model
    /// the session is CURRENTLY on (mutated by `/model` switches / resume), not
    /// the boot snapshot. Empty ⇒ the live model gate is skipped (the boot gate
    /// already downgraded an auto-unsupported boot model, so a fresh session
    /// never reaches this path in `Auto`); a contended read returns `None` and
    /// likewise skips (fail-open, matching every other post-orch live cell).
    live_model_provider: Arc<std::sync::OnceLock<LiveModelProvider>>,
    loop_classifier: Arc<std::sync::OnceLock<Arc<dyn crate::classifier::LoopPermissionClassifier>>>,
    /// Live session rule/directory state after applying host
    /// `updatedPermissions`. Starts as a clone of the boot policy's mutable
    /// permission state, then diverges only through
    /// [`Self::apply_permission_update`].
    live_state: std::sync::RwLock<LivePermissionState>,
    /// LIVE administrator-managed `disableAutoMode` kill switch.  The boot
    /// policy remains an immutable snapshot for rule evaluation; this atomic
    /// is the narrow settings-reload seam used by the desktop watcher.
    auto_mode_disabled_from_settings: std::sync::atomic::AtomicBool,
    /// MOBILE DIVERGENCE (guest/host coordinate split). `None` on every
    /// non-mobile composition root, which makes the whole feature unreachable
    /// from engine-desktop / tui / cli / bridge-server.
    path_translator: Option<Arc<dyn crate::model_path::ModelPathTranslator>>,
    /// Deny specs contributed by the slash command / skill behind the CURRENT
    /// user input — upstream `alwaysDenyRules.command`, written only by `Tbt`
    /// ([`PermissionGate::set_command_input_denies`]).
    ///
    /// Kept in its OWN cell rather than written into `live_state.deny_rules`
    /// under [`PermissionRuleSource::Command`], because a hook's
    /// `permissionUpdates` can also name that source in this port and the
    /// replace-per-input semantics would silently delete what the hook added.
    /// Folded into the deny bucket at evaluation time instead, which keeps both
    /// producers and gives this one the lifetime it is supposed to have.
    command_input_denies: std::sync::RwLock<Vec<String>>,
}

impl PolicyPermissionGate {
    async fn check_prompt_transport(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        // 2.1.263 Ge: only the actual prompt wait counts; policy/classifier
        // work happens before this boundary. Drop also covers cancellation.
        let _pause = ctx.pause_observer.as_ref().map(|observer| observer.begin());
        self.inner.check_with_context(name, input, ctx).await
    }

    fn parse_update_destination(value: &Value) -> Option<PermissionRuleSource> {
        match value.as_str()? {
            "userSettings" => Some(PermissionRuleSource::Settings(
                protocol::SettingsScope::User,
            )),
            "projectSettings" => Some(PermissionRuleSource::Settings(
                protocol::SettingsScope::Project,
            )),
            "localSettings" => Some(PermissionRuleSource::Settings(
                protocol::SettingsScope::Local,
            )),
            "cliArg" => Some(PermissionRuleSource::CliArg),
            "session" => Some(PermissionRuleSource::Session),
            _ => None,
        }
    }

    fn parse_update_behavior(value: &Value) -> Option<PermissionBehavior> {
        match value.as_str()? {
            "allow" => Some(PermissionBehavior::Allow),
            "deny" => Some(PermissionBehavior::Deny),
            "ask" => Some(PermissionBehavior::Ask),
            _ => None,
        }
    }

    fn parse_update_rules(
        value: &Value,
        behavior: PermissionBehavior,
        source: PermissionRuleSource,
    ) -> Option<Vec<PermissionRule>> {
        value
            .as_array()?
            .iter()
            .map(|rule| {
                let object = rule.as_object()?;
                let tool_name = object.get("toolName").and_then(Value::as_str)?;
                let rule_content = match object.get("ruleContent") {
                    None => None,
                    Some(Value::String(content)) => Some(content.clone()),
                    Some(_) => return None,
                };
                Some(PermissionRule {
                    value: PermissionRuleValue {
                        tool_name: crate::rule::normalize_legacy_tool_name(tool_name),
                        rule_content,
                    },
                    behavior,
                    source,
                })
            })
            .collect()
    }

    fn rule_bucket_mut(
        live: &mut LivePermissionState,
        behavior: PermissionBehavior,
    ) -> &mut HashMap<PermissionRuleSource, Vec<PermissionRule>> {
        match behavior {
            PermissionBehavior::Allow => &mut live.allow_rules,
            PermissionBehavior::Deny => &mut live.deny_rules,
            PermissionBehavior::Ask => &mut live.ask_rules,
        }
    }

    /// Wrap `policy` with `inner` as the `Ask`-delegation prompt transport.
    #[must_use]
    pub fn new(policy: Arc<PermissionPolicy>, inner: Arc<dyn PermissionGate>) -> Self {
        inner.set_permission_persistence_enabled(!policy.allow_managed_permission_rules_only);
        let auto_mode_disabled_from_settings = policy.auto_mode_disabled;
        Self {
            live_state: std::sync::RwLock::new(LivePermissionState::from_policy(&policy)),
            bypass_permissions_confirmed: std::sync::atomic::AtomicBool::new(
                policy.bypass_permissions_available,
            ),
            policy,
            inner,
            mode_override: std::sync::RwLock::new(None),
            mcp_mode_overrides: std::sync::RwLock::new(std::collections::HashMap::new()),
            live_model_provider: Arc::new(std::sync::OnceLock::new()),
            loop_classifier: Arc::new(std::sync::OnceLock::new()),
            path_translator: None,
            command_input_denies: std::sync::RwLock::new(Vec::new()),
            auto_mode_disabled_from_settings: std::sync::atomic::AtomicBool::new(
                auto_mode_disabled_from_settings,
            ),
        }
    }

    /// Fill after constructing the session so the classifier can use weak ownership.
    pub fn loop_classifier_handle(
        &self,
    ) -> Arc<std::sync::OnceLock<Arc<dyn crate::classifier::LoopPermissionClassifier>>> {
        self.loop_classifier.clone()
    }

    /// Install the guest→host translator (mobile only). Without it the gate is
    /// byte-identical to the desktop behavior.
    #[must_use]
    pub fn with_path_translator(
        mut self,
        translator: Arc<dyn crate::model_path::ModelPathTranslator>,
    ) -> Self {
        self.path_translator = Some(translator);
        self
    }

    /// Restore a mode recorded by the current session's own transcript.
    ///
    /// A restored `bypassPermissions` mode represents a prior explicit choice
    /// in that same session, so it is not subject to the one-shot live-entry
    /// acknowledgement. Administrative bypass disablement remains absolute.
    pub async fn restore_session_permission_mode(&self, mode: &str) -> Result<(), String> {
        if parse_settable_mode(mode) != Some(PermissionMode::BypassPermissions) {
            return PermissionGate::set_permission_mode(self, mode).await;
        }
        if self.policy.restricted {
            return Err("bypassPermissions not supported in restricted mode".to_string());
        }
        if self.policy.bypass_killswitch_active {
            return Err(
                "Cannot set permission mode to bypassPermissions because it is disabled by settings or configuration"
                    .to_string(),
            );
        }
        *self
            .mode_override
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Some(PermissionMode::BypassPermissions);
        Ok(())
    }

    /// Handle to the set-once LIVE-model cell (cycle-break): the composition root
    /// grabs this BEFORE coercing the gate to `Arc<dyn PermissionGate>`, then
    /// fills it once the orchestrator (owner of the live `session.model`) exists,
    /// so the auto `set_permission_mode` gate reads the CURRENT model. Same
    /// pattern as `agent::OrchestratorHandle::default_model_provider_handle`.
    #[must_use]
    pub fn live_model_provider_handle(&self) -> Arc<std::sync::OnceLock<LiveModelProvider>> {
        Arc::clone(&self.live_model_provider)
    }

    /// Apply the effective managed-settings `disableAutoMode` value to the
    /// current session.
    ///
    /// Tightening is ordered as one kill-switch update followed by one mode
    /// write: a concurrent `set_permission_mode("auto")` either observes the
    /// new kill switch and rejects, or wins the mode lock first and is then
    /// evicted here.  Clearing the setting deliberately does not restore Auto
    /// (nor any per-server Auto pin), matching the administrator's current
    /// state without silently changing a user's mode choice.
    pub fn update_auto_mode_disabled_from_settings(&self, disabled: bool) {
        let was_disabled = self
            .auto_mode_disabled_from_settings
            .swap(disabled, std::sync::atomic::Ordering::AcqRel);

        if disabled {
            let mut mode = self
                .mode_override
                .write()
                .unwrap_or_else(|error| error.into_inner());
            let active_mode = mode.unwrap_or(self.policy.mode);
            if active_mode == PermissionMode::Auto {
                *mode = Some(PermissionMode::Default);
                // There is no reliable transport-neutral system-message
                // channel on PermissionGate. Keep the oracle's exact reason
                // in the permission log for hosts to surface.
                tracing::warn!(target: "permission", "auto mode disabled by settings");
            }
            drop(mode);

            // A per-server Auto pin must not survive an administrative
            // disable. Removing it is safe because an active session-wide
            // Auto mode was downgraded above; after a later re-enable the
            // session remains Default and cannot re-enter Auto implicitly.
            self.mcp_mode_overrides
                .write()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|_, override_mode| *override_mode != PermissionMode::Auto);
        } else {
            // Keep this branch explicit: availability may reopen for a future
            // user request, but the gate never promotes the current mode.
            tracing::debug!(
                target: "permission",
                was_disabled,
                "auto mode settings kill switch cleared; current mode unchanged"
            );
        }
    }

    /// Short alias for callers that already have the concrete policy gate.
    pub fn update_auto_mode_disabled(&self, disabled: bool) {
        self.update_auto_mode_disabled_from_settings(disabled);
    }

    /// `dUe(wi())` at the LIVE `set_permission_mode` surface: does the model the
    /// session is CURRENTLY on FAIL the auto-mode model gate? Reads the live-model
    /// provider ([`Self::live_model_provider_handle`]) and returns `false`
    /// (fail-open, no rejection) when the provider is unset or the model cannot be
    /// read without blocking.
    fn live_model_unsupported_for_auto(&self) -> bool {
        let Some(provider) = self.live_model_provider.get() else {
            return false;
        };
        let Some(context) = provider() else {
            return false;
        };
        !crate::auto_gate::model_supports_auto_mode(&context.model, &context.provider)
    }

    /// `One()` — the live reason auto mode is unavailable, in the oracle's
    /// user-facing precedence. Shared by the session-wide mode switch and the
    /// per-MCP-server auto pin so neither control surface can bypass the other.
    fn auto_mode_denial_reason(&self) -> Option<crate::auto_gate::AutoGateDenialReason> {
        if self
            .auto_mode_disabled_from_settings
            .load(std::sync::atomic::Ordering::Acquire)
        {
            Some(crate::auto_gate::AutoGateDenialReason::Settings)
        } else if self
            .policy
            .denial_tracking
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_circuit_broken()
        {
            Some(crate::auto_gate::AutoGateDenialReason::CircuitBreaker)
        } else if self.live_model_unsupported_for_auto() {
            Some(crate::auto_gate::AutoGateDenialReason::Model)
        } else {
            None
        }
    }

    fn auto_prompt_for_ask(
        &self,
        mode: PermissionMode,
        name: &str,
        input: &Value,
        reason: &PermissionDecisionReason,
        ctx: &PermissionCheckContext,
    ) -> Option<AutoModePrompt> {
        if !self.can_request_auto_mode()
            || ctx.hook_ask_floor
            || ctx.suppress_always_allow_rule
            || ctx.matched_ask_rule.is_some()
            || ctx.request_source != Some(PermissionRequestSource::WorkflowAgent)
            || ctx.classifier_approvable != Some(true)
            || !matches!(mode, PermissionMode::Default | PermissionMode::AcceptEdits)
            || name != "Bash"
            || !input.is_object()
        {
            return None;
        }
        let command = input.get("command").and_then(Value::as_str)?;
        let command_lower = command.to_ascii_lowercase();
        if command_lower.contains("powershell") || command_lower.contains("pwsh") {
            return None;
        }
        // Keep the reason argument in the eligibility seam so future policy
        // producers cannot accidentally bypass their classifier provenance.
        matches!(classifier_approvable(reason), Some(true)).then_some(AutoModePrompt::WorkflowBash)
    }

    /// Compute the engine-owned Auto action for an ExitPlanMode approval.
    ///
    /// ExitPlanMode is a tool-owned approval flow rather than a policy `Ask`,
    /// so it never reaches [`Self::auto_prompt_for_ask`].  Keep the same
    /// fail-closed availability inputs here and explicitly reject contexts
    /// that cannot safely transition the owning session.
    fn auto_prompt_for_exit_plan(&self, ctx: &PermissionCheckContext) -> Option<AutoModePrompt> {
        if !self.can_request_auto_mode()
            || self.policy.restricted
            || ctx.is_agent_context
            || ctx.is_non_interactive_session
            || ctx.suppress_always_allow_rule
            || ctx.request_source.is_some()
            || self.effective_mode_for_tool("ExitPlanMode") == PermissionMode::BypassPermissions
        {
            return None;
        }
        Some(AutoModePrompt::ExitPlanMode)
    }

    /// Consume an engine-authorized Auto response at the policy boundary.
    ///
    /// The prompt transport only reports the user's selection; this gate owns
    /// the live permission mode. Keep the availability check and mode write
    /// behind `set_permission_mode`, whose mode lock closes the settings race
    /// before committing `Auto`. A stale or malicious Auto response is still a
    /// one-shot approval, while an eligible response that cannot transition is
    /// denied rather than pretending that Auto was enabled.
    async fn consume_auto_outcome(
        &self,
        outcome: PermissionOutcome,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        let PermissionOutcome::AllowAuto { updated_input } = outcome else {
            return outcome;
        };
        if ctx.auto_mode_prompt.is_none() || ctx.suppress_always_allow_rule {
            return PermissionOutcome::Allow {
                updated_input,
                permission_updates: Vec::new(),
                decision_classification: Some(
                    platform_api::permission_gate::ToolDecisionClassification::UserTemporary,
                ),
            };
        }
        match PermissionGate::set_permission_mode(self, "auto").await {
            Ok(()) => PermissionOutcome::AllowAuto { updated_input },
            Err(reason) => PermissionOutcome::Deny { reason },
        }
    }

    /// Authorize under the LIVE mode: the `set_permission_mode` override when
    /// set, else the policy's boot mode. Shared by every non-plan check path so a
    /// runtime mode change takes effect everywhere at once.
    fn effective_mode(&self) -> PermissionMode {
        self.mode_override
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .unwrap_or(self.policy.mode)
    }

    /// Effective mode for one specific tool, including the tighten-only
    /// per-MCP-server override used by Claude's control channel. The override is
    /// only consulted on MCP tools and only when the session-wide mode is one of
    /// the modes Claude lets the per-server pin narrow: `bypassPermissions`,
    /// `auto`, or `plan` while bypass remains available.
    fn effective_mode_for_tool(&self, tool_name: &str) -> PermissionMode {
        let base = self.effective_mode();
        let Some(server) = mcp_server_token(tool_name) else {
            return base;
        };
        let should_apply = matches!(
            base,
            PermissionMode::BypassPermissions | PermissionMode::Auto
        ) || (base == PermissionMode::Plan
            && self.policy.bypass_permissions_available
            && !self.policy.bypass_killswitch_active);
        if !should_apply {
            return base;
        }
        self.mcp_mode_overrides
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(server)
            .copied()
            .unwrap_or(base)
    }

    fn effective_authorize(&self, name: &str, input: &Value) -> (PermissionMode, PermissionResult) {
        self.effective_authorize_with_lease(name, input, None)
    }

    fn effective_authorize_with_lease(
        &self,
        name: &str,
        input: &Value,
        workspace_lease_token: Option<u64>,
    ) -> (PermissionMode, PermissionResult) {
        let mode = self.effective_mode_for_tool(name);
        (
            mode,
            self.authorize_with_live_state(name, input, mode, workspace_lease_token),
        )
    }

    /// claude-code `_pt` — the RULE + SAFETY verdict for a call, with **NO MODE
    /// BACKSTOP**.
    ///
    /// `_pt` walks deny rules → allow rules → the tool's own `checkPermissions`
    /// → ask rules → `requiresUserInteraction` → mcp `effectiveMaxPermission`,
    /// and returns **`null`** when none of them decides. It never consults the
    /// permission MODE — so an ordinary "Default mode, no matching rule,
    /// mutating tool" call yields NO verdict, and a caller resolving a hook
    /// `allow` lets that allow stand.
    ///
    /// This port folds rules + safety + mode into one `authorize_with_mode`, so
    /// the mode layer is SUBTRACTED here: a result whose reason is
    /// [`PermissionDecisionReason::PermissionMode`] is exactly what the mode tail
    /// produced (`ask_with_mode` / `allow_with_mode` / the plan-mutation ask) and
    /// therefore corresponds to `_pt`'s `null`. Everything else — deny rules, ask
    /// rules, and the safety walks (dangerous-removal, path constraints, sed,
    /// background-operator) — is a genuine rule/safety verdict and is returned.
    ///
    /// The evaluation runs under a NEUTRAL [`PermissionMode::Default`] rather
    /// than the session's mode. `_pt` consults no mode at all, and every
    /// mode-specific tail transform would otherwise masquerade as a rule verdict:
    /// `DontAsk` rewrites a surviving ask into a mode-tagged **deny**
    /// (`policy.rs:477`, the only `deny_with_mode` site), plan adds its mutation
    /// backstop, and `bypassPermissions`/`acceptEdits` add allows. Under
    /// `Default` none of those fire, and the remaining mode fallback
    /// (`ask_with_mode(Default)`) is filtered out below — leaving exactly the
    /// rule + safety walks. The session's real mode is still returned for
    /// auto-mode bookkeeping.
    /// Shared impl behind [`PermissionGate::check_after_hook_allow`] and its
    /// rich-outcome counterpart. The `lin` re-check of a
    /// hook `allow`: deny rule → deny; ask rule/safety → delegate to the inner
    /// transport (prompt / headless deny) carrying the dispatch context (real
    /// tool_use_id) enriched with the ask's serialized `decision_reason`, so the
    /// stdio `can_use_tool` is byte-faithful (was a fresh UUID + no reason); no
    /// verdict → the hook's allow stands.
    async fn check_after_hook_allow_impl(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        let (mode, verdict) = self.rule_or_safety_verdict(
            name,
            input,
            ctx.workspace_lease_token,
            &self.fold_call_context(ctx),
        );
        match verdict {
            Some(PermissionResult::Deny {
                reason,
                explanation,
                ..
            }) => {
                let msg = explanation.unwrap_or_else(|| deny_reason_string(&reason, name));
                tracing::warn!(
                    target: "permission",
                    "Hook returned 'allow' for {name}, but deny rule overrides: {msg}"
                );
                PermissionOutcome::Deny { reason: msg }
            }
            Some(PermissionResult::Ask {
                ref reason,
                ref metadata,
                ..
            }) => {
                tracing::warn!(
                    target: "permission",
                    "Hook returned 'allow' for {name}, but ask rule/safety check requires full permission pipeline"
                );
                // Re-check stays MODE-LESS (`rule_or_safety_verdict` evaluated
                // under `PermissionMode::Default`) — the oracle's post-hook-allow
                // handler hands the ask to the permission pipeline WITHOUT a mode
                // backstop, so we must not re-run the auto-mode classifier here
                // (HOOKALLOW-01). We DO carry the same control-request metadata the
                // normal Ask delegation builds so the stdio `can_use_tool` payload
                // matches, and apply any `updatedPermissions` the host allow
                // returns to the live session (parity with `setToolPermissionContext`).
                let mut ctx2 = ctx.clone();
                ctx2.decision_reason = serialize_decision_reason(reason);
                ctx2.decision_reason_type = decision_reason_type(reason).map(str::to_string);
                ctx2.classifier_approvable = classifier_approvable(reason);
                ctx2.matched_ask_rule = matched_ask_rule(reason);
                if metadata.permission_suggestions.is_some() {
                    ctx2.permission_suggestions = metadata.permission_suggestions.clone();
                }
                if metadata.blocked_path.is_some() {
                    ctx2.blocked_path = metadata.blocked_path.clone();
                }
                let outcome = self.check_prompt_transport(name, input, &ctx2).await;
                match self.consume_auto_outcome(outcome, &ctx2).await {
                    PermissionOutcome::Allow {
                        updated_input,
                        permission_updates,
                        decision_classification,
                    } => {
                        if !permission_updates.is_empty() {
                            self.apply_permission_updates(&permission_updates);
                        }
                        PermissionOutcome::Allow {
                            updated_input,
                            permission_updates,
                            // This branch exists only because the resolver
                            // required an interactive ask. A host that omits
                            // the optional classification still represents a
                            // temporary user grant, not a standing hook allow.
                            decision_classification: decision_classification.or(Some(
                                platform_api::permission_gate::ToolDecisionClassification::UserTemporary,
                            )),
                        }
                    }
                    PermissionOutcome::AllowAuto { updated_input } => {
                        PermissionOutcome::AllowAuto { updated_input }
                    }
                    PermissionOutcome::Deny { reason } => PermissionOutcome::Deny { reason },
                }
            }
            _ => {
                self.record_auto_mode_non_deny(mode);
                PermissionOutcome::Allow {
                    updated_input: None,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                }
            }
        }
    }

    fn flatten_permission_outcome(outcome: PermissionOutcome) -> PermissionDecision {
        match outcome {
            PermissionOutcome::Allow { .. } | PermissionOutcome::AllowAuto { .. } => {
                PermissionDecision::Allow
            }
            PermissionOutcome::Deny { reason } => PermissionDecision::Deny { reason },
        }
    }

    fn rule_or_safety_verdict(
        &self,
        name: &str,
        input: &Value,
        workspace_lease_token: Option<u64>,
        folded: &FoldedPermissionContext,
    ) -> (PermissionMode, Option<PermissionResult>) {
        let mode = self.effective_mode_for_tool(name);
        // `_pt` is mode-less, but it still reads `gn(t)` for the RULE state — so
        // the call's layer delta (extra command allow/deny rules and, above all,
        // an active `bashCommandClamp`) binds here too. A hook `allow` must not
        // be able to defeat a per-spawn clamp.
        let result = self.authorize_with_layers(
            name,
            input,
            PermissionMode::Default,
            workspace_lease_token,
            folded,
        );
        let verdict = match &result {
            // Deny rules + the tool's own `checkPermissions` denies. (A
            // mode-sourced deny cannot occur here: `deny_with_mode` is reachable
            // only under `DontAsk`, which this neutral evaluation excludes.)
            PermissionResult::Deny { .. } => Some(result),
            // `_pt` reports an ASK only for its three arms — an ask RULE (`MFo`,
            // including a nested `subcommandResults` ask), a `safetyCheck`
            // (`W9`), or a `sandboxOverride`. Guard asks tagged `Other` (path
            // constraints, `$IFS`/bash-safety, sed, PowerShell containment,
            // edit-read-deny) are `type:"other"` in the oracle too and `_pt`
            // SKIPS them — so they must NOT block a hook allow.
            PermissionResult::Ask { reason, .. } if Self::is_pt_ask_reason(reason) => Some(result),
            _ => None,
        };
        (mode, verdict)
    }

    /// A DECIDED deny (matched rule / safety check), the strongest verdict.
    const RANK_DECIDED_DENY: u8 = 4;

    /// Rank a verdict so the two coordinate spaces can be joined WITHOUT
    /// inverting the precedence each space applies internally.
    ///
    /// `authorize_inner` walks deny -> ask -> allow, so inside one space an
    /// explicit ask RULE outranks an allow rule. Joining on "Deny > Allow >
    /// Ask" would silently let an allow matched in one space override an ask
    /// rule the user deliberately wrote in the other, and the prompt would
    /// never fire.
    ///
    /// Verdicts whose reason is the MODE rank LOWEST: they are the
    /// no-rule-matched fallback (an unmatched Ask, or the Deny that `DontAsk`
    /// rewrites it into), not a decision about this path.
    fn coordinate_rank(result: &PermissionResult) -> u8 {
        match result {
            PermissionResult::Deny { reason, .. }
                if !matches!(reason, PermissionDecisionReason::PermissionMode { .. }) =>
            {
                Self::RANK_DECIDED_DENY
            }
            // EVERY ask that came from a rule or a guard, not just the `_pt`
            // ones. `ask_edit_read_deny_covered` is documented BYPASS-IMMUNE
            // and reports `Other`, so ranking it with the mode fallback would
            // put it BELOW `Allow` and let an allow matched in the other
            // coordinate space silently write a read-denied file.
            PermissionResult::Ask { reason, .. }
                if !matches!(reason, PermissionDecisionReason::PermissionMode { .. }) =>
            {
                3
            }
            PermissionResult::Allow { .. } => 2,
            // Mode fallback: an unmatched Ask, or a `DontAsk` mode Deny.
            _ => 1,
        }
    }

    /// Does this ask reason correspond to one of `_pt`'s ask arms?
    /// `MFo(reason)` = a matched rule whose behavior is `ask` (recursing into
    /// `subcommandResults`), `W9(reason)` = `safetyCheck`, plus `sandboxOverride`.
    fn is_pt_ask_reason(reason: &PermissionDecisionReason) -> bool {
        match reason {
            PermissionDecisionReason::MatchedRule { rule } => {
                rule.behavior == PermissionBehavior::Ask
            }
            PermissionDecisionReason::SubcommandResults { reasons } => {
                reasons.values().any(|r| match r.as_ref() {
                    PermissionResult::Ask { reason, .. } => Self::is_pt_ask_reason(reason),
                    _ => false,
                })
            }
            PermissionDecisionReason::SafetyCheck { .. }
            | PermissionDecisionReason::SandboxOverride { .. } => true,
            _ => false,
        }
    }

    /// Evaluate one call against the current in-memory permission overlay while
    /// forcing a caller-selected mode. Per-worker and plan-mode checks must see
    /// the same live rules and working directories as the ordinary check path.
    fn authorize_with_live_state(
        &self,
        name: &str,
        input: &Value,
        mode: PermissionMode,
        workspace_lease_token: Option<u64>,
    ) -> PermissionResult {
        let folded = FoldedPermissionContext::default();
        // GUEST-COORD (mobile DIVERGENCE). The model names files in guest
        // coordinates while every root in `FsRoots` is a host path, so a rule
        // like `Edit(./**)` relativizes the target to `../…` and matches
        // nothing. Rewrite the tool's declared path field onto its host twin
        // FOR THE DURATION OF THIS EVALUATION ONLY: the rewritten value is a
        // local. `permission` only ever emits `updated_input: None`, so the
        // prompt transport, the `updatedInput` back-flow, and the tool body
        // all keep seeing the model's own coordinates.
        let Some(rewritten) = self
            .path_translator
            .as_ref()
            .and_then(|t| crate::model_path::rewrite_tool_input(t.as_ref(), name, input))
        else {
            // Desktop, non-file tools, relative paths, host-coordinate paths,
            // and fenced guest regions: one evaluation, byte-identical to the
            // pre-existing behavior.
            return self.authorize_with_layers(name, input, mode, workspace_lease_token, &folded);
        };

        // UNION OF BOTH COORDINATE SPACES, with the lattice
        //     Deny (either) > Allow (either) > Ask.
        //
        // Evaluating only the host form would be a net FAIL-OPEN for any rule
        // written in the coordinates the model actually uses:
        // `Deny(Edit(//workspace/**))` resolves against root `/` and matches
        // the guest spelling today, but can never match the host twin. So the
        // ORIGINAL guest form is re-evaluated and a Deny from either side wins.
        //
        // Symmetrically, an ALLOW that already matched the guest path must
        // keep working — returning the host verdict whenever the guest side is
        // merely not-a-Deny would turn such a grant into a prompt, a
        // regression introduced by the fix itself.
        //
        // `Ask` never wins on its own: it is the no-rule-matched fallback, and
        // a guest path failing to match is precisely the bug being repaired.
        // Letting it veto would make the host Allow unreachable and leave
        // every write prompting.
        let host_verdict =
            self.authorize_with_layers(name, &rewritten, mode, workspace_lease_token, &folded);
        // COST, accepted: a file-tool call whose path actually needs
        // translation walks the rule set twice and deep-clones the tool input
        // (a `Write`'s `content` included). Both are inherent to joining two
        // coordinate spaces — the guest form cannot be evaluated without the
        // original input, and a rule may read any field. It is bounded to
        // mobile, to file tools, and to absolute guest paths under a mount;
        // a decided deny on the host form short-circuits the second walk.
        //
        // A DECIDED deny is final and skips the second walk. A deny that merely
        // reflects the MODE is not a decision — `DontAsk` rewrites every
        // unmatched Ask into one — and must not pre-empt the guest evaluation,
        // or a grant spelled only in guest coordinates would flip from Allow to
        // Deny the moment a translator is installed.
        if Self::coordinate_rank(&host_verdict) == Self::RANK_DECIDED_DENY {
            return host_verdict;
        }
        let guest_verdict =
            self.authorize_with_layers(name, input, mode, workspace_lease_token, &folded);
        // Ties go to the GUEST form. Equal rank means both spaces decided the
        // same way, so the verdicts are interchangeable in strength — but only
        // the guest one carries paths the model actually used. `ask_plan_mutation`
        // embeds the target path in its prompt, and that prompt is surfaced
        // verbatim as the headless deny reason, so preferring host here would
        // report a host container path the model never named.
        if Self::coordinate_rank(&host_verdict) > Self::coordinate_rank(&guest_verdict) {
            host_verdict
        } else {
            guest_verdict
        }
    }

    /// Build THIS call's ordered permission-layer stack — the port of
    /// `toolUseContext.permissionLayers` (claude-code `gn`, binary @287028951).
    ///
    /// Two of the ten upstream layer kinds already existed in this port as FLAT
    /// fields on [`PermissionCheckContext`]; they are hoisted to the HEAD of the
    /// stack in the order upstream's spawn path appends them, so a richer
    /// `ctx.permission_layers` entry can still override them:
    ///
    /// * `is_non_interactive_session` → `avoid_prompts`;
    /// * `mode_override` (a spawned subagent's clamped spawn mode) →
    ///   `permission_mode`.
    ///
    /// Everything else comes from the raw wire array
    /// [`PermissionCheckContext::permission_layers`].
    fn call_layers(ctx: &PermissionCheckContext) -> Vec<PermissionLayer> {
        let mut base: Vec<PermissionLayer> = Vec::new();
        if ctx.is_non_interactive_session {
            base.push(PermissionLayer::AvoidPrompts);
        }
        if let Some(mode) = ctx.mode_override.as_deref() {
            base.push(PermissionLayer::PermissionMode(mode.to_string()));
        }
        apply_context_layers(&base, &parse_permission_layers(&ctx.permission_layers))
    }

    /// `gn(toolUseContext)` for THIS call.
    ///
    /// `isBypassPermissionsModeAvailable` is the boot grant OR "the session is
    /// already running in `bypassPermissions`" — a session whose live mode IS
    /// bypass necessarily has the grant, and folding that in keeps the
    /// pre-layer `mode_override` behavior byte-identical while still honouring
    /// the settings killswitch (`gq()`), which upstream checks first.
    fn fold_call_context(&self, ctx: &PermissionCheckContext) -> FoldedPermissionContext {
        let layers = Self::call_layers(ctx);
        if layers.is_empty() {
            return FoldedPermissionContext::default();
        }
        fold_permission_layers(
            &layers,
            LayerFoldInputs {
                bypass_killswitch_active: self.policy.bypass_killswitch_active,
                bypass_permissions_available: self.policy.bypass_permissions_available
                    || self.effective_mode() == PermissionMode::BypassPermissions,
            },
        )
    }

    /// Authorize under the live overlay PLUS this call's folded layer delta.
    ///
    /// With an EMPTY fold this is exactly `live_policy().authorize_with_mode…`,
    /// i.e. byte-identical to the pre-layer path — which is the case for every
    /// call that carries no layers.
    fn authorize_with_layers(
        &self,
        name: &str,
        input: &Value,
        mode: PermissionMode,
        workspace_lease_token: Option<u64>,
        folded: &FoldedPermissionContext,
    ) -> PermissionResult {
        let policy = self.live_policy_with_layers(folded);
        if folded.bash_command_clamps.is_empty() {
            return policy.authorize_with_mode_and_workspace_lease(
                name,
                input,
                mode,
                workspace_lease_token,
            );
        }
        // `wTv` / `FJa` (binary @290295374) — while a `bashCommandClamp` is
        // attached the tool declares a FAIL-CLOSED posture through
        // `permissionCheckFailureDecision`, so a permission check that CRASHES
        // must DENY with `gCi`'s byte-locked message rather than propagate:
        // "denying rather than running an unverified command". Gated on an
        // active clamp exactly as upstream is, so an unclamped session keeps
        // today's unwind semantics untouched.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            policy.authorize_with_mode_and_workspace_lease(name, input, mode, workspace_lease_token)
        })) {
            Ok(result) => result,
            Err(_) => {
                tracing::error!(
                    target: "permission",
                    "permission check for {name} crashed under an active bashCommandClamp; denying"
                );
                PermissionResult::Deny {
                    reason: PermissionDecisionReason::Other {
                        reason: crate::bash_command_clamp::CLAMP_FAIL_CLOSED_REASON.to_string(),
                    },
                    explanation: Some(crate::bash_command_clamp::clamp_crash_deny_message(name)),
                    metadata: PermissionMetadata::default(),
                }
            }
        }
    }

    /// The live policy with this call's folded layer delta applied:
    /// `allowed_tools` / `disallowed_tools` land in the `command` rule source
    /// (upstream `ULa`/`jLa` write `alwaysAllowRules.command` /
    /// `alwaysDenyRules.command`), `working_directory` appends one additional
    /// working dir, and `bash_command_clamp` groups are attached for
    /// [`PermissionPolicy::bash_command_clamps`].
    fn live_policy_with_layers(&self, folded: &FoldedPermissionContext) -> PermissionPolicy {
        if folded.is_empty() {
            return self.live_policy();
        }
        let live = self
            .live_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut allow_rules = live.allow_rules;
        let mut deny_rules = live.deny_rules;
        let mut working_dirs = live.additional_working_dirs;
        Self::extend_command_rules(
            &mut allow_rules,
            &folded.allow_command_rules,
            PermissionBehavior::Allow,
        );
        Self::extend_command_rules(
            &mut deny_rules,
            &folded.deny_command_rules,
            PermissionBehavior::Deny,
        );
        self.fold_command_input_denies(&mut deny_rules);
        // PARITY `Zm` `case "working_directory"`:
        // `if (!ctx.additionalWorkingDirectories.has(dir))
        //    …set(dir, {path: dir, source: "session"})` — an existing entry keeps
        // its original source, so the guard is `!contains` and not a re-insert.
        if let Some(dir) = folded.additional_working_directory.as_ref() {
            if !working_dirs.contains(dir) {
                working_dirs.insert(dir.clone(), PermissionRuleSource::Session);
            }
        }
        self.policy
            .clone_with_live_state(allow_rules, deny_rules, live.ask_rules, working_dirs)
            .with_bash_command_clamps(folded.bash_command_clamps.clone())
    }

    /// `dKo`'s **acceptEdits simulation** — the fast path that answers an Auto
    /// mode call without a classifier round-trip when the SAME call would have
    /// been allowed outright in `acceptEdits`:
    ///
    /// ```js
    /// let ko=(Wr)=>e.checkPermissions(rs,{…toolPermissionContext:
    ///        {...Zo.toolPermissionContext,mode:"acceptEdits",alwaysAllowRules:yo}}),
    ///     vo=await ko([]);
    /// …
    /// if(vo.behavior==="allow"&&!jn){ …
    ///   t(`Skipping auto mode classifier for ${e.name}: would be allowed in acceptEdits mode`),
    ///   …De({updatedInput:vo.updatedInput??n,decisionReason:{type:"mode",mode:"auto"}})}
    /// ```
    ///
    /// `yo` is the allow-rule set with the dangerous-classifier rules filtered
    /// out, which is [`PermissionPolicy::apply_auto_mode_restrictions`] here:
    /// the probe runs under `AcceptEdits` but keeps `Auto`'s rule availability,
    /// so a `Bash(python:*)` allow cannot buy its way past the classifier
    /// through the probe. Without the probe, an ordinary in-tree `Edit` on an
    /// ABSOLUTE path — the shape a model produces constantly — pays a model
    /// call in Auto mode, and so does every `mkdir`/`touch`/`cp`/`mv`/`rm`/safe
    /// `sed` on `ACCEPT_EDITS_ALLOWED_COMMANDS`.
    ///
    /// Upstream guards the probe with `Xn===void 0 && Pe===void 0 && !Le &&
    /// e.sandboxNetworkLists?.(n)===void 0 && !ze && !fn && !zn`: a forced
    /// Chrome navigation, `classifierOnly()`, a `sandboxNetworkLists` /
    /// `sandboxOverride` decision reason, an edit-classification gate, and plan
    /// mode. Only the last has a port counterpart — `zn` is `U==="plan"`, and
    /// this is only ever called with the effective mode, which the caller has
    /// already established is `Auto`. The rest are structurally absent.
    ///
    /// Upstream also re-runs the probe with linked-worktree directories
    /// (`vo.decisionReason?.type==="workingDir" && UZt()` → `xMn` → `ko(Nn)`);
    /// that retry is NOT ported, which can only make this probe answer `false`
    /// more often — i.e. send the call to the classifier, never past it.
    fn accept_edits_fast_path(
        &self,
        name: &str,
        input: &Value,
        folded: &FoldedPermissionContext,
        workspace_lease_token: Option<u64>,
    ) -> bool {
        let policy = self
            .live_policy_with_layers(folded)
            .with_apply_auto_mode_restrictions(true);
        matches!(
            policy.authorize_with_mode_and_workspace_lease(
                name,
                input,
                PermissionMode::AcceptEdits,
                workspace_lease_token,
            ),
            PermissionResult::Allow { .. }
        )
    }

    /// `ULa` / `jLa` — append rule strings to the `command` source bucket,
    /// deduped (`to(...)`).
    fn extend_command_rules(
        bucket: &mut HashMap<PermissionRuleSource, Vec<PermissionRule>>,
        specs: &[String],
        behavior: PermissionBehavior,
    ) {
        if specs.is_empty() {
            return;
        }
        let rules = bucket.entry(PermissionRuleSource::Command).or_default();
        for spec in specs {
            let value = PermissionRuleValue::from_rule_string(spec);
            if rules.iter().any(|existing| existing.value == value) {
                continue;
            }
            rules.push(PermissionRule {
                value,
                behavior,
                source: PermissionRuleSource::Command,
            });
        }
    }

    /// Fold the current input's command denies into a deny bucket about to be
    /// evaluated. Every policy-building path calls this, so the rules behave
    /// exactly like the `command`-source denies upstream stores directly —
    /// they are just not PERSISTED into the shared bucket. See
    /// [`Self::command_input_denies`].
    fn fold_command_input_denies(
        &self,
        deny_rules: &mut HashMap<PermissionRuleSource, Vec<PermissionRule>>,
    ) {
        let specs = self
            .command_input_denies
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Self::extend_command_rules(deny_rules, &specs, PermissionBehavior::Deny);
    }

    fn live_policy(&self) -> PermissionPolicy {
        let live = self
            .live_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut deny_rules = live.deny_rules;
        self.fold_command_input_denies(&mut deny_rules);
        self.policy.clone_with_live_state(
            live.allow_rules,
            deny_rules,
            live.ask_rules,
            live.additional_working_dirs,
        )
    }

    /// Map a 3-valued [`PermissionResult`] onto the 2-valued
    /// [`PermissionDecision`] the orchestrator consumes: `Allow`/`Deny` pass
    /// through (the deny reason rendered), and an `Ask` either AUTO-ALLOWS a
    /// read-only / agent-local tool ([`PromptDefault::AllowByDefault`]) or
    /// DELEGATES to the inner prompt transport. Shared by [`Self::check`] (boot
    /// mode) and [`PermissionGate::check_in_plan_mode`] (live Plan mode) so an
    /// `Ask` is mapped identically regardless of which mode produced it.
    async fn decide(
        &self,
        mode: PermissionMode,
        result: PermissionResult,
        name: &str,
        input: &Value,
    ) -> PermissionDecision {
        // No worker attribution (main turn loop / plan-mode path).
        self.decide_with_worker(mode, result, name, input, None)
            .await
    }

    /// Like [`Self::decide`], but forwards the originating-worker identity to the
    /// inner prompt transport on the Ask-delegate path so the prompt is
    /// attributed (claude-code's worker permission badge). `None` → identical to
    /// [`Self::decide`].
    async fn decide_with_worker(
        &self,
        mode: PermissionMode,
        result: PermissionResult,
        name: &str,
        input: &Value,
        worker: Option<crate::gate::PromptWorker>,
    ) -> PermissionDecision {
        match result {
            PermissionResult::Allow { .. } => {
                self.record_auto_mode_non_deny(mode);
                PermissionDecision::Allow
            }
            PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => PermissionDecision::Deny {
                reason: explanation.unwrap_or_else(|| deny_reason_string(&reason, name)),
            },
            PermissionResult::Ask { ref reason, .. } => {
                if self.policy.is_restricted_protected_mutation(name, input) {
                    // Protected restricted writes must never enter the
                    // classifier or an unsuppressed transport path. A TUI or
                    // configured handler may still approve once, but it may
                    // not persist an AllowAlways rule for this call.
                    let ctx = PermissionCheckContext {
                        worker: worker.clone(),
                        suppress_always_allow_rule: true,
                        ..PermissionCheckContext::default()
                    };
                    let decision = match self.inner.check_with_context(name, input, &ctx).await {
                        PermissionOutcome::Allow { .. } | PermissionOutcome::AllowAuto { .. } => {
                            PermissionDecision::Allow
                        }
                        PermissionOutcome::Deny { reason } => PermissionDecision::Deny { reason },
                    };
                    if matches!(decision, PermissionDecision::Allow) {
                        self.record_auto_mode_non_deny(mode);
                    }
                    return decision;
                }
                match self
                    .auto_mode_classifier_result(mode, reason, name, input, false, &|| {
                        self.accept_edits_fast_path(
                            name,
                            input,
                            &FoldedPermissionContext::default(),
                            None,
                        )
                    })
                    .await
                {
                    Ok(AutoModeClassifierResult::Classified(classified)) => {
                        return self.classified_result_to_decision(classified, name);
                    }
                    Ok(AutoModeClassifierResult::NoDecision)
                    | Ok(AutoModeClassifierResult::PromptFallback { .. }) => {}
                    Err(abort) => {
                        // `false` above makes this unreachable; retain a
                        // fail-closed mapping for custom/future classifiers on
                        // this legacy two-valued surface.
                        return PermissionDecision::Deny {
                            reason: abort.message,
                        };
                    }
                }
                if read_only_default_auto_allows(name, reason, mode) {
                    // Read-only / agent-local tool with NO explicit `ask` rule —
                    // auto-allow rather than ask-storm (phase-2 stand-in for the
                    // per-tool default). An explicit `ask` rule (tool-wide or
                    // content) tags the ask `MatchedRule` and is NOT short-circuited
                    // here — it falls through to the prompt transport below.
                    PermissionDecision::Allow
                } else {
                    // Surface the prompt through the host's transport, carrying
                    // the worker identity so it is attributed in the dialog.
                    let decision = self.inner.check_with_worker(name, input, worker).await;
                    if matches!(decision, PermissionDecision::Allow) {
                        self.record_auto_mode_non_deny(mode);
                    }
                    decision
                }
            }
        }
    }

    /// Like [`Self::decide_with_worker`] but returns a [`PermissionOutcome`] that
    /// can carry the host/policy `updatedInput` rewrite, forwarding the full
    /// [`PermissionCheckContext`] (real `tool_use_id`, decision reason) to the
    /// inner transport on the Ask-delegate path. Used by the tool-dispatch
    /// `check_with_context` seam.
    async fn decide_outcome_with_context(
        &self,
        mode: PermissionMode,
        result: PermissionResult,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
        accept_edits_fast_path: &(dyn Fn() -> bool + Sync),
    ) -> Result<PermissionOutcome, PermissionAbort> {
        match result {
            // A policy-rule allow may itself carry a rewritten input — surface it
            // (previously dropped at the `PermissionDecision::Allow` boundary). The
            // local policy gate never derives `updatedPermissions` rule updates
            // (those originate from the stdio host's `can_use_tool` response), so
            // `permission_updates` is always empty on this path.
            PermissionResult::Allow { updated_input, .. } => {
                self.record_auto_mode_non_deny(mode);
                Ok(PermissionOutcome::Allow {
                    updated_input,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                })
            }
            PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => {
                // GATE-SYSMSG-01: notify the transport of this LOCAL deny so the
                // stdio control-plane can emit a `permission_denied` system message
                // (createCanUseTool's deny arm). Compute the reason discriminant +
                // `oin`-filtered reason text from the deny reason BEFORE it is moved
                // into the rendered message. No-op on non-stdio transports.
                let drt = decision_reason_type(&reason);
                let dr = sysmsg_decision_reason(&reason);
                let message = explanation.unwrap_or_else(|| deny_reason_string(&reason, name));
                self.inner
                    .on_permission_denied(name, ctx, drt, dr.as_deref(), &message)
                    .await;
                Ok(PermissionOutcome::Deny { reason: message })
            }
            PermissionResult::Ask {
                ref reason,
                ref metadata,
                ref prompt,
                ..
            } => {
                // PERM-07 (claude-code 2.1.238 `STv`): inside the Auto-mode ask
                // arm (`if($mt(d)||h)`), a hook-established ask floor in a
                // session that cannot surface prompts is a hard DENY, not a
                // delegated ask:
                //   let S=a.hookAskFloor===!0;
                //   if(S&&!Ski()&&u.shouldAvoidPermissionPrompts)return DJa(l.message);
                // (`Ski()` is a constant `false` in the shipped build, so the
                // `!Ski()` conjunct is always satisfied.) 2.1.238 wraps the ask
                // message in the new `Ixf` copy; 2.1.220's `G8s` forwarded it
                // verbatim. `$mt(d)` is `auto || (plan && autoActive)`; the
                // `plan && autoActive` half and the `h` chrome-consent disjunct
                // have no port counterpart, so only `auto` is gated here.
                if mode == PermissionMode::Auto
                    && ctx.hook_ask_floor
                    && ctx.is_non_interactive_session
                {
                    let message = prompts_unavailable_deny_message(&prompt.message);
                    self.inner
                        .on_permission_denied(
                            name,
                            ctx,
                            Some("asyncAgent"),
                            Some(PROMPTS_UNAVAILABLE_ASYNC_AGENT_REASON),
                            &message,
                        )
                        .await;
                    return Ok(PermissionOutcome::Deny { reason: message });
                }
                // HOOK-ASKFLOOR-03: when a PreToolUse hook returned `ask`
                // (`ctx.hook_ask_floor`), the Auto-mode classifier's ALLOW must NOT
                // silently defeat the hook's ask — the 2.1.207 regression 211/215
                // removed. The floor lives in `De`, `dKo`'s ALLOW callback:
                //
                // ```js
                // let De=(rs)=>{ …
                //   if(xe){ if(U==="dontAsk")return{behavior:"deny",…};
                //           return {...M, updatedInput:rs.updatedInput} }   // M is the ASK
                //   return{behavior:"allow",...rs}};
                // ```
                //
                // `xe` is `v.hookAskFloor===!0`, and `De` is only ever called on
                // the allow paths. The classifier still RUNS under the floor and
                // its DENY arms return before `De` ever sees them — so a hook
                // `ask` in front of a blocked action is still a deny upstream.
                // Skipping the classifier outright (what this did) kept the
                // allow side right and silently dropped the deny side, turning
                // every hook-asked dangerous action into a user prompt.
                let fallback_decision_reason = match self
                    .auto_mode_classifier_result(
                        mode,
                        reason,
                        name,
                        input,
                        ctx.is_non_interactive_session,
                        accept_edits_fast_path,
                    )
                    .await?
                {
                    AutoModeClassifierResult::Classified(classified) => {
                        if ctx.hook_ask_floor
                            && matches!(classified, PermissionResult::Allow { .. })
                        {
                            // `De`'s `xe` arm: keep `M`, the hook's ask. It
                            // falls through to the normal path below, which
                            // prompts interactively and denies headless.
                            None
                        } else {
                            return Ok(self.classified_result_to_outcome(classified, name));
                        }
                    }
                    AutoModeClassifierResult::PromptFallback { decision_reason } => {
                        Some(decision_reason)
                    }
                    AutoModeClassifierResult::NoDecision => None,
                };
                if read_only_default_auto_allows(name, reason, mode) {
                    self.record_auto_mode_non_deny(mode);
                    Ok(PermissionOutcome::Allow {
                        updated_input: None,
                        permission_updates: Vec::new(),
                        decision_classification: None,
                    })
                } else {
                    // Delegate to the inner transport WITH the context so a stdio
                    // `can_use_tool` request carries the real tool_use_id and its
                    // allow can return the host's `updatedInput`. Enrich the ctx
                    // here (where the policy `reason` is in scope) with the
                    // serialized `decision_reason` — claude-code
                    // `createCanUseTool` sets `decision_reason:
                    // serializeDecisionReason(mainPermissionResult.decisionReason)`.
                    // The turn loop builds the ctx with only `tool_use_id`; we add
                    // the reason without touching the unit `PermissionResolution::Ask`.
                    let mut ctx2 = ctx.clone();
                    let breaker_fallback = fallback_decision_reason.is_some();
                    if ctx2.decision_reason.is_none() {
                        ctx2.decision_reason = fallback_decision_reason
                            .clone()
                            .or_else(|| serialize_decision_reason(reason));
                    }
                    if ctx2.decision_reason_type.is_none() {
                        ctx2.decision_reason_type = if breaker_fallback {
                            Some("classifier".to_string())
                        } else {
                            decision_reason_type(reason).map(str::to_string)
                        };
                    }
                    // These two fields are functions of the authoritative
                    // decision reason. Clear any stale caller value when the
                    // reason does not support it instead of emitting metadata
                    // for an unrelated Ask.
                    ctx2.classifier_approvable = classifier_approvable(reason);
                    ctx2.matched_ask_rule = matched_ask_rule(reason);
                    ctx2.auto_mode_prompt =
                        self.auto_prompt_for_ask(mode, name, input, reason, &ctx2);
                    // Tool-specific policy producers take precedence; retain an
                    // explicitly supplied transport context only when the
                    // permission result has no structured value.
                    if metadata.permission_suggestions.is_some() {
                        ctx2.permission_suggestions = metadata.permission_suggestions.clone();
                    }
                    if metadata.blocked_path.is_some() {
                        ctx2.blocked_path = metadata.blocked_path.clone();
                    }
                    let outcome = self
                        .consume_auto_outcome(
                            self.check_prompt_transport(name, input, &ctx2).await,
                            &ctx2,
                        )
                        .await;
                    if let PermissionOutcome::Allow {
                        permission_updates, ..
                    } = &outcome
                    {
                        self.record_auto_mode_non_deny(mode);
                        // In-memory apply (claude-code `setToolPermissionContext(
                        // u=>bJ(u,updates))`): when the host's allow carried
                        // `updatedPermissions`, apply them to the LIVE session so
                        // subsequent checks see the change — not only the next
                        // session load. Mode, rule, and directory arms all fold
                        // through the same live reducer below.
                        if !permission_updates.is_empty() {
                            self.apply_permission_updates(permission_updates);
                        }
                    }
                    if matches!(&outcome, PermissionOutcome::AllowAuto { .. }) {
                        self.record_auto_mode_non_deny(mode);
                    }
                    Ok(outcome)
                }
            }
        }
    }

    /// Like [`Self::decide`] but WITHOUT consulting the inner prompt transport:
    /// returns a [`PermissionResolution`] that carries the deny SOURCE and, for a
    /// would-be prompt, an [`PermissionResolution::Ask`] (or
    /// [`PermissionResolution::AskWithContext`] when the classifier breaker
    /// rewrites the reason) instead of resolving it.
    /// The turn loop uses this to fire the source-gated permission hooks
    /// (`PermissionRequest` on `Ask`, `PermissionDenied` on a classifier `Deny`)
    /// before delegating to the transport. See [`PermissionGate::resolve_detailed`].
    async fn resolve_with_mode(
        &self,
        mode: PermissionMode,
        result: PermissionResult,
        name: &str,
        input: &Value,
        is_non_interactive_session: bool,
        accept_edits_fast_path: &(dyn Fn() -> bool + Sync),
    ) -> Result<PermissionResolution, PermissionAbort> {
        match result {
            PermissionResult::Allow { ref reason, .. } => Ok(PermissionResolution::Allow {
                rule_source: rule_settings_source(reason),
                classifier_approved: matches!(
                    reason,
                    PermissionDecisionReason::ClassifierApproved { .. }
                ),
            }),
            PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => Ok(PermissionResolution::Deny {
                source: map_decision_source(&reason),
                rule_source: rule_settings_source(&reason),
                // GATE-SYSMSG-01: pre-compute the system-message discriminants from
                // the FULL reason (in scope here) so the turn loop can emit the
                // `permission_denied` frame on the main-conversation deny path —
                // faithful for rule/mode AND classifier (the classifier deny
                // recurses back into this arm with a `ClassifierRejected` reason).
                decision_reason_type: decision_reason_type(&reason).map(str::to_string),
                decision_reason: sysmsg_decision_reason(&reason),
                reason: explanation.unwrap_or_else(|| deny_reason_string(&reason, name)),
                // The policy gate's rule/mode/classifier denials are never an
                // `ask`-behavior rejection carrying contentBlocks (the external
                // build has no classifier/contentBlocks producer), so the
                // top-level-image path stays dormant — byte-identical to before.
                behavior_ask: false,
                content_blocks: Vec::new(),
            }),
            PermissionResult::Ask { ref reason, .. } => {
                match self
                    .auto_mode_classifier_result(
                        mode,
                        reason,
                        name,
                        input,
                        is_non_interactive_session,
                        accept_edits_fast_path,
                    )
                    .await?
                {
                    AutoModeClassifierResult::Classified(classified) => {
                        return Box::pin(self.resolve_with_mode(
                            mode,
                            classified,
                            name,
                            input,
                            is_non_interactive_session,
                            accept_edits_fast_path,
                        ))
                        .await;
                    }
                    AutoModeClassifierResult::PromptFallback { decision_reason } => {
                        return Ok(PermissionResolution::AskWithContext {
                            decision_reason_type: Some("classifier".to_string()),
                            decision_reason: Some(decision_reason),
                        });
                    }
                    AutoModeClassifierResult::NoDecision => {}
                }
                if read_only_default_auto_allows(name, reason, mode) {
                    // Read-only / agent-local tool with NO explicit `ask` rule —
                    // auto-allowed, no prompt. No rule matched, so no scope.
                    Ok(PermissionResolution::Allow {
                        rule_source: None,
                        classifier_approved: false,
                    })
                } else {
                    // A would-be prompt (a mutating tool, OR an explicit `ask`
                    // rule on a read-only tool): the turn loop fires
                    // PermissionRequest before this is delegated to the transport.
                    Ok(PermissionResolution::Ask)
                }
            }
        }
    }

    async fn auto_mode_classifier_result(
        &self,
        mode: PermissionMode,
        reason: &PermissionDecisionReason,
        name: &str,
        input: &Value,
        is_non_interactive_session: bool,
        accept_edits_fast_path: &(dyn Fn() -> bool + Sync),
    ) -> Result<AutoModeClassifierResult, PermissionAbort> {
        if mode != PermissionMode::Auto
            || !crate::classifier::is_classifier_permissions_enabled()
            || !reason_allows_classifier(reason)
        {
            return Ok(AutoModeClassifierResult::NoDecision);
        }
        {
            let tracking = self
                .policy
                .denial_tracking
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if tracking.is_circuit_broken() {
                return Ok(AutoModeClassifierResult::NoDecision);
            }
        }
        // `dKo` runs the acceptEdits SIMULATION before the safe allowlist; keep
        // that order so the telemetry reason a future port emits is the one
        // upstream emits. See [`Self::accept_edits_fast_path`].
        // Evaluated HERE, not at the call sites: the probe is a full second
        // policy walk (clone the live policy, re-authorize under `AcceptEdits`),
        // and every caller reaches this method on every check in every mode.
        // Passing it by value ran that walk for Default/Plan/acceptEdits/bypass/
        // dontAsk too, and inside Auto for calls that had already resolved to
        // Allow or Deny. Behind `&dyn Fn()` it runs only where `dKo` runs it:
        // an Auto-mode `ask` that the classifier is about to judge.
        if accept_edits_fast_path() {
            self.record_auto_mode_non_deny(mode);
            tracing::debug!(
                target: "permission",
                "Skipping auto mode classifier for {name}: would be allowed in acceptEdits mode"
            );
            return Ok(AutoModeClassifierResult::Classified(
                PermissionResult::Allow {
                    reason: PermissionDecisionReason::PermissionMode {
                        mode: PermissionMode::Auto,
                    },
                    updated_input: None,
                    update_destination: None,
                    metadata: PermissionMetadata::default(),
                },
            ));
        }
        // `dKo`'s safe-allowlist FAST PATH, which runs BEFORE the classifier:
        //
        // ```js
        // if(Xn===void 0&&Pe===void 0&&!Le&&Sft(e.name,n)){
        //   t(`Skipping auto mode classifier for ${e.name}: tool is on the safe allowlist`),
        //   …De({updatedInput:M.updatedInput??n,decisionReason:{type:"mode",mode:"auto"}})}
        // ```
        //
        // `Sft` is [`crate::mode_policy::is_auto_mode_safe_tool`]. The three
        // guards upstream ANDs in are all structurally absent here: `Xn` is a
        // forced Chrome navigation, `Pe` is `classifierOnly()` (no port tool
        // declares it), and `Le` is a `sandboxNetworkLists` decision reason —
        // none of which this build can produce, so the predicate reduces to
        // `Sft` alone. Upstream also resets the consecutive-denial counter here
        // (`if(!Ame(v))ZJ(v,D6)`), which is [`Self::record_auto_mode_non_deny`].
        if crate::mode_policy::is_auto_mode_safe_tool(name) {
            self.record_auto_mode_non_deny(mode);
            tracing::debug!(
                target: "permission",
                "Skipping auto mode classifier for {name}: tool is on the safe allowlist"
            );
            return Ok(AutoModeClassifierResult::Classified(
                PermissionResult::Allow {
                    reason: PermissionDecisionReason::PermissionMode {
                        mode: PermissionMode::Auto,
                    },
                    updated_input: None,
                    update_destination: None,
                    metadata: PermissionMetadata::default(),
                },
            ));
        }
        // SH-01 — the auto-mode classifier is the DESTINATION for every
        // `classifierContext` a `PostToolUse` hook attached (oracle 2.1.238
        // @ 296466460 / 296974134). The hooks executor publishes them to
        // `host_context::store()`; this is where they are read back and handed
        // to the classifier. Without this read the whole hook-side pipeline
        // would be a parse with no consumer.
        let host_context = crate::host_context::store().snapshot();
        let mut classified =
            crate::classifier::classify_tool_call_with_host_context(name, input, &host_context);
        // `dKo`'s terminal step: everything that survived the fast paths is
        // judged by the two-stage auto-mode classifier
        // (`Yn = await <classifier>`), NOT by a local table. This port owns the
        // whole classifier — the bundled 2.1.270 system prompt, the fast/thinking
        // stages and the transcript renderer all live in
        // [`crate::loop_llm`] + `orchestrator::loop_permission_classifier` — but
        // it used to be reachable for exactly three tool names
        // (`CronCreate` / `ScheduleWakeup` / `Monitor`), a leftover from the
        // batch that introduced it for the `/loop` tools. Every other tool fell
        // through to the offline `classify_tool_call` table, whose `Pass` arm is
        // a PROMPT: 23 of 50 ordinary development shell commands raised a
        // permission request in Auto mode, which is the whole complaint about
        // Auto mode prompting on everything.
        //
        // The deterministic table is kept AHEAD of the classifier, where it now
        // plays the role of upstream's remaining fast paths: an `Allow` verdict
        // (read-only command, local operation, in-tree file mutation) is
        // answered without paying a round-trip, exactly as `Sft` above is.
        // Anything else — `Pass` or a local `Deny` — is the classifier's call,
        // matching `dKo`, which has no local deny list at all.
        //
        // When no classifier is bound (a host that never filled
        // `loop_classifier_handle`, e.g. a bare `PolicyPermissionGate` in a
        // test) the local verdict stands, so those hosts keep today's behavior
        // instead of silently losing the deny side.
        let already_allowed = matches!(classified.verdict, AutoModeClassifierVerdict::Allow { .. });
        // A HARD deny is a boundary, not an opinion: `classify_tool_call_with_host_context`
        // documents that host context "still never lifts a HARD BLOCK boundary", and the
        // same has to hold for the LLM stage — otherwise `hard_shell_denial`'s
        // exfiltration/persistence list is advisory and a model answer can wave it through.
        let hard_denied = matches!(
            classified.verdict,
            AutoModeClassifierVerdict::Deny { hard: true, .. }
        );
        if !already_allowed && !hard_denied {
            if let Some(classifier) = self.loop_classifier.get() {
                let deny_rules = {
                    let live = self.live_state.read().unwrap_or_else(|e| e.into_inner());
                    let mut sources: Vec<_> = live.deny_rules.iter().collect();
                    sources.sort_by_key(|(source, _)| std::cmp::Reverse(source.priority()));
                    let mut rules = Vec::new();
                    for (source, entries) in sources {
                        if matches!(
                            source,
                            PermissionRuleSource::Command | PermissionRuleSource::ToolsNarrowing
                        ) {
                            continue;
                        }
                        for entry in entries {
                            if entry
                                .value
                                .rule_content
                                .as_deref()
                                .is_some_and(|content| content.starts_with("prompt:"))
                            {
                                continue;
                            }
                            let rule = entry.value.to_rule_string();
                            if !rules.contains(&rule) {
                                rules.push(rule);
                            }
                        }
                    }
                    rules
                };
                classified.verdict = classifier
                    .classify(name, input, &host_context, &deny_rules)
                    .await;
                classified.host_context_is_inert_for_this_verdict = false;
            }
        }
        if !host_context.is_empty() {
            tracing::debug!(
                target: "permission",
                "auto-mode classifier saw {} host-context line(s), {} eligible as user intent (inert for this verdict: {})",
                host_context.len(),
                classified.eligible_live_contexts,
                classified.host_context_is_inert_for_this_verdict,
            );
        }
        match classified.verdict {
            AutoModeClassifierVerdict::Allow { score, .. } => {
                self.record_auto_mode_non_deny(mode);
                Ok(AutoModeClassifierResult::Classified(
                    PermissionResult::Allow {
                        reason: PermissionDecisionReason::ClassifierApproved {
                            classifier: ClassifierKind::Transcript,
                            score,
                        },
                        updated_input: None,
                        update_destination: None,
                        metadata: PermissionMetadata::default(),
                    },
                ))
            }
            AutoModeClassifierVerdict::Deny { score, reason, .. } => {
                let mut tracking = self
                    .policy
                    .denial_tracking
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                tracking.record_auto_deny();
                if let Some(trip) = tracking.trip(is_non_interactive_session) {
                    telemetry::emit_auto_mode_denial_limit_exceeded(
                        trip.mode_tag(),
                        trip.consecutive_denials,
                        trip.total_denials,
                        name,
                    );
                    if trip.headless {
                        return Err(PermissionAbort {
                            message:
                                crate::denial_tracking::DenialBreakerTrip::HEADLESS_ABORT_MESSAGE
                                    .to_string(),
                        });
                    }
                    tracing::warn!(target: "permission", "{}", trip.fallback_warn_line());
                    return Ok(AutoModeClassifierResult::PromptFallback {
                        // Claude's breaker receives the classifier's own
                        // human-readable blocked-action reason (`q.reason`),
                        // not a synthesized tool-name prefix.
                        decision_reason: trip.decision_reason(&reason),
                    });
                }
                Ok(AutoModeClassifierResult::Classified(
                    PermissionResult::Deny {
                        reason: PermissionDecisionReason::ClassifierRejected {
                            classifier: ClassifierKind::Transcript,
                            score,
                            reason: reason.clone(),
                        },
                        explanation: Some(format!("Auto mode classifier blocked action: {reason}")),
                        metadata: PermissionMetadata::default(),
                    },
                ))
            }
            // `dKo`'s `Yn.unavailable` / `Yn.refusedBySafeguard` arms: deny
            // fail-closed with retry guidance, but do NOT call `ZJ(v,xft)` —
            // the classifier never judged the action, so it cannot advance the
            // consecutive-denial counter. Three provider hiccups would
            // otherwise trip the breaker and drop Auto mode back to prompting
            // for the rest of the session, which is the failure the whole
            // classifier path exists to avoid. `record_auto_mode_non_deny` is
            // not called either: this is still a deny, it just isn't counted.
            AutoModeClassifierVerdict::NoVerdict { reason, message } => {
                tracing::warn!(
                    target: "permission",
                    "Auto mode classifier gave no verdict for {name} ({reason}); denying (fail closed, exempt from the denial counter)"
                );
                Ok(AutoModeClassifierResult::Classified(
                    PermissionResult::Deny {
                        reason: PermissionDecisionReason::ClassifierRejected {
                            classifier: ClassifierKind::Transcript,
                            score: crate::classifier::RULE_MATCH_SCORE,
                            reason,
                        },
                        explanation: Some(message),
                        metadata: PermissionMetadata::default(),
                    },
                ))
            }
            // `dKo`'s `Yn.transcriptTooLong` arm, which does NOT deny — it
            // hands the call back to normal permission handling and names
            // `/compact` as the way out. Two carve-outs come first:
            //
            // ```js
            // if(e.name===ht) return{behavior:"allow",updatedInput:n,decisionReason:{type:"mode",mode:"auto"}};
            // …
            // if(F.shouldAvoidPermissionPrompts) throw new Ye("Agent aborted: …");
            // …
            // return {...M, decisionReason:Wmt(M,{type:"other",reason:eut})}
            // ```
            //
            // `ht` is `"Agent"`: spawning a subagent is how a session ESCAPES an
            // over-long transcript, so gating it behind a transcript it cannot
            // shorten would be a deadlock.
            AutoModeClassifierVerdict::TranscriptTooLong => {
                if crate::rule::normalize_legacy_tool_name(name) == "Agent" {
                    // No `ZJ(v,D6)` here: unlike the acceptEdits and allowlist
                    // fast paths, this arm does NOT reset the consecutive-denial
                    // counter upstream.
                    return Ok(AutoModeClassifierResult::Classified(
                        PermissionResult::Allow {
                            reason: PermissionDecisionReason::PermissionMode {
                                mode: PermissionMode::Auto,
                            },
                            updated_input: None,
                            update_destination: None,
                            metadata: PermissionMetadata::default(),
                        },
                    ));
                }
                if is_non_interactive_session {
                    return Err(PermissionAbort {
                        message: crate::loop_llm::TRANSCRIPT_TOO_LONG_HEADLESS_ABORT.to_string(),
                    });
                }
                tracing::warn!(
                    target: "permission",
                    "Auto mode classifier transcript too long, falling back to normal permission handling"
                );
                Ok(AutoModeClassifierResult::PromptFallback {
                    decision_reason: crate::loop_llm::TRANSCRIPT_TOO_LONG.to_string(),
                })
            }
            AutoModeClassifierVerdict::Pass { .. } => Ok(AutoModeClassifierResult::NoDecision),
        }
    }

    fn classified_result_to_decision(
        &self,
        result: PermissionResult,
        name: &str,
    ) -> PermissionDecision {
        match result {
            PermissionResult::Allow { .. } => PermissionDecision::Allow,
            PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => PermissionDecision::Deny {
                reason: explanation.unwrap_or_else(|| deny_reason_string(&reason, name)),
            },
            PermissionResult::Ask { .. } => PermissionDecision::Deny {
                reason: format!("Permission to use {name} has been denied."),
            },
        }
    }

    fn classified_result_to_outcome(
        &self,
        result: PermissionResult,
        name: &str,
    ) -> PermissionOutcome {
        match result {
            PermissionResult::Allow { updated_input, .. } => PermissionOutcome::Allow {
                updated_input,
                permission_updates: Vec::new(),
                decision_classification: None,
            },
            PermissionResult::Deny {
                reason,
                explanation,
                ..
            } => PermissionOutcome::Deny {
                reason: explanation.unwrap_or_else(|| deny_reason_string(&reason, name)),
            },
            PermissionResult::Ask { .. } => PermissionOutcome::Deny {
                reason: format!("Permission to use {name} has been denied."),
            },
        }
    }

    fn record_auto_mode_non_deny(&self, mode: PermissionMode) {
        if mode == PermissionMode::Auto {
            self.policy
                .denial_tracking
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record_non_deny();
        }
    }

    /// Apply a SINGLE host `updatedPermissions` entry to the LIVE session
    /// in-memory (claude-code `Xb` — the per-update reducer folded by `bJ`).
    ///
    /// This is the in-memory half of `applyPermissionUpdate`: when a
    /// `can_use_tool` (or `PermissionRequest` hook) ALLOW response carries
    /// `updatedPermissions`, the SAME session's later checks must see the change
    /// — not only the NEXT session load. Here we port the **`setMode`** arm
    /// byte-faithfully (the only arm that maps cleanly onto the gate's existing
    /// interior-mutable state, [`Self::mode_override`]):
    ///
    /// - `bypassPermissions` is REJECTED (no mode change) when bypass is not
    ///   available — the disabled-by-settings killswitch OR a session not
    ///   launched with `--dangerously-skip-permissions`
    ///   (`!isBypassPermissionsModeAvailable`) — with the byte-exact
    ///   `Ignoring permission update: setMode 'bypassPermissions' rejected …`
    ///   debug log. NOTE this differs from
    ///   [`PermissionGate::set_permission_mode`]: `Xb` applies the mode RAW with
    ///   ONLY the bypass-availability guard (no `auto` `Nle` gate — that guards
    ///   the interactive `setPermissionMode` handler, not this reducer).
    /// - any other mode is applied, logged `Applying permission update: Setting
    ///   mode to '<mode>'`. An UNKNOWN mode string is logged (matching `Xb`,
    ///   which sets the raw string and lets `transitionPermissionMode` no-op it)
    ///   but leaves the typed [`Self::mode_override`] unchanged.
    ///
    /// Rule and directory updates are applied to the gate's live overlay and
    /// therefore affect subsequent model, subagent, and prompt-shell checks in
    /// the current session. Persistence remains the control plane's concern.
    pub fn apply_permission_update(&self, update: &Value) {
        match update.get("type").and_then(Value::as_str) {
            Some("setMode") => {
                let Some(mode_str) = update.get("mode").and_then(Value::as_str) else {
                    return;
                };
                if mode_str == "auto"
                    && self
                        .auto_mode_disabled_from_settings
                        .load(std::sync::atomic::Ordering::Acquire)
                {
                    tracing::debug!(
                        "Ignoring permission update: setMode 'auto' rejected — auto mode disabled by settings"
                    );
                    return;
                }
                let bypass_unavailable = self.policy.bypass_killswitch_active
                    || !self.policy.bypass_permissions_available;
                if mode_str == "bypassPermissions" && bypass_unavailable {
                    tracing::debug!(
                        "Ignoring permission update: setMode 'bypassPermissions' rejected — mode is not available (disableBypassPermissionsMode set, or session not launched in bypassPermissions mode)"
                    );
                    return;
                }
                tracing::debug!("Applying permission update: Setting mode to '{mode_str}'");
                if let Some(parsed) = parse_settable_mode(mode_str) {
                    let mut mode_override = self
                        .mode_override
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    if parsed == PermissionMode::Auto
                        && self
                            .auto_mode_disabled_from_settings
                            .load(std::sync::atomic::Ordering::Acquire)
                    {
                        tracing::debug!(
                            "Ignoring permission update: setMode 'auto' rejected — auto mode disabled by settings"
                        );
                        return;
                    }
                    *mode_override = Some(parsed);
                }
            }
            Some("addRules") | Some("replaceRules") | Some("removeRules") => {
                let Some(source) = Self::parse_update_destination(
                    update.get("destination").unwrap_or(&Value::Null),
                ) else {
                    return;
                };
                let Some(behavior) =
                    Self::parse_update_behavior(update.get("behavior").unwrap_or(&Value::Null))
                else {
                    return;
                };
                let Some(rules) = Self::parse_update_rules(
                    update.get("rules").unwrap_or(&Value::Null),
                    behavior,
                    source,
                ) else {
                    return;
                };
                let mut live = self.live_state.write().unwrap_or_else(|e| e.into_inner());
                let bucket = Self::rule_bucket_mut(&mut live, behavior);
                match update.get("type").and_then(Value::as_str) {
                    Some("addRules") => {
                        let entry = bucket.entry(source).or_default();
                        for rule in rules {
                            if !entry.iter().any(|existing| existing.value == rule.value) {
                                entry.push(rule);
                            }
                        }
                    }
                    Some("replaceRules") => {
                        bucket.insert(source, rules);
                    }
                    Some("removeRules") => {
                        let to_remove: Vec<_> = rules.into_iter().map(|rule| rule.value).collect();
                        if let Some(entry) = bucket.get_mut(&source) {
                            entry.retain(|existing| {
                                !to_remove.iter().any(|rule| rule == &existing.value)
                            });
                        }
                    }
                    _ => {}
                }
            }
            Some("addDirectories") | Some("removeDirectories") => {
                // PARITY `Oc(ctx,{type:"addDirectories",directories,destination})`:
                // the destination is the SOURCE recorded on each entry, not just a
                // validity check — `mEt` later drops the `projectSettings` ones.
                let Some(update_source) = Self::parse_update_destination(
                    update.get("destination").unwrap_or(&Value::Null),
                ) else {
                    return;
                };
                let Some(directories) = update.get("directories").and_then(Value::as_array) else {
                    return;
                };
                let Some(directories) = directories
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
                else {
                    return;
                };
                // `PathBuf::from` accepts an embedded NUL on Unix, but the
                // resulting path cannot be passed to filesystem APIs. Claude
                // rejects the entire union member before applying any entry;
                // preflight the complete array so a valid prefix cannot leak
                // into the live session.
                if let Some(directory) = directories.iter().find(|dir| dir.contains('\0')) {
                    let update_type = update
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("addDirectories");
                    tracing::debug!(
                        "{}",
                        directory_update_null_byte_reason(update_type, directory)
                    );
                    return;
                }
                let mut live = self.live_state.write().unwrap_or_else(|e| e.into_inner());
                match update.get("type").and_then(Value::as_str) {
                    Some("addDirectories") => {
                        for dir in directories {
                            live.additional_working_dirs
                                .insert(PathBuf::from(dir), update_source);
                        }
                    }
                    Some("removeDirectories") => {
                        for dir in directories {
                            live.additional_working_dirs.remove(&PathBuf::from(dir));
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                // Unknown update type: ignored, matching the JS reducer.
            }
        }
    }

    /// Fold [`Self::apply_permission_update`] over a host `updatedPermissions`
    /// array (claude-code `bJ`), applying each entry to the live session.
    pub fn apply_permission_updates(&self, updates: &[Value]) {
        for update in updates {
            self.apply_permission_update(update);
        }
    }
}

/// Whether the read-only / agent-local default-allow stand-in applies to an
/// `Ask`. It does ONLY when the tool is [`PromptDefault::AllowByDefault`] AND the
/// ask was NOT produced by an explicit `ask` rule. claude-code's read-only
/// default-allow (`checkPermissions`' allow verdict) is reached only AFTER the
/// ask-rule walk (`mSm` steps 1c/1d precede the per-tool allow), so an explicit
/// `ask:["Read(...)"]` / `ask:["Glob"]` rule — tool-wide or content, tagged
/// [`PermissionDecisionReason::MatchedRule`] — PRE-EMPTS it and forces the prompt
/// (firing the `PermissionRequest` hook). A mode-fallback ask
/// ([`PermissionDecisionReason::PermissionMode`]) keeps the frictionless
/// read-only auto-allow so the common no-rule case never ask-storms.
fn read_only_default_auto_allows(
    name: &str,
    reason: &PermissionDecisionReason,
    mode: PermissionMode,
) -> bool {
    // An undecided Auto-mode subagent needs contextual approval. Its legacy
    // AllowByDefault entry must not turn classifier Pass into an approval when
    // the contextual classifier is unavailable or itself cannot decide.
    if mode == PermissionMode::Auto && matches!(name, "Agent" | "Task") {
        return false;
    }
    // LINGXI DIVERGENCE, narrowly scoped to the rows that have no oracle
    // counterpart (`defaults_per_tool::is_divergence_tool`: the `LocalApp*`
    // family plus `Workflow`).
    //
    // `AllowByDefault` short-circuits the Plan-mode backstop, so a mutating
    // tool that is auto-allowed for convenience would RUN while the user
    // believes they are only planning — `LocalAppBuild` starts a 30-minute
    // build, `LocalAppRuntime {action:"open"}` puts an app on screen, and
    // `Workflow` fans out a whole crew of side-effecting agents
    // (`WorkflowTool::is_read_only` answers `false`). `PLAN_SAFE_TOOLS` is the
    // existing statement of what may run in Plan mode, and none of these are
    // in it.
    //
    // Deliberately NOT applied to the oracle tools: several of them are
    // `AllowByDefault` without being plan-safe, and changing that would be a
    // parity change rather than a fix. A divergence row carries no such
    // constraint — its default is ours to set — so it fails closed here.
    if mode == PermissionMode::Plan
        && crate::defaults_per_tool::is_divergence_tool(name)
        && !crate::mode_policy::is_plan_safe_tool(name)
    {
        return false;
    }
    matches!(tool_default(name), PromptDefault::AllowByDefault)
        && !matches!(reason, PermissionDecisionReason::MatchedRule { .. })
}

/// Upstream `MWn = /^(?:Read|Edit)\((?:\.\/)?!/` — a `Read(!…)` / `Edit(!…)`
/// rule spec, with an optional `./` before the `!`. Such a spec is a gitignore
/// NEGATION (see [`permission::filesystem::RulePatternMatch`]), which cancels
/// deny rules in its own source instead of adding one.
fn is_negated_path_rule(spec: &str) -> bool {
    for tool in ["Read(", "Edit("] {
        if let Some(rest) = spec.strip_prefix(tool) {
            let rest = rest.strip_prefix("./").unwrap_or(rest);
            if rest.starts_with('!') {
                return true;
            }
        }
    }
    false
}

#[async_trait]
impl PermissionGate for PolicyPermissionGate {
    /// `Tbt(setToolPermissionContext, specs, mode)` — see the trait doc for why
    /// the two modes differ.
    fn set_command_input_denies(&self, specs: &[String], union: bool) {
        let mut cell = self
            .command_input_denies
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if union {
            // `Y([...(existing ?? []), ...specs.filter(s => !MWn.test(s))])`:
            // append, dedup, and REFUSE a `!`-negated Read/Edit path rule. That
            // filter is the load-bearing half — a negation cancels deny rules in
            // its own source, so a skill unioning one in would turn
            // "deny these tools" into "undo the denies already standing".
            for spec in specs {
                if is_negated_path_rule(spec) {
                    tracing::warn!(
                        target: "permission",
                        spec = %spec,
                        "a skill's disallowed-tools may not negate a path deny rule; dropping it"
                    );
                    continue;
                }
                if !cell.iter().any(|existing| existing == spec) {
                    cell.push(spec.clone());
                }
            }
        } else {
            // REPLACE, verbatim and unfiltered, exactly as upstream's default
            // mode is: it overwrites the whole command source rather than adding
            // to it, so a negation here can only cancel rules this same input
            // put there.
            *cell = specs.to_vec();
        }
    }

    fn can_request_auto_mode(&self) -> bool {
        self.auto_mode_denial_reason().is_none()
    }

    fn can_request_bypass_permissions(&self) -> bool {
        !self.policy.restricted && !self.policy.bypass_killswitch_active
    }

    fn confirm_bypass_permissions(&self) -> Result<(), String> {
        if self.policy.restricted {
            return Err("bypassPermissions not supported in restricted mode".to_string());
        }
        if self.policy.bypass_killswitch_active {
            return Err(
                "Cannot set permission mode to bypassPermissions because it is disabled by settings or configuration"
                    .to_string(),
            );
        }
        self.bypass_permissions_confirmed
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(())
    }

    fn read_deny_exclude_globs(&self, cwd: &std::path::Path) -> Option<Vec<String>> {
        // GUEST-COORD (mobile DIVERGENCE), the ENUMERATION half of the fix.
        // `Grep`/`Glob` pass their OWN resolved search base, which on mobile is
        // a guest path. A ROOTED deny pattern (`Read(/secrets/**)`) is rebased
        // onto that base by `relativize_for_cwd`; relativizing a host pattern
        // root against a guest base escapes upward, returns `None`, and the
        // walk proceeds with ZERO exclusions straight through a Read-denied
        // directory. Translate to the host twin first, exactly as the authorize
        // path does. (Unrooted patterns are cwd-independent and unaffected.)
        //
        // FAIL CLOSED on a fenced region. `to_host` collapses "not a guest
        // path" and "must not be touched" into one `None`, and falling back to
        // the untranslated base for the second case yields ZERO exclusions —
        // indistinguishable from "this policy has no read-deny rules", so the
        // walk proceeds straight through a denied directory. Returning `None`
        // withholds the exclusion list instead of asserting an empty one.
        let base = match self.path_translator.as_ref() {
            None => cwd.to_path_buf(),
            Some(translator) => match translator.translate(&cwd.to_string_lossy(), false) {
                crate::ModelPathOutcome::Host(host) => std::path::PathBuf::from(host),
                crate::ModelPathOutcome::NotGuest => cwd.to_path_buf(),
                crate::ModelPathOutcome::Fenced => return None,
            },
        };
        Some(crate::read_deny_exclude_globs(&self.live_policy(), &base))
    }

    fn check_noninteractive_with_allow_rules(
        &self,
        name: &str,
        input: &Value,
        transient_allow_rules: &[String],
    ) -> Option<platform_api::permission_gate::NonInteractivePermissionDecision> {
        use platform_api::permission_gate::NonInteractivePermissionDecision;

        let mode = self.effective_mode_for_tool(name);
        let mut live = self
            .live_state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let command_rules = live
            .allow_rules
            .entry(PermissionRuleSource::Command)
            .or_default();
        command_rules.extend(transient_allow_rules.iter().map(|spec| PermissionRule {
            value: PermissionRuleValue::from_rule_string(spec),
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Command,
        }));
        let mut deny_rules = live.deny_rules;
        self.fold_command_input_denies(&mut deny_rules);
        let policy = self.policy.clone_with_live_state(
            live.allow_rules,
            deny_rules,
            live.ask_rules,
            live.additional_working_dirs,
        );
        Some(match policy.authorize_with_mode(name, input, mode) {
            PermissionResult::Allow { .. } => NonInteractivePermissionDecision::Allow,
            PermissionResult::Deny { explanation, .. } => NonInteractivePermissionDecision::Deny {
                reason: explanation,
            },
            PermissionResult::Ask { prompt, .. } => NonInteractivePermissionDecision::Deny {
                reason: Some(prompt.message),
            },
        })
    }

    async fn check(&self, name: &str, input: &Value) -> PermissionDecision {
        // Authorize under the LIVE mode (boot mode or a set_permission_mode
        // override), then map the 3-valued result (an `Ask` auto-allows read-only
        // tools or delegates to the prompt).
        let (mode, result) = self.effective_authorize(name, input);
        self.decide(mode, result, name, input).await
    }

    /// As [`Self::check`], but forwards the originating subagent/teammate
    /// worker identity to the inner prompt transport when an `Ask` is delegated,
    /// so the dialog attributes the request to that worker. The rule/mode
    /// decision is unchanged — only the prompt presentation gains attribution.
    async fn check_with_worker(
        &self,
        name: &str,
        input: &Value,
        worker: Option<crate::gate::PromptWorker>,
    ) -> PermissionDecision {
        let (mode, result) = self.effective_authorize(name, input);
        self.decide_with_worker(mode, result, name, input, worker)
            .await
    }

    /// As [`Self::check_with_worker`], but returns a [`PermissionOutcome`] that
    /// can carry the host/policy `updatedInput` rewrite and forwards the full
    /// context (real `tool_use_id`) to the inner transport on the Ask path.
    async fn check_with_context(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        match self.check_with_context_or_abort(name, input, ctx).await {
            Ok(outcome) => outcome,
            Err(abort) => PermissionOutcome::Deny {
                reason: abort.message,
            },
        }
    }

    async fn check_exit_plan_mode(
        &self,
        plan: &str,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        // ExitPlanMode owns its approval UI and carries the plan body as a
        // first-class request.  Do not route it through the ordinary policy
        // authorize path: ExitPlanMode is AllowByDefault there, which would
        // skip the approval dialog entirely.
        //
        // BypassPermissions does NOT skip this dialog. Every other gate here is
        // a SAFETY gate, and "Full access" is the user saying they accept the
        // risk of an action; the plan dialog is not that. It is the one point
        // where the user reads the plan and says go — a product decision, not a
        // risk decision — so suppressing it in Full access left the user with a
        // transcript that announced "User has approved the plan" for a plan they
        // were never shown.
        if ctx.is_non_interactive_session {
            // Headless has no dialog to show, so the mode still decides:
            // BypassPermissions means "never block me" and keeps its allow;
            // every other mode fails closed exactly as before.
            if self.effective_mode_for_tool("ExitPlanMode") == PermissionMode::BypassPermissions {
                return PermissionOutcome::Allow {
                    updated_input: None,
                    permission_updates: Vec::new(),
                    decision_classification: None,
                };
            }
            return PermissionOutcome::Deny {
                reason: headless_deny_message("ExitPlanMode"),
            };
        }
        let input = serde_json::json!({ "plan": plan });
        let mut ctx2 = ctx.clone();
        ctx2.auto_mode_prompt = self.auto_prompt_for_exit_plan(ctx);
        self.consume_auto_outcome(
            self.inner
                .check_with_context("ExitPlanMode", &input, &ctx2)
                .await,
            &ctx2,
        )
        .await
    }

    /// BASH-10 substrate — resolve a TOOL-originated ask straight through the
    /// inner prompt transport.
    ///
    /// The default trait impl delegates to [`Self::check_with_context`], which
    /// for THIS gate would re-run `authorize_with_layers` and re-derive the very
    /// allow the tool's own `check_permissions` just escalated — silently
    /// defeating the escalation. Override it to do what
    /// `decide_outcome_with_context`'s Ask arm does after it has decided to
    /// prompt: hand the call to `self.inner` (the transport) and keep the
    /// auto-mode / `updatedPermissions` bookkeeping every allow arm records.
    ///
    /// The classifier and `read_only_default_auto_allows` short-circuits are
    /// DELIBERATELY not re-run here: the oracle's protected-reason arm
    /// (`decisionReason?.type === "sandboxOverride"`) returns the ask ahead of
    /// every auto-allow arm in `a6e`, so a classifier must not be able to
    /// re-allow it.
    async fn ask_via_transport(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        let outcome = self
            .consume_auto_outcome(self.check_prompt_transport(name, input, ctx).await, ctx)
            .await;
        match &outcome {
            PermissionOutcome::Allow {
                permission_updates, ..
            } => {
                self.record_auto_mode_non_deny(self.effective_mode_for_tool(name));
                if !permission_updates.is_empty() {
                    PolicyPermissionGate::apply_permission_updates(self, permission_updates);
                }
            }
            PermissionOutcome::AllowAuto { .. } => {
                self.record_auto_mode_non_deny(self.effective_mode_for_tool(name));
            }
            PermissionOutcome::Deny { .. } => {}
        }
        outcome
    }

    /// `EZe` — review a finished subagent's work before its parent reads it.
    ///
    /// ```js
    /// if(r.mode!=="auto"||M==="send"||M==="flagged")return null;
    /// if(!x$n(e,n)&&!S?.trim())return null;
    /// let U=await bke(e,A$n(S,…),n,r,s,{isSubagentLoop:!0,…});
    /// if(U.shouldBlock){ refused → …; unavailable → kae(…); else → flagged } return null
    /// ```
    ///
    /// The `handback` kinds (`send`/`flagged`/`withheld`) have no port
    /// counterpart, so only the mode gate and the nothing-to-review gate are
    /// applied. Whatever the review concludes, the work is still DELIVERED:
    /// this returns copy to prepend, never a denial.
    async fn review_subagent_handoff(
        &self,
        transcript: Option<&std::path::Path>,
        final_text: &str,
    ) -> Option<HandoffReview> {
        if self.effective_mode() != PermissionMode::Auto {
            return None;
        }
        // `!x$n(e,n) && !S?.trim()` — nothing to review.
        if transcript.is_none() && final_text.trim().is_empty() {
            return None;
        }
        let classifier = self.loop_classifier.get()?;
        match classifier.classify_handoff(transcript, final_text).await {
            AutoModeClassifierVerdict::Deny { reason, .. } => {
                Some(HandoffReview::Flagged { reason })
            }
            AutoModeClassifierVerdict::NoVerdict { reason, .. } => {
                if reason == crate::loop_llm::REFUSED {
                    Some(HandoffReview::Refused)
                } else {
                    // `kae("")` — the arm upstream's own `.catch` sites take.
                    // The verdict does not carry the model or the failure kind
                    // back, so the parameterised arm is not reachable here; the
                    // rendered copy is upstream's, for the no-model case.
                    Some(HandoffReview::Unavailable {
                        model: String::new(),
                        detail: String::new(),
                    })
                }
            }
            // A transcript too long to classify is not a finding about the
            // subagent; `EZe` has no arm for it and returns null.
            AutoModeClassifierVerdict::Allow { .. }
            | AutoModeClassifierVerdict::Pass { .. }
            | AutoModeClassifierVerdict::TranscriptTooLong => None,
        }
    }

    async fn check_with_context_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionOutcome, PermissionAbort> {
        // PER-CALL PERMISSION LAYERS — `gn(toolUseContext)` (binary @287028951).
        // This is the fold's PRIMARY call site: every subagent / teammate /
        // local-app tool dispatch reaches the gate here, so a spawn's
        // `permission_mode` (its clamped spawn mode), `avoid_prompts`,
        // `allowed_tools` / `disallowed_tools`, `working_directory` and
        // `bash_command_clamp` layers all take effect on THIS call without
        // mutating the shared boot policy (the parent's own checks are
        // unaffected). An empty stack folds to nothing and the path below is
        // byte-identical to the pre-layer behavior.
        let folded = self.fold_call_context(ctx);
        let mode = folded
            .mode
            .unwrap_or_else(|| self.effective_mode_for_tool(name));
        let result =
            self.authorize_with_layers(name, input, mode, ctx.workspace_lease_token, &folded);
        self.decide_outcome_with_context(mode, result, name, input, ctx, &|| {
            self.accept_edits_fast_path(name, input, &folded, ctx.workspace_lease_token)
        })
        .await
    }

    fn apply_permission_updates(&self, updates: &[Value]) {
        PolicyPermissionGate::apply_permission_updates(self, updates);
    }

    async fn persist_permission_updates(&self, updates: &[Value]) {
        if !self.policy.allow_managed_permission_rules_only {
            self.inner.persist_permission_updates(updates).await;
        }
    }

    fn set_permission_persistence_enabled(&self, enabled: bool) {
        self.inner.set_permission_persistence_enabled(
            enabled && !self.policy.allow_managed_permission_rules_only,
        );
    }

    fn is_restricted_protected_mutation(&self, name: &str, input: &Value) -> bool {
        self.policy.is_restricted_protected_mutation(name, input)
    }

    /// GATE-SYSMSG-01: forward a deny notification to the inner transport (the
    /// only layer with an outbound stream). The main turn loop calls this on the
    /// OUTER gate for a main-conversation deny (which resolves via
    /// `resolve_detailed`, not `check_with_context`), so the stdio control-plane
    /// still emits the `permission_denied` system message; the subagent-dispatch
    /// path emits directly from `decide_outcome_with_context`.
    async fn on_permission_denied(
        &self,
        name: &str,
        ctx: &PermissionCheckContext,
        decision_reason_type: Option<&str>,
        decision_reason: Option<&str>,
        message: &str,
    ) {
        self.inner
            .on_permission_denied(name, ctx, decision_reason_type, decision_reason, message)
            .await;
    }

    /// A PreToolUse / PermissionRequest hook `allow` skips the PROMPT but still
    /// applies rule-based deny/ask (claude-code `resolveHookPermissionDecision` +
    /// `checkRuleBasedPermissions`): a hook cannot override an explicit deny
    /// rule or the active mode's mutation backstop. So run `authorize` and map
    /// `Deny → Deny` (deny rules + mode still bind) but `Ask → Allow` (the hook
    /// approved, so the would-be prompt is skipped) and `Allow → Allow`. Unlike
    /// `check`, an `Ask` NEVER delegates to the inner prompt transport here — the
    /// hook already resolved the prompt.
    /// claude-code `lin` — resolve a **PreToolUse** hook `allow`.
    ///
    /// `lin` re-runs the rule/safety check ([`Self::rule_or_safety_verdict`],
    /// the `_pt` analog) UNCONDITIONALLY — there is no "only if the hook rewrote
    /// the input" gate — and then:
    ///
    /// * `deny` ⇒ the deny rule OVERRIDES the hook
    ///   (`"…but deny rule overrides: ${u.message}"`);
    /// * `ask`  ⇒ the call goes to the FULL permission pipeline
    ///   (`"…but ask rule/safety check requires full permission pipeline"`), i.e.
    ///   it PROMPTS — headless resolves that to a deny via the inner gate;
    /// * no verdict ⇒ the hook's allow stands, prompt skipped.
    ///
    /// The third arm is why the mode layer must be subtracted: an ordinary
    /// Default-mode mutating call has no rule verdict at all, so the hook allow
    /// is honoured. Feeding the mode-backstop ask in here instead would deny
    /// almost every hook-rescued call.
    async fn check_after_hook_allow(&self, name: &str, input: &Value) -> PermissionDecision {
        Self::flatten_permission_outcome(
            self.check_after_hook_allow_impl(name, input, &PermissionCheckContext::default())
                .await,
        )
    }

    async fn check_after_hook_allow_ctx(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionDecision {
        Self::flatten_permission_outcome(self.check_after_hook_allow_impl(name, input, ctx).await)
    }

    async fn check_after_hook_allow_outcome_ctx(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> PermissionOutcome {
        self.check_after_hook_allow_impl(name, input, ctx).await
    }

    /// claude-code `Fxy` + `epr` — resolve a **PermissionRequest** (headless
    /// rescue) hook `allow` that came with a REWRITTEN input.
    ///
    /// Same `_pt` re-check, but an `ask` becomes a **hard deny** carrying the
    /// ASK's own message (`{behavior:"deny", message:c.message, …}`) — that
    /// surface has no prompt to fall back to, and the hook already consumed the
    /// one opportunity to resolve it. Without this a hook could rewrite a tool's
    /// arguments into something an ask rule covers and have it run unprompted.
    async fn check_after_hook_allow_rewritten(
        &self,
        name: &str,
        input: &Value,
    ) -> PermissionDecision {
        let (mode, verdict) =
            self.rule_or_safety_verdict(name, input, None, &FoldedPermissionContext::default());
        match verdict {
            Some(PermissionResult::Deny {
                reason,
                explanation,
                ..
            }) => {
                let msg = explanation.unwrap_or_else(|| deny_reason_string(&reason, name));
                tracing::warn!(
                    target: "permission",
                    "PermissionRequest hook allowed {name} with updatedInput, but deny rule overrides: {msg}"
                );
                PermissionDecision::Deny { reason: msg }
            }
            Some(PermissionResult::Ask { prompt, .. }) => {
                tracing::warn!(
                    target: "permission",
                    "PermissionRequest hook allowed {name} with updatedInput, but ask rule overrides: {}",
                    prompt.message
                );
                // `c.behavior==="ask" ? {behavior:"deny", message:c.message, …}`
                PermissionDecision::Deny {
                    reason: prompt.message,
                }
            }
            _ => {
                self.record_auto_mode_non_deny(mode);
                PermissionDecision::Allow
            }
        }
    }

    /// claude-code `Fxy` — the STANDING PermissionRequest-hook `allow` (no
    /// `updatedInput`, tool does not `requiresUserInteraction`). The oracle SKIPS
    /// the `_pt` re-check entirely and returns the allow, so we must NOT run
    /// [`Self::rule_or_safety_verdict`]: an ask rule (the reason the gate resolved
    /// `Ask` in the first place) would otherwise re-prompt / hard-deny and defeat
    /// the rescue. A matching deny rule cannot reach here — it resolves `Deny`
    /// before the PermissionRequest hook fires — so honouring the unchanged input
    /// is safe. We keep only the mode-less auto-mode non-deny bookkeeping every
    /// allow arm records (reset of the classifier breaker's consecutive-denial
    /// counter), matching the pre-HOOKALLOW-01 behavior for this arm.
    async fn honour_hook_allow(&self, name: &str, input: &Value) -> PermissionDecision {
        if self.policy.is_restricted_protected_mutation(name, input) {
            return PermissionDecision::Deny {
                reason: "Restricted mode requires a person or configured permission handler to approve writes to settings, git, and tool-configuration files".to_string(),
            };
        }
        self.record_auto_mode_non_deny(self.effective_mode_for_tool(name));
        PermissionDecision::Allow
    }

    /// In PLAN mode the gate authorizes under [`PermissionMode::Plan`] regardless
    /// of the policy's boot mode, so a runtime `EnterPlanMode` dynamically
    /// activates the mutation backstop: a plan-safe READ-ONLY tool falls through
    /// to the read-only auto-allow (no prompt), while a mutating tool that matched
    /// no allow rule trips the backstop → `Ask` → DELEGATES to the inner prompt
    /// transport (an interactive prompt, or a headless deny). Deny rules and
    /// explicit allow rules still bind — they are resolved before the mode layer
    /// inside `authorize_with_mode`. The `Ask` mapping is shared with
    /// [`Self::check`] via [`Self::decide`].
    async fn check_in_plan_mode(&self, name: &str, input: &Value) -> PermissionDecision {
        if self.policy.is_restricted_protected_mutation(name, input) {
            let ctx = PermissionCheckContext {
                suppress_always_allow_rule: true,
                ..PermissionCheckContext::default()
            };
            return match self.inner.check_with_context(name, input, &ctx).await {
                PermissionOutcome::Allow { .. } | PermissionOutcome::AllowAuto { .. } => {
                    PermissionDecision::Allow
                }
                PermissionOutcome::Deny { reason } => PermissionDecision::Deny { reason },
            };
        }
        self.decide(
            PermissionMode::Plan,
            self.authorize_with_live_state(name, input, PermissionMode::Plan, None),
            name,
            input,
        )
        .await
    }

    async fn resolve_detailed_in_plan_mode_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionResolution, PermissionAbort> {
        let folded = self.fold_call_context(ctx);
        let result = self.authorize_with_layers(
            name,
            input,
            PermissionMode::Plan,
            ctx.workspace_lease_token,
            &folded,
        );
        self.resolve_with_mode(
            PermissionMode::Plan,
            result,
            name,
            input,
            ctx.is_non_interactive_session || folded.should_avoid_permission_prompts,
            // `dKo` guards the acceptEdits simulation with `!zn`, i.e. NOT in
            // plan mode. This surface is plan mode by construction.
            &|| false,
        )
        .await
    }

    async fn resolve_detailed(&self, name: &str, input: &Value) -> PermissionResolution {
        // Authorize under the LIVE mode WITHOUT delegating to the inner prompt,
        // so the turn loop can read the decision source (and an about-to-ask) and
        // fire PermissionRequest / PermissionDenied before the prompt resolves.
        let (mode, result) = self.effective_authorize(name, input);
        match self
            .resolve_with_mode(mode, result, name, input, false, &|| {
                self.accept_edits_fast_path(name, input, &FoldedPermissionContext::default(), None)
            })
            .await
        {
            Ok(resolution) => resolution,
            Err(abort) => PermissionResolution::Deny {
                reason: abort.message,
                source: PermissionDecisionSource::Unspecified,
                rule_source: None,
                decision_reason_type: None,
                decision_reason: None,
                behavior_ask: false,
                content_blocks: Vec::new(),
            },
        }
    }

    async fn resolve_detailed_or_abort(
        &self,
        name: &str,
        input: &Value,
        ctx: &PermissionCheckContext,
    ) -> Result<PermissionResolution, PermissionAbort> {
        // Same per-call layer fold as `check_with_context_or_abort` — the main
        // turn loop resolves through THIS surface, so a layer stack attached to
        // a dispatch is honoured on both the source-first and the outcome path.
        let folded = self.fold_call_context(ctx);
        let mode = folded
            .mode
            .unwrap_or_else(|| self.effective_mode_for_tool(name));
        let result =
            self.authorize_with_layers(name, input, mode, ctx.workspace_lease_token, &folded);
        // An `avoid_prompts` LAYER is the per-call twin of the flat
        // `is_non_interactive_session` field (`shouldAvoidPermissionPrompts`).
        self.resolve_with_mode(
            mode,
            result,
            name,
            input,
            ctx.is_non_interactive_session || folded.should_avoid_permission_prompts,
            &|| self.accept_edits_fast_path(name, input, &folded, ctx.workspace_lease_token),
        )
        .await
    }

    /// Surface the wrapped policy's TOOL-WIDE deny-rule names so the orchestrator
    /// strips blanket-denied tools from the wire `tools` array before the model
    /// sees them (claude-code `filterToolsByDenyRules`). Content deny rules are
    /// excluded by [`PermissionPolicy::tool_wide_deny_names`] (they deny calls,
    /// not the tool). With zero deny rules this is empty ⇒ no tools stripped.
    async fn tool_wide_deny_names(&self) -> Vec<String> {
        self.live_policy().tool_wide_deny_names()
    }

    /// Surface the source of a matching `Agent(<type>)` deny rule so the Agent
    /// tool can reject a denied subagent type with the byte-exact
    /// `AgentTypeError` message (claude-code `getDenyRuleForAgent`). The source is
    /// rendered as the raw `SettingSource` identifier
    /// ([`PermissionRuleSource::lingxi_settings_source`]), matching the binary's
    /// `… from ${rule.source}.`.
    async fn agent_type_deny(&self, agent_type: &str) -> Option<String> {
        self.live_policy()
            .agent_type_deny_source(agent_type)
            .map(|s| s.lingxi_settings_source().to_string())
    }

    /// Surface the wrapped policy's CONTENT-ful `Agent(<x>)` deny set so the
    /// advertised agent catalog and `Available agents:` lists exclude denied
    /// types (claude-code `Pxe`).
    async fn agent_deny_content_types(&self) -> Vec<String> {
        self.live_policy().agent_deny_content_types()
    }

    /// Apply a LIVE `set_permission_mode` override (claude-code
    /// `handleSetPermissionMode`), byte-faithful to the binary's gating:
    ///
    /// - `bypassPermissions` is gated by TWO ordered checks: first the
    ///   disabled-by-settings killswitch, then the launch-flag availability
    ///   (`isBypassPermissionsModeAvailable` / `bypass_permissions_available`) —
    ///   each with its byte-exact error string. So a session NOT launched with
    ///   `--dangerously-skip-permissions` cannot switch live into bypass.
    /// - an UNKNOWN mode is NOT an error: the binary reads the mode raw, acks
    ///   `{mode}`, and `transitionPermissionMode` no-ops an unrecognized mode
    ///   (classifier off), so we ack WITHOUT changing the live mode.
    /// - `auto` is gated by claude-code 2.1.207's `Nle`
    ///   (`setPermissionModeWithGuards`): `if(e==="auto"&&!P0()){...error:
    ///   \`Cannot set permission mode to auto: ${Jce(One())}\`}`. We evaluate the
    ///   three runtime-available `P0()` inputs in `One()`'s precedence — the
    ///   `disableAutoMode` settings killswitch
    ///   ([`crate::PermissionPolicy::auto_mode_disabled`]), the local denial
    ///   circuit-breaker, then the model gate (`dUe(wi())`) against the LIVE
    ///   session model (via [`Self::live_model_provider_handle`]) — and reject
    ///   with the byte-exact message for the
    ///   [`crate::auto_gate::AutoGateDenialReason::Settings`] /
    ///   [`crate::auto_gate::AutoGateDenialReason::CircuitBreaker`] /
    ///   [`crate::auto_gate::AutoGateDenialReason::Model`] cases. So a live
    ///   runtime switch to `auto` after a `/model` to an auto-unsupported model
    ///   is now rejected here (`auto mode unavailable for this model`), matching
    ///   the binary — not only downgraded at boot.
    ///
    ///   When the live model/provider cell is unset (headless /
    ///   pre-orchestrator), the model check is skipped and the boot downgrade
    ///   remains the authoritative enforcement.
    async fn set_permission_mode(&self, mode: &str) -> Result<(), String> {
        let Some(parsed) = parse_settable_mode(mode) else {
            // Unknown mode: accept + ack, but do not mutate the live mode.
            return Ok(());
        };
        if parsed == PermissionMode::BypassPermissions {
            if self.policy.restricted {
                return Err("bypassPermissions not supported in restricted mode".to_string());
            }
            if self.policy.bypass_killswitch_active {
                return Err("Cannot set permission mode to bypassPermissions because it is disabled by settings or configuration".to_string());
            }
            if !self.policy.bypass_permissions_available
                && !self
                    .bypass_permissions_confirmed
                    .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                return Err("Cannot set permission mode to bypassPermissions because the session was not launched with --dangerously-skip-permissions".to_string());
            }
        }
        // Hold the live-mode lock while checking Auto availability and writing
        // the new mode. This makes the user-selected transition one atomic
        // decision with the managed-settings eviction path.
        let mut mode_override = self
            .mode_override
            .write()
            .unwrap_or_else(|e| e.into_inner());
        if parsed == PermissionMode::Auto {
            // `Nle`: reject `auto` when `!P0()`. Report `One()`'s reason in the
            // binary's precedence (settings → circuit-breaker → model), rendering
            // the byte-exact `Cannot set permission mode to auto: <Jce(reason)>`.
            if let Some(reason) = self.auto_mode_denial_reason() {
                return Err(crate::auto_gate::cannot_set_auto_message(reason));
            }
        }
        // Re-check the managed kill switch while holding the same mode lock
        // used by `update_auto_mode_disabled_from_settings`. This closes the
        // check-then-store race where a settings update could otherwise land
        // between `auto_mode_denial_reason()` and this write and resurrect
        // Auto after the eviction.
        if parsed == PermissionMode::Auto
            && self
                .auto_mode_disabled_from_settings
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(crate::auto_gate::cannot_set_auto_message(
                crate::auto_gate::AutoGateDenialReason::Settings,
            ));
        }
        *mode_override = Some(parsed);
        Ok(())
    }

    fn update_auto_mode_disabled(&self, disabled: bool) {
        self.update_auto_mode_disabled_from_settings(disabled);
    }

    async fn set_mcp_permission_mode_override(
        &self,
        server_name: &str,
        mode: Option<&str>,
    ) -> Result<(), String> {
        let normalized = protocol::normalize_name_for_mcp(server_name);
        let mut overrides = self
            .mcp_mode_overrides
            .write()
            .unwrap_or_else(|e| e.into_inner());
        match mode {
            None => {
                overrides.remove(&normalized);
                Ok(())
            }
            Some("default") => {
                overrides.insert(normalized, PermissionMode::Default);
                Ok(())
            }
            Some("auto") => {
                // `set_mcp_permission_mode_override` applies the same `_k()` /
                // `Zse()` availability gate as the session-wide auto switch.
                // A disabled auto mode must not be reintroduced through a
                // server pin; the control response uses this byte-exact text.
                if let Some(reason) = self.auto_mode_denial_reason() {
                    return Err(format!(
                        "Cannot pin MCP server '{server_name}' to auto: {}",
                        reason.message()
                    ));
                }
                // The settings watcher and this control path synchronize via
                // the atomic kill switch plus this override lock. Re-check
                // after acquiring the lock so a concurrent managed disable
                // cannot be followed by a stale Auto insertion.
                if self
                    .auto_mode_disabled_from_settings
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    return Err(format!(
                        "Cannot pin MCP server '{server_name}' to auto: auto mode disabled by settings"
                    ));
                }
                overrides.insert(normalized, PermissionMode::Auto);
                Ok(())
            }
            Some(other) => Err(format!(
                "Permission mode override over the control channel is tighten-only ('default', 'auto', or null); rejected '{other}'"
            )),
        }
    }

    /// The LIVE effective mode as a wire string — the override when set, else the
    /// boot mode ([`Self::effective_mode`]). Lets the in-process `/resume`
    /// re-mount snapshot and restore the user's mid-session Shift+Tab mode.
    fn permission_mode(&self) -> Option<String> {
        Some(self.effective_mode().wire_str().to_string())
    }
}

/// Parse a `set_permission_mode` wire string into a [`PermissionMode`].
///
/// Accepts the five external modes plus the internal `auto` (the binary's
/// settable set is `default`/`plan`/`acceptEdits`/`bypassPermissions`/`dontAsk`/
/// `auto`), with the 2.1.211 `manual` alias (`LE`/`PERMISSION_MODE_MANUAL_ALIAS`)
/// normalized to `default` — applied in both the wire schema `preprocess` and the
/// `-p` engine handler. Returns `None` for an unrecognized mode — the binary
/// accepts any string and no-ops an unknown one (it does NOT error), so the
/// caller acks without mutating rather than returning an error frame.
fn parse_settable_mode(s: &str) -> Option<PermissionMode> {
    match s {
        "default" | "manual" => Some(PermissionMode::Default),
        "plan" => Some(PermissionMode::Plan),
        "acceptEdits" => Some(PermissionMode::AcceptEdits),
        "bypassPermissions" => Some(PermissionMode::BypassPermissions),
        "dontAsk" => Some(PermissionMode::DontAsk),
        "auto" => Some(PermissionMode::Auto),
        _ => None,
    }
}

fn mcp_server_token(tool_name: &str) -> Option<&str> {
    let mut parts = tool_name.splitn(3, "__");
    let Some("mcp") = parts.next() else {
        return None;
    };
    parts.next().filter(|server| !server.is_empty())
}

/// Render a [`PermissionDecisionReason`] to the model-facing deny string for
/// `tool_name` (used when the `Deny` carries no explicit `explanation`).
///
/// claude-code's generic permission deny message is
/// `Permission to use ${tool} has been denied.` (`permissions.ts:1087,1179`);
/// the `dontAsk` mode appends the don't-ask clause + the shared workaround
/// guidance (`DONT_ASK_REJECT_MESSAGE`, `messages.ts:237`). Tool-specific deny
/// checks (e.g. Bash's `Permission to use Bash with command ${cmd} has been
/// denied.`) set an explicit `explanation` that takes precedence over this.
fn deny_reason_string(reason: &PermissionDecisionReason, tool_name: &str) -> String {
    match reason {
        PermissionDecisionReason::PermissionMode {
            mode: PermissionMode::DontAsk,
        } => format!(
            "Permission to use {tool_name} has been denied because LingXi is running in don't ask mode. {DENIAL_WORKAROUND_GUIDANCE}"
        ),
        _ => format!("Permission to use {tool_name} has been denied."),
    }
}

/// Shared guidance appended to certain permission denials (`messages.ts:226`),
/// instructing the model on acceptable workarounds.
///
/// This is claude-code's `Rws` (`lDd` + the "If you believe this capability is
/// essential…" suffix). `GRu(tool)` — the headless deny message — is
/// `` `Permission to use ${tool} has been denied. ${Rws}` `` (see
/// [`crate::headless_gate::DenyOnAskGate`]).
pub(crate) const DENIAL_WORKAROUND_GUIDANCE: &str = "IMPORTANT: You *may* attempt to accomplish this action using other tools that might naturally be used to accomplish this goal, e.g. using head instead of cat. But you *should not* attempt to work around this denial in malicious ways, e.g. do not use your ability to run tests to execute non-test actions. You should only try to work around this restriction in reasonable ways that do not attempt to bypass the intent behind this denial. If you believe this capability is essential to complete the user's request, STOP and explain to the user what you were trying to do and why you need this permission. Let the user decide how to proceed.";

/// The `asyncAgent` decision reason claude-code attaches to `DJa`'s deny —
/// byte-identical in 2.1.220 (`G8s`) and 2.1.238 (`DJa`).
pub(crate) const PROMPTS_UNAVAILABLE_ASYNC_AGENT_REASON: &str =
    "Action requires interactive approval and permission prompts are not available in this context";

/// PERM-07 — claude-code 2.1.238 `Ixf`: the model-visible copy that now WRAPS
/// the ask message when an action needs interactive approval in a session that
/// cannot prompt.
///
/// 2.1.220's `G8s(e)` forwarded the raw ask message as the deny message; 2.1.238
/// splits the builder out as
/// `` DJa(e) => {behavior:"deny", message: Ixf(e), decisionReason:{type:"asyncAgent", reason: `` [`PROMPTS_UNAVAILABLE_ASYNC_AGENT_REASON`] `}}`.
///
/// This is a DIFFERENT site from the port's headless `Ask`→`Deny` transport
/// ([`crate::headless_gate::DenyOnAskGate`]), which implements the separate
/// `xxf`/`GRu` message — unchanged between 2.1.220 and 2.1.238.
#[must_use]
pub fn prompts_unavailable_deny_message(ask_message: &str) -> String {
    format!(
        "Permission for this tool use was denied: it requires interactive approval, and permission prompts are not available in this session. The action was NOT performed. Do not claim it succeeded, and do not retry it in this session \u{2014} report the limitation to the user, or suggest an alternative. What was requested: {ask_message}"
    )
}

/// Compute the stdio `classifier_approvable` value from the structured
/// decision reason.  Claude Code emits this field only when at least one
/// `safetyCheck` exists and folds composite subcommand reasons so one
/// non-approvable check makes the whole request non-approvable.
fn classifier_approvable(reason: &PermissionDecisionReason) -> Option<bool> {
    fn visit(reason: &PermissionDecisionReason) -> (bool, bool) {
        match reason {
            PermissionDecisionReason::SafetyCheck {
                classifier_approvable,
                ..
            } => (true, *classifier_approvable),
            PermissionDecisionReason::SubcommandResults { reasons } => {
                reasons.values().fold((false, true), |(any, all), result| {
                    let nested = match result.as_ref() {
                        PermissionResult::Allow { reason, .. }
                        | PermissionResult::Deny { reason, .. }
                        | PermissionResult::Ask { reason, .. } => visit(reason),
                    };
                    (any || nested.0, all && nested.1)
                })
            }
            _ => (false, true),
        }
    }

    let (has_safety_check, all_approvable) = visit(reason);
    has_safety_check.then_some(all_approvable)
}

/// The `matched_ask_rule` control-request field — 1:1 with the 2.1.218
/// `matched_ask_rule` SDK schema (`b.object({source,tool_name,rule_content?})`).
///
/// The oracle sets this ONLY in the *ask-rule substitution* case: a
/// `permissions.ask` rule forced the prompt **but the ask carries the tool's own
/// `decision_reason`** (a richer tool-minted ask), so the rule "rides here
/// **instead of** `decision_reason_type: 'rule'`". `matched_ask_rule` and
/// `decision_reason_type: "rule"` are therefore MUTUALLY EXCLUSIVE.
///
/// This port collapses a plain ask-RULE match into a top-level
/// [`PermissionDecisionReason::MatchedRule`] ⇒ `decision_reason_type: "rule"`, so
/// the rule is already conveyed by the type and there is no tool-minted reason to
/// substitute against — the substitution case is never produced. Deriving the
/// field from a bare `MatchedRule` (the previous behavior) emitted BOTH
/// `decision_reason_type: "rule"` AND `matched_ask_rule` for the same ask, which
/// the oracle never does. There is no substitution producer in the port, so this
/// is always `None`.
fn matched_ask_rule(_reason: &PermissionDecisionReason) -> Option<MatchedAskRule> {
    None
}

/// Serialize a [`PermissionDecisionReason`] to the free-text `decision_reason`
/// string a stdio `can_use_tool` control_request carries — 1:1 with claude-code
/// `serializeDecisionReason` (`cli/structuredIO.ts:64-91`).
///
/// The oracle returns `undefined` (⇒ `None`, the key is OMITTED) for the
/// `rule`/`mode`/`subcommandResults`/`permissionPromptTool` reasons (the common
/// ask cases — an SDK host parses `decision_reason_type` for those, not the
/// text), and the reason STRING for `hook`/`asyncAgent`/`sandboxOverride`/
/// `workingDir`/`safetyCheck`/`other`/`classifier`.
///
/// Field-shape notes vs claude-code:
/// - [`PermissionDecisionReason::HookOverride`] carries `reason: Option<String>`
///   (not a bare string): `Some(r)` ⇒ that text, `None` ⇒ `None` (the hook
///   supplied no reason, so there is nothing to serialize).
/// - [`PermissionDecisionReason::SandboxOverride`] carries a
///   [`SandboxOverrideReason`] ENUM, not a free string. claude-code's
///   `sandboxOverride` reason is itself a string; rather than fabricate a
///   rendering, this returns `None` (the override is an allow-side reason that
///   does not reach the ask path in the external build anyway).
fn serialize_decision_reason(reason: &PermissionDecisionReason) -> Option<String> {
    match reason {
        // Oracle: rule/mode/subcommandResults/permissionPromptTool ⇒ undefined.
        PermissionDecisionReason::MatchedRule { .. }
        | PermissionDecisionReason::PermissionMode { .. }
        | PermissionDecisionReason::SubcommandResults { .. }
        | PermissionDecisionReason::PermissionPromptTool { .. } => None,
        // Oracle: hook/asyncAgent/workingDir/safetyCheck/other ⇒ reason string.
        PermissionDecisionReason::HookOverride { reason, .. } => reason.clone(),
        PermissionDecisionReason::AsyncAgent { reason }
        | PermissionDecisionReason::WorkingDirectory { reason }
        | PermissionDecisionReason::SafetyCheck { reason, .. }
        | PermissionDecisionReason::Other { reason } => Some(reason.clone()),
        // sandboxOverride: enum reason, no faithful string rendering → None
        // (see fn doc). Classifier denials preserve the classifier's own
        // human-readable reason, as `createCanUseTool` does in claude-code.
        PermissionDecisionReason::SandboxOverride { .. }
        | PermissionDecisionReason::ClassifierApproved { .. }
        | PermissionDecisionReason::DenialLimitExceeded
        | PermissionDecisionReason::AutoModeFallback
        | PermissionDecisionReason::BypassPermissions => None,
        PermissionDecisionReason::ClassifierRejected { reason, .. } => Some(reason.clone()),
    }
}

/// The claude-code `decisionReason.type` discriminant string for this reason —
/// sent as the `decision_reason_type` field of a `can_use_tool` request so an SDK
/// host can classify Ask reasons where the free-text `decision_reason` is
/// `undefined` (rule/mode/subcommandResults/permissionPromptTool). Every string
/// is byte-confirmed present in the 2.1.215 binary; the LingXi-internal
/// denial/auto-mode/bypass variants carry no CC `.type` and return `None`.
pub(crate) fn decision_reason_type(reason: &PermissionDecisionReason) -> Option<&'static str> {
    Some(match reason {
        PermissionDecisionReason::MatchedRule { .. } => "rule",
        PermissionDecisionReason::PermissionMode { .. } => "mode",
        PermissionDecisionReason::SubcommandResults { .. } => "subcommandResults",
        PermissionDecisionReason::PermissionPromptTool { .. } => "permissionPromptTool",
        PermissionDecisionReason::ClassifierApproved { .. }
        | PermissionDecisionReason::ClassifierRejected { .. } => "classifier",
        PermissionDecisionReason::HookOverride { .. } => "hook",
        PermissionDecisionReason::AsyncAgent { .. } => "asyncAgent",
        PermissionDecisionReason::WorkingDirectory { .. } => "workingDir",
        PermissionDecisionReason::SafetyCheck { .. } => "safetyCheck",
        PermissionDecisionReason::SandboxOverride { .. } => "sandboxOverride",
        PermissionDecisionReason::Other { .. } => "other",
        PermissionDecisionReason::DenialLimitExceeded
        | PermissionDecisionReason::AutoModeFallback
        | PermissionDecisionReason::BypassPermissions => return None,
    })
}

/// GATE-SYSMSG-01 `oin(decisionReason)`: the reason TEXT surfaced in a
/// `permission_denied` system message. Only the free-text reason kinds expose
/// their reason; `rule`/`mode`/`subcommandResults`/`permissionPromptTool` return
/// `None` (byte-faithful to `oin`).
///
/// The oracle also returns `e.reason` for `classifier` and `sandboxOverride`.
/// Classifier denials retain that text; `SandboxOverride` still carries a
/// structured [`SandboxOverrideReason`] rather than the oracle's plain string,
/// so that allow-side reason remains omitted here.
pub(crate) fn sysmsg_decision_reason(reason: &PermissionDecisionReason) -> Option<String> {
    match reason {
        PermissionDecisionReason::HookOverride { reason, .. } => reason.clone(),
        PermissionDecisionReason::AsyncAgent { reason } => Some(reason.clone()),
        PermissionDecisionReason::WorkingDirectory { reason } => Some(reason.clone()),
        PermissionDecisionReason::SafetyCheck { reason, .. } => Some(reason.clone()),
        PermissionDecisionReason::ClassifierRejected { reason, .. } => Some(reason.clone()),
        PermissionDecisionReason::Other { reason } => Some(reason.clone()),
        _ => None,
    }
}

/// Map a [`PermissionDecisionReason`] to the coarse [`PermissionDecisionSource`]
/// the turn loop gates its permission hooks on (claude-code `decisionReason.type`).
/// Only the classifier source unblocks the `PermissionDenied` hook.
fn map_decision_source(reason: &PermissionDecisionReason) -> PermissionDecisionSource {
    match reason {
        PermissionDecisionReason::MatchedRule { .. } => PermissionDecisionSource::Rule,
        PermissionDecisionReason::PermissionMode { .. } => PermissionDecisionSource::Mode,
        PermissionDecisionReason::ClassifierRejected { .. } => PermissionDecisionSource::Classifier,
        _ => PermissionDecisionSource::Unspecified,
    }
}

/// Raw `SettingSource` token of the rule behind a `decisionReason`, or `None`
/// when the decision did not come from a rule. Feeds the OTEL decision-source
/// label the turn loop derives with claude-code `ZX_` — which reads exactly
/// `decisionReason.rule.source` and only for `decisionReason.type === 'rule'`
/// (a `subcommandResults` composite maps to `"config"` there, so it is NOT
/// unwrapped here).
fn rule_settings_source(reason: &PermissionDecisionReason) -> Option<String> {
    match reason {
        PermissionDecisionReason::MatchedRule { rule } => {
            Some(rule.source.lingxi_settings_source().to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
#[path = "policy_gate_test.rs"]
mod policy_gate_test;

// PLAN-DIVERGENCE-01: separate inline module (kept out of the concurrently
// edited policy_gate_test.rs) proving that the LingXi-only `AllowByDefault`
// rows do not short-circuit the Plan-mode mutation backstop, while still being
// frictionless in the modes the row exists for.
#[cfg(test)]
mod plan_mode_divergence_test {
    use super::*;
    use crate::{PermissionMode, PermissionPolicy, PermissionRuleSource};
    use async_trait::async_trait;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Stands in for the interactive prompt transport. Asserting its CALL COUNT
    /// is what separates "the gate asked" from "the gate silently auto-allowed"
    /// — the two are indistinguishable from the returned decision alone when
    /// the user would have answered yes.
    struct CountingInner {
        calls: AtomicUsize,
        decision: PermissionDecision,
    }

    impl CountingInner {
        fn new(decision: PermissionDecision) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                decision,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl PermissionGate for CountingInner {
        async fn check(&self, _name: &str, _input: &Value) -> PermissionDecision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.decision.clone()
        }
    }

    fn gate_in(mode: PermissionMode, inner: Arc<CountingInner>) -> PolicyPermissionGate {
        let rules = crate::loader::permission_rules_from_settings_json(
            r#"{ "permissions": {} }"#,
            PermissionRuleSource::Settings(protocol::SettingsScope::Local),
        )
        .unwrap();
        let policy = Arc::new(PermissionPolicy::from_rules(mode, rules));
        PolicyPermissionGate::new(policy, inner)
    }

    /// `Workflow` is `AllowByDefault` so the create-app hand-off does not
    /// double-prompt — but `AllowByDefault` also short-circuits the Plan-mode
    /// backstop, and a workflow fans out side-effecting agents. It is not in
    /// `PLAN_SAFE_TOOLS`, so Plan mode must still reach the prompt transport.
    #[tokio::test]
    async fn plan_mode_still_gates_the_workflow_launcher() {
        let inner = CountingInner::new(PermissionDecision::Deny {
            reason: "planning".into(),
        });
        let gate = gate_in(PermissionMode::Plan, inner.clone());

        let decision = gate
            .check(
                "Workflow",
                &serde_json::json!({"scriptPath": "/w/build.js"}),
            )
            .await;
        assert!(
            matches!(decision, PermissionDecision::Deny { .. }),
            "a workflow launch must not run in plan mode, got {decision:?}"
        );
        assert_eq!(
            inner.calls(),
            1,
            "the prompt transport must be consulted — a silent auto-allow \
             never reaches it"
        );
    }

    /// The other half of the row: outside Plan mode the hand-off is still
    /// frictionless, and the prompt transport is never touched. Without this
    /// the test above would pass just as well with the row reverted.
    #[tokio::test]
    async fn default_mode_auto_allows_the_workflow_launcher_without_prompting() {
        let inner = CountingInner::new(PermissionDecision::Deny {
            reason: "should not be reached".into(),
        });
        let gate = gate_in(PermissionMode::Default, inner.clone());

        assert_eq!(
            gate.check(
                "Workflow",
                &serde_json::json!({"scriptPath": "/w/build.js"})
            )
            .await,
            PermissionDecision::Allow,
            "the create-app hand-off must not double-prompt"
        );
        assert_eq!(
            inner.calls(),
            0,
            "the auto-allow must short-circuit before the prompt transport"
        );
    }
}

// GATE-SYSMSG-01: separate inline module (kept out of the concurrently-edited
// policy_gate_test.rs) covering the `oin` reason filter and the
// `on_permission_denied` hook firing on a local Deny.
#[cfg(test)]
mod gate_sysmsg_test {
    use super::*;
    use crate::{
        PermissionBehavior, PermissionMode, PermissionPolicy, PermissionRule, PermissionRuleSource,
        PermissionRuleValue,
    };
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn sysmsg_decision_reason_matches_oin_filter() {
        // Free-text kinds expose their reason.
        assert_eq!(
            sysmsg_decision_reason(&PermissionDecisionReason::Other {
                reason: "nope".into()
            })
            .as_deref(),
            Some("nope")
        );
        assert_eq!(
            sysmsg_decision_reason(&PermissionDecisionReason::SafetyCheck {
                reason: "danger".into(),
                classifier_approvable: false,
                circuit_breaker: None,
            })
            .as_deref(),
            Some("danger")
        );
        assert_eq!(
            sysmsg_decision_reason(&PermissionDecisionReason::WorkingDirectory {
                reason: "escapes".into()
            })
            .as_deref(),
            Some("escapes")
        );
        // rule / mode → None (byte-faithful to `oin`).
        assert_eq!(
            sysmsg_decision_reason(&PermissionDecisionReason::PermissionMode {
                mode: PermissionMode::Default
            }),
            None
        );
        // classifier → the classifier's own free-text reason.
        assert_eq!(
            sysmsg_decision_reason(&PermissionDecisionReason::ClassifierRejected {
                classifier: crate::ClassifierKind::Bash,
                score: 0.9,
                reason: "blocked".into(),
            }),
            Some("blocked".to_string())
        );
    }

    #[derive(Default)]
    struct RecordingGate {
        denied: Arc<
            StdMutex<
                Vec<(
                    String,
                    Option<String>,
                    Option<String>,
                    Option<String>,
                    String,
                )>,
            >,
        >,
    }

    #[async_trait::async_trait]
    impl crate::gate::PermissionGate for RecordingGate {
        async fn check(&self, _name: &str, _input: &Value) -> PermissionDecision {
            PermissionDecision::Allow
        }
        async fn on_permission_denied(
            &self,
            name: &str,
            ctx: &PermissionCheckContext,
            decision_reason_type: Option<&str>,
            decision_reason: Option<&str>,
            message: &str,
        ) {
            self.denied.lock().unwrap().push((
                name.to_string(),
                ctx.tool_use_id.clone(),
                decision_reason_type.map(str::to_string),
                decision_reason.map(str::to_string),
                message.to_string(),
            ));
        }
    }

    #[tokio::test]
    async fn deny_fires_on_permission_denied_with_ctx_and_reason() {
        // A tool-wide Bash DENY rule → any Bash call denies.
        let deny = PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Bash".into(),
                rule_content: None,
            },
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::User),
        };
        let policy = Arc::new(PermissionPolicy::from_rules(
            PermissionMode::Default,
            vec![deny],
        ));
        let recorder = Arc::new(RecordingGate::default());
        let calls = recorder.denied.clone();
        let gate = PolicyPermissionGate::new(policy, recorder);

        let ctx = PermissionCheckContext {
            tool_use_id: Some("tu-77".into()),
            ..PermissionCheckContext::default()
        };
        let outcome = gate
            .check_with_context("Bash", &json!({"command": "rm -rf /"}), &ctx)
            .await;
        assert!(matches!(outcome, PermissionOutcome::Deny { .. }));

        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1, "on_permission_denied fired exactly once");
        let (name, tuid, drt, _dr, message) = &recorded[0];
        assert_eq!(name, "Bash");
        assert_eq!(tuid.as_deref(), Some("tu-77"));
        assert_eq!(drt.as_deref(), Some("rule")); // a deny RULE → type "rule"
        assert!(!message.is_empty());
    }

    #[tokio::test]
    async fn allow_does_not_fire_on_permission_denied() {
        // No rules, Default mode: a read-only tool allows → no deny notification.
        let policy = Arc::new(PermissionPolicy::from_rules(
            PermissionMode::Default,
            std::iter::empty(),
        ));
        let recorder = Arc::new(RecordingGate::default());
        let calls = recorder.denied.clone();
        let gate = PolicyPermissionGate::new(policy, recorder);
        let _ = gate
            .check_with_context("Read", &json!({}), &PermissionCheckContext::default())
            .await;
        assert!(calls.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod task_pause_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Prompt;
    #[async_trait]
    impl PermissionGate for Prompt {
        async fn check(&self, _: &str, _: &Value) -> PermissionDecision {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            PermissionDecision::Allow
        }
    }
    fn fixture(
        mode: PermissionMode,
    ) -> (PolicyPermissionGate, PermissionCheckContext, Arc<AtomicU64>) {
        let rules = crate::loader::permission_rules_from_settings_json(
            r#"{"permissions":{}}"#,
            PermissionRuleSource::Settings(protocol::SettingsScope::Local),
        )
        .unwrap();
        let total = Arc::new(AtomicU64::new(0));
        let sink = total.clone();
        let ctx = PermissionCheckContext {
            pause_observer: Some(platform_api::permission_gate::PermissionPauseObserver::new(
                move |ms| {
                    sink.fetch_add(ms, Ordering::SeqCst);
                },
            )),
            ..Default::default()
        };
        (
            PolicyPermissionGate::new(
                Arc::new(PermissionPolicy::from_rules(mode, rules)),
                Arc::new(Prompt),
            ),
            ctx,
            total,
        )
    }

    #[tokio::test]
    async fn task_pause_records_real_ask_but_not_policy_allow() {
        let (gate, ctx, total) = fixture(PermissionMode::Default);
        gate.ask_via_transport("Bash", &serde_json::json!({"command":"echo probe"}), &ctx)
            .await;
        assert!(total.load(Ordering::SeqCst) >= 20);
        let (gate, ctx, total) = fixture(PermissionMode::BypassPermissions);
        gate.check_with_context_or_abort(
            "Bash",
            &serde_json::json!({"command":"echo probe"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(total.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn task_pause_records_cancelled_prompt_wait() {
        let (gate, ctx, total) = fixture(PermissionMode::Default);
        let input = serde_json::json!({"command":"echo probe"});
        let pending = gate.ask_via_transport("Bash", &input, &ctx);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), pending)
                .await
                .is_err()
        );
        assert!(total.load(Ordering::SeqCst) >= 5);
    }
}
