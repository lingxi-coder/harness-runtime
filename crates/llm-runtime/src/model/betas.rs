//! `anthropic-beta` header constants + the per-model / per-request beta gate.
//!
//! Claude Code 2.1.287 built-in `aN/uN/eZ/vqo` model gates and current
//! request feature additions. Native model ordering is chronological (`av/lr`),
//! not substring matching. Environment and feature admission remain host-owned.
//!
//! `betas_2_1_287.json` executes the pinned native source with no served catalog,
//! dynamic flags, HIPAA taint, subscriber or beta-denial state. Those stateful
//! gates and `awn` per-turn controls still need end-to-end acceptance.

#![forbid(unsafe_code)]

// ---- beta constants (declaration order = the binary's `qS(...)` registry) ----

/// Core claude-code feature gate (`J1e`).
pub const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
/// Interleaved-thinking output (`pYe`).
pub const INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
/// 1M-token context window (`$7`, gated by `T_`).
pub const CONTEXT_1M: &str = "context-1m-2025-08-07";
/// Context-management endpoint (`X1e`).
pub const CONTEXT_MANAGEMENT: &str = "context-management-2025-06-27";
/// Structured-output JSON-schema mode (`bQ`, `tengu_tool_pear` flag-gated).
pub const STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";
/// First-party web-search tool gating (`Xvt`, vertex/foundry only).
pub const WEB_SEARCH: &str = "web-search-2025-03-05";
/// Advanced tool-use semantics (`fRr`, 1P only).
pub const ADVANCED_TOOL_USE_1P: &str = "advanced-tool-use-2025-11-20";
/// Tool-search tool (`Qvt`, Vertex / Bedrock).
pub const TOOL_SEARCH_TOOL_3P: &str = "tool-search-tool-2025-10-19";
/// Effort hint header (`mYe`, per-request `output_config.effort`).
pub const EFFORT: &str = "effort-2025-11-24";
/// Per-task token budget enforcement (`wun`, per-request `output_config.task_budget`).
pub const TASK_BUDGETS: &str = "task-budgets-2026-03-13";
/// Prompt-caching scope control (`fYe`, experimental).
pub const PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";
/// Context-hint negotiation (`_9i`, per-request `context_hint` body key).
///
/// Gated OFF by default at the caller — the oracle's `tengu_hazel_osprey` is
/// false in the binary AND server-delivered as false, so this header only ever
/// goes out when a host explicitly opts in.
pub const CONTEXT_HINT: &str = "context-hint-2026-04-09";
/// Fast-mode / `speed: "fast"` (`AYe`, per-request).
pub const FAST_MODE: &str = "fast-mode-2026-02-01";
/// Redact thinking-block output (`Zvt`, experimental ∧ thinking ∧ interactive).
pub const REDACT_THINKING: &str = "redact-thinking-2026-02-12";
/// Thinking-token-count (`Run`, experimental ∧ thinking ∧ firstParty).
///
/// NEW vs the prior static module — the binary emits this on every first-party
/// thinking request; LingXi previously missed it.
pub const THINKING_TOKEN_COUNT: &str = "thinking-token-count-2026-05-13";
/// Mid-conversation system messages (`q7`, `nhn`-gated by model).
///
/// NEW vs the prior static module — the binary emits this for opus-4-8 /
/// fable-5 / mythos-5 (and unknown first-party models); LingXi previously
/// missed it.
pub const MID_CONVERSATION_SYSTEM: &str = "mid-conversation-system-2026-04-07";
/// Native main-turn system effort control, emitted after mid-conversation
/// system support and before the general per-request effort beta.
pub const PER_TURN_CONTROL: &str = "per-turn-control-2026-07-01";
/// Native main-request beta for admitted mid-conversation tool changes.
pub const MID_CONVERSATION_TOOL_CHANGES: &str =
    lingxi_llm_client::providers::anthropic::beta_repair::Beta::MidConversationToolChanges.header();
/// Native's additional inline tool-definition beta (`p_`), admitted only after
/// the MCTC gate and Native's request-local inline-tools gate.
pub const INLINE_TOOLS_BETA: &str = "inline-tools-2026-09-15";
/// Native safeguard beta paired with an admitted server-classifier context.
pub const DANGEROUS_TOOL_USE: &str = "dangerous-tool-use-2026-09-03";
/// Connector-text summarization (`ewt`, `pewter_owl_header` flag — never emitted).
pub const SUMMARIZE_CONNECTOR_TEXT: &str = "summarize-connector-text-2026-03-13";
/// AFK transcript-classifier mode (`PH`).
pub const AFK_MODE: &str =
    lingxi_llm_client::providers::anthropic::beta_repair::Beta::AfkMode.header();
/// Advisor tool integration (`ARr`, per-request advisor tool).
pub const ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";
/// OAuth bearer-token auth (`VEe`, subscriber-driven via [`apply_beta_header_with_auth`]).
pub const OAUTH: &str = "oauth-2025-04-20";

// ---- Per-provider / per-endpoint policy whitelists --------------------------

/// `count_tokens` keeps only these (the binary's `ERr` set, applied on the
/// first-party `beta.messages.countTokens` path: `e5(model).filter(ERr.has)`).
///
/// Four entries — includes [`OAUTH`], which the prior 3-entry `Vertex`-labelled
/// whitelist omitted. The binary applies `ERr` on the first-party count_tokens
/// path, not just Vertex.
pub const COUNT_TOKENS_ALLOWED: &[&str] = &[
    CLAUDE_CODE_BETA,
    INTERLEAVED_THINKING,
    CONTEXT_MANAGEMENT,
    OAUTH,
];

/// Bedrock requires these to ride in `extraBodyParams`, NOT the header
/// (the binary's `bRr` set: `e5(model).filter(n => !bRr.has(n))`).
pub const BEDROCK_EXTRA_PARAMS_HEADERS: &[&str] =
    &[INTERLEAVED_THINKING, CONTEXT_1M, TOOL_SEARCH_TOOL_3P];

// ---- Provider × Endpoint matrix --------------------------------------------

/// API provider routing target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Direct Anthropic API (the binary's `firstParty`).
    Anthropic,
    /// Azure Foundry Claude models (distinct native beta gates).
    Foundry,
    /// Google Cloud Vertex AI Anthropic models.
    Vertex,
    /// AWS Bedrock Anthropic models.
    Bedrock,
}

/// Endpoint kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Non-streaming `POST /v1/messages`.
    MessagesCreate,
    /// Streaming `POST /v1/messages` (`stream: true`).
    MessagesCreateStream,
    /// `POST /v1/messages/count_tokens`.
    CountTokens,
}

/// Per-request inputs to the beta gate (the binary's `xLr(model)` closure
/// state). Built at the call site from the prepared request + session state.
#[derive(Debug, Clone)]
pub struct BetaContext {
    /// Resolved request model id (e.g. `claude-opus-4-8`).
    pub model: String,
    /// `!isInteractive` is the binary's `kr()`; this is `isInteractive`. Gates
    /// `redact-thinking`. Defaults to `true` (the interactive TUI is LingXi's
    /// dominant path).
    pub interactive: bool,
    /// The binary's `ZAn()` (`settings.showThinkingSummaries`). When `true`,
    /// `redact-thinking` is suppressed. Defaults to `false`.
    pub show_thinking_summaries: bool,
    /// The request body sets `speed: "fast"`. Gates `fast-mode`. Defaults
    /// to `false`.
    pub fast_mode: bool,
    /// Native YMe selects the automatic effort beta, including supported main defaults.
    /// Defaults to `false`.
    pub effort: bool,
    /// Native per-turn system effort marker is present on this request.
    pub per_turn_control: bool,
    /// Native admitted mid-conversation tool-change beta for this request.
    pub mid_conversation_tool_changes: bool,
    /// Native admitted the additional inline-tools beta for this request.
    pub inline_tools: bool,
    /// Native AFK beta after route admission and session/conversation latches.
    pub afk_mode: bool,
    /// Native admitted server-classifier safeguard beta for this request.
    pub dangerous_tool_use: bool,
    /// Automatic format admitted before explicit extra-body merging.
    pub structured_output: bool,
    pub request_kind: lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
    /// Request uses ToolSearch/deferred schemas. Appends the first-party
    /// `advanced-tool-use` beta after the ordinary model betas.
    pub tool_search: bool,
    /// The request body sets `context_hint`. Gates the [`CONTEXT_HINT`] beta.
    /// Defaults to `false`.
    pub context_hint: bool,
}

impl BetaContext {
    /// Context for `model` with the faithful external-default gates
    /// (interactive TUI, no thinking summaries, standard speed).
    #[must_use]
    pub fn for_model(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            interactive: true,
            show_thinking_summaries: false,
            fast_mode: false,
            effort: false,
            per_turn_control: false,
            mid_conversation_tool_changes: false,
            inline_tools: false,
            afk_mode: false,
            dangerous_tool_use: false,
            structured_output: false,
            request_kind: Default::default(),
            tool_search: false,
            context_hint: false,
        }
    }

    /// Builder: set the interactive flag (the binary's `isInteractive`).
    #[must_use]
    pub fn with_interactive(mut self, interactive: bool) -> Self {
        self.interactive = interactive;
        self
    }

    /// Builder: set `showThinkingSummaries` (the binary's `ZAn()`).
    #[must_use]
    pub fn with_show_thinking_summaries(mut self, on: bool) -> Self {
        self.show_thinking_summaries = on;
        self
    }

    /// Builder: set `fast_mode` (request body `speed: "fast"`).
    #[must_use]
    pub fn with_fast_mode(mut self, on: bool) -> Self {
        self.fast_mode = on;
        self
    }

    /// Builder: set `effort` (request body `output_config.effort`).
    #[must_use]
    pub fn with_effort(mut self, on: bool) -> Self {
        self.effort = on;
        self
    }

    #[must_use]
    pub fn with_per_turn_control(mut self, on: bool) -> Self {
        self.per_turn_control = on;
        self
    }

    #[must_use]
    pub fn with_mid_conversation_tool_changes(mut self, on: bool) -> Self {
        self.mid_conversation_tool_changes = on;
        self
    }

    #[must_use]
    pub fn with_inline_tools(mut self, on: bool) -> Self {
        self.inline_tools = on;
        self
    }

    #[must_use]
    pub fn with_afk_mode(mut self, on: bool) -> Self {
        self.afk_mode = on;
        self
    }

    #[must_use]
    pub fn with_dangerous_tool_use(mut self, on: bool) -> Self {
        self.dangerous_tool_use = on;
        self
    }

    #[must_use]
    pub fn with_structured_output(mut self, on: bool) -> Self {
        self.structured_output = on;
        self
    }

    #[must_use]
    pub fn with_request_kind(
        mut self,
        kind: lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
    ) -> Self {
        self.request_kind = kind;
        self
    }

    /// Builder: set dynamic ToolSearch usage for this request.
    #[must_use]
    pub fn with_tool_search(mut self, on: bool) -> Self {
        self.tool_search = on;
        self
    }

    /// Set [`Self::context_hint`] — the request body carries `context_hint`.
    #[must_use]
    pub fn with_context_hint(mut self, on: bool) -> Self {
        self.context_hint = on;
        self
    }
}

/// Native OI/Ab built-in identity branch. Served model aliases and overrides
/// are resolved by the caller; this branch handles native wire IDs.
pub(crate) fn beta_canonical(model: &str) -> String {
    let model = model.to_ascii_lowercase();
    const KNOWN: &[&str] = &[
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-mythos-5-1",
        "claude-mythos-5",
        "claude-opus-5-5",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-sonnet-5-5",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-3-7-sonnet",
        "claude-3-5-sonnet",
        "claude-3-5-haiku",
        "claude-3-opus",
        "claude-3-sonnet",
        "claude-3-haiku",
    ];
    // OI checks the undotted 4.0 identities before the subsequent families.
    for &known in KNOWN {
        if known == "claude-sonnet-5-5" && undotted_four(&model, "claude-opus-4") {
            return "claude-opus-4-0".into();
        }
        if known == "claude-haiku-4-5" && undotted_four(&model, "claude-sonnet-4") {
            return "claude-sonnet-4-0".into();
        }
        if model.contains(known) {
            return known.into();
        }
    }
    let suffix = model
        .rsplit_once('-')
        .filter(|(_, suffix)| suffix.len() == 8 && suffix.bytes().all(|b| b.is_ascii_digit()));
    suffix.map_or(model.clone(), |(base, _)| base.into())
}

fn undotted_four(model: &str, family: &str) -> bool {
    model.match_indices(family).any(|(start, _)| {
        let tail = &model.as_bytes()[start + family.len()..];
        !(tail.first() == Some(&b'-')
            && tail.get(1).is_some_and(u8::is_ascii_digit)
            && !tail.get(2).is_some_and(u8::is_ascii_digit))
    })
}

const MODEL_ORDER: &[&str] = &[
    "claude-opus-4-0",
    "claude-sonnet-4-0",
    "claude-opus-4-1",
    "claude-sonnet-4-5",
    "claude-haiku-4-5",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-sonnet-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
];

pub(crate) fn older_than(model: &str, threshold: &str) -> bool {
    let model = beta_canonical(model);
    model.contains("claude-3-")
        || MODEL_ORDER
            .iter()
            .position(|&name| name == model)
            .is_some_and(|index| {
                index
                    < MODEL_ORDER
                        .iter()
                        .position(|&name| name == threshold)
                        .expect("native threshold")
            })
}

pub(crate) fn structured_outputs_model_capable(model: &str) -> bool {
    // Native Tvn/f$: all four Claude protocol families are admitted. The
    // caller checks the Claude protocol family before this model-order gate.
    !older_than(model, "claude-opus-4-1")
}

pub(crate) fn effort_capable(provider: Provider, model: &str) -> bool {
    effort_capable_in(
        provider,
        model,
        env_truthy("CLAUDE_CODE_ALWAYS_ENABLE_EFFORT"),
    )
}

fn effort_capable_in(provider: Provider, model: &str, always: bool) -> bool {
    let model = beta_canonical(model);
    !model.contains("claude-3-")
        && ![
            "claude-opus-4-0",
            "claude-opus-4-1",
            "claude-sonnet-4-0",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
        ]
        .contains(&model.as_str())
        && (always
            || model == "claude-mythos-5"
            || matches!(provider, Provider::Anthropic | Provider::Foundry))
}

fn is_haiku(model: &str) -> bool {
    beta_canonical(model).contains("haiku")
}
fn thinking_capable(model: &str) -> bool {
    !beta_canonical(model).contains("claude-3-")
}
fn context_management_capable(model: &str) -> bool {
    thinking_capable(model)
}
fn context_1m_active(model: &str) -> bool {
    model.to_ascii_lowercase().contains("[1m]")
}
fn mid_conversation_system(model: &str) -> bool {
    !older_than(model, "claude-opus-4-8")
}

/// Native `mSe(model)` capability after the caller has admitted the provider
/// route and `hQ(model)`. Unknown baked-catalog ids and Mythos-5 use the
/// source's permissive fallback; a known catalog entry without this capability
/// is a negative observation.
pub(crate) fn native_mid_conversation_tool_change_supported(
    raw_model: &str,
    model_unsupported: bool,
) -> bool {
    let force_enabled = native_environment_truthy(
        std::env::var_os("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM").as_deref(),
    );
    let canonical =
        lingxi_core::host::model_capabilities::normalize_model_id(&beta_canonical(raw_model));
    let explicit_capability = lingxi_core::host::model_capabilities::capability_override(
        &canonical,
        "mid_conv_tool_change",
        std::env::var("CLAUDE_CODE_MODEL_CAPABILITIES")
            .ok()
            .as_deref(),
    );
    let baked_capability = lingxi_core::host::model_capabilities::capabilities_for(&canonical)
        .contains(&"mid_conv_tool_change");
    native_mid_conversation_tool_change_supported_with_inputs(
        &canonical,
        force_enabled,
        model_unsupported,
        explicit_capability,
        baked_capability,
        lingxi_core::host::model_capabilities::is_known_model_id(&canonical),
    )
}

/// Combine the process-local Native MCTC and inline feature inputs after they
/// have been sampled for one logical request. Route and model capability are
/// checked separately by the caller. `inline_tools_env` preserves the
/// distinction between an unset override and an explicit false value.
pub(crate) fn native_tool_change_feature_gates_for_inputs(
    b6_disabled: bool,
    foamy_spring_enabled: bool,
    session_profile_active: bool,
    inline_tools_env: Option<&str>,
    brisk_meadow_enabled: bool,
) -> (bool, bool) {
    let mid_conversation_tool_changes = !b6_disabled && foamy_spring_enabled;
    let inline_tools = mid_conversation_tool_changes
        && !session_profile_active
        && inline_tools_env
            .map(crate::structured_output::bool_value)
            .unwrap_or(brisk_meadow_enabled);
    (mid_conversation_tool_changes, inline_tools)
}

fn native_mid_conversation_tool_change_supported_with_inputs(
    canonical_model: &str,
    force_enabled: bool,
    model_unsupported: bool,
    explicit_capability: Option<bool>,
    baked_capability: bool,
    known_catalog_model: bool,
) -> bool {
    if force_enabled {
        return true;
    }
    if model_unsupported {
        return false;
    }
    if let Some(supported) = explicit_capability.or_else(|| baked_capability.then_some(true)) {
        return supported;
    }
    canonical_model == "claude-mythos-5" || !known_catalog_model
}

/// Native `TF(model)` fallback used by `hQ` when deciding whether a main
/// conversation may place system attachments before its closing line. The
/// service layer applies Native's outer `Ta` lazy memoization around this
/// fallback. `raw_model` is retained for Native's model-option lookup;
/// `resolved_model` is the selected route's actual model identity.
pub(crate) fn native_mid_conversation_system_supported(
    raw_model: &str,
    resolved_model: &str,
    model_option_overrides_allowed: bool,
) -> bool {
    let hipaa_tainted = lingxi_core::host::compliance_taints::is_tainted("hipaa");
    if hipaa_tainted {
        return false;
    }
    let force_enabled = native_environment_truthy(
        std::env::var_os("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM").as_deref(),
    );
    if force_enabled {
        return true;
    }
    let option_overrides = native_model_option_capability_from_process_env(
        raw_model,
        model_option_overrides_allowed,
        "mid_conversation_system",
    );
    native_mid_conversation_system_supported_with_inputs(
        resolved_model,
        model_option_overrides_allowed,
        false,
        false,
        option_overrides,
        std::env::var("CLAUDE_CODE_MODEL_CAPABILITIES")
            .ok()
            .as_deref(),
    )
}

fn native_mid_conversation_system_supported_with_inputs(
    resolved_model: &str,
    model_option_overrides_allowed: bool,
    hipaa_tainted: bool,
    force_enabled: bool,
    model_option_capability: Option<bool>,
    model_capabilities: Option<&str>,
) -> bool {
    if hipaa_tainted {
        return false;
    }
    if force_enabled {
        return true;
    }
    if model_option_overrides_allowed {
        if let Some(supported) = model_option_capability {
            return supported;
        }
    }

    let canonical = lingxi_core::host::model_capabilities::normalize_model_id(resolved_model);
    if older_than(&canonical, "claude-opus-4-8") {
        return false;
    }
    lingxi_core::host::model_capabilities::capability_override(
        &canonical,
        "mid_conv_system",
        model_capabilities,
    )
    .unwrap_or(true)
}

const NATIVE_MODEL_OPTION_CAPABILITY_ENVS: [(&str, &str); 5] = [
    (
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
    ),
    (
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
    ),
    (
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES",
    ),
    (
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES",
    ),
    (
        "ANTHROPIC_CUSTOM_MODEL_OPTION",
        "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
    ),
];

pub(crate) fn native_model_option_capability_from_process_env(
    raw_model: &str,
    model_option_overrides_allowed: bool,
    capability: &str,
) -> Option<bool> {
    native_model_option_capability_in(
        raw_model,
        model_option_overrides_allowed,
        capability,
        |name| std::env::var(name).ok(),
    )
}

fn native_model_option_capability_in(
    raw_model: &str,
    model_option_overrides_allowed: bool,
    capability: &str,
    mut get_environment: impl FnMut(&str) -> Option<String>,
) -> Option<bool> {
    if !model_option_overrides_allowed {
        return None;
    }
    for (model_env, capabilities_env) in NATIVE_MODEL_OPTION_CAPABILITY_ENVS {
        let Some(configured_model) = get_environment(model_env) else {
            continue;
        };
        if configured_model.trim().is_empty() {
            continue;
        }
        let Some(capabilities) = get_environment(capabilities_env) else {
            continue;
        };
        if !configured_model.trim().eq_ignore_ascii_case(raw_model) {
            continue;
        }
        return Some(
            capabilities
                .split(',')
                .any(|candidate| candidate.trim().eq_ignore_ascii_case(capability)),
        );
    }
    None
}

fn env_truthy(var: &str) -> bool {
    crate::structured_output::bool_environment(var)
}

/// Native TF uses a direct JavaScript truthiness check for its force override.
/// Unlike boolean-valued settings, every nonempty process environment string
/// (including `"false"` and `"0"`) is truthy.
fn native_environment_truthy(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|value| !value.is_empty())
}

/// Build the first-party (`xLr(model)`) ordered beta set for messages.create.
fn model_betas(provider: Provider, ctx: &BetaContext) -> Vec<&'static str> {
    let environment = BetaEnvironment::read();
    model_betas_in(provider, ctx, environment)
}

#[derive(Clone, Copy, Default)]
struct BetaEnvironment {
    disabled: bool,
    interleaved_disabled: bool,
    force_mid_system: bool,
}
impl BetaEnvironment {
    fn read() -> Self {
        Self {
            disabled: crate::model::thinking_binding_controls::native_b6_disabled(),
            interleaved_disabled: env_truthy("DISABLE_INTERLEAVED_THINKING"),
            force_mid_system: env_truthy("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM"),
        }
    }
}
fn model_betas_in(
    provider: Provider,
    ctx: &BetaContext,
    env: BetaEnvironment,
) -> Vec<&'static str> {
    let model = &ctx.model;
    let exp = !env.disabled;
    let first_party = matches!(provider, Provider::Anthropic | Provider::Foundry);
    let gkt = provider == Provider::Foundry
        || (thinking_capable(model)
            && (first_party || beta_canonical(model) != "claude-haiku-4-5"));
    let mut betas: Vec<&'static str> = Vec::new();

    // 1. claude-code (unless Haiku).
    if !is_haiku(model) {
        betas.push(CLAUDE_CODE_BETA);
    }
    // (2. oauth `VEe` is appended by apply_beta_header_with_auth — subscriber-driven.)
    // 3. context-1m, only when the model id carries `[1m]`.
    if context_1m_active(model) {
        betas.push(CONTEXT_1M);
    }
    // 4. interleaved-thinking, model thinking-capable ∧ not env-disabled.
    if !env.interleaved_disabled && gkt {
        betas.push(INTERLEAVED_THINKING);
    }
    // 5. redact-thinking: experimental ∧ thinking ∧ interactive ∧ !summaries.
    if first_party && exp && gkt && ctx.interactive && !ctx.show_thinking_summaries {
        betas.push(REDACT_THINKING);
    }
    // 6. thinking-token-count: experimental ∧ thinking ∧ firstParty.
    if exp
        && gkt
        && (provider == Provider::Anthropic
            || provider == Provider::Bedrock && !older_than(model, "claude-opus-4-7"))
    {
        betas.push(THINKING_TOKEN_COUNT);
    }
    // 7. narration_summaries (`pewter_owl_header` flag, default off) — skipped.
    // 8. context-management: !$Be ∧ KWu(model).
    if first_party && exp && (provider == Provider::Foundry || context_management_capable(model)) {
        betas.push(CONTEXT_MANAGEMENT);
    }
    // 9. structured-outputs (`tengu_tool_pear` flag, default off) — skipped.
    if provider == Provider::Foundry
        || provider == Provider::Vertex && !older_than(model, "claude-opus-4-0")
    {
        betas.push(WEB_SEARCH);
    }
    // 11. prompt-caching-scope: experimental.
    if first_party && exp {
        betas.push(PROMPT_CACHING_SCOPE);
    }
    // 12. mid-conversation-system: nhn(model).
    if env.force_mid_system
        || beta_canonical(model) == "claude-mythos-5"
        || first_party && mid_conversation_system(model)
    {
        betas.push(MID_CONVERSATION_SYSTEM);
    }
    if first_party
        && exp
        && ctx.effort
        && ctx.request_kind == Default::default()
        && lingxi_llm_client::providers::anthropic::supports_per_message_effort(model)
    {
        betas.push(PER_TURN_CONTROL);
    }
    // (ANTHROPIC_BETAS env append is honored by apply_beta_header's merge.)
    // Per-feature: fast-mode when the request sets speed:"fast".
    if ctx.fast_mode {
        betas.push(FAST_MODE);
    }
    // Per-feature: effort when the request sets output_config.effort.
    let side = ctx.request_kind
        == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery;
    if side && ctx.structured_output {
        betas.push(STRUCTURED_OUTPUTS);
    }
    if ctx.per_turn_control {
        betas.push(PER_TURN_CONTROL);
    }
    if first_party
        && ctx.request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        && ctx.mid_conversation_tool_changes
    {
        betas.push(MID_CONVERSATION_TOOL_CHANGES);
    }
    if first_party
        && ctx.request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        && ctx.mid_conversation_tool_changes
        && ctx.inline_tools
    {
        betas.push(INLINE_TOOLS_BETA);
    }
    if ctx.effort {
        betas.push(EFFORT);
    }
    if first_party
        && ctx.request_kind
            == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main
        && ctx.dangerous_tool_use
    {
        betas.push(DANGEROUS_TOOL_USE);
    }
    // Per-feature: context-hint when the request sets the `context_hint` key.
    if ctx.context_hint {
        betas.push(CONTEXT_HINT);
    }
    if !side && ctx.structured_output {
        betas.push(STRUCTURED_OUTPUTS);
    }
    // claude.ts appends the provider-specific tool-search header after the
    // ordinary model/request beta list once `useToolSearch` is resolved.
    if ctx.tool_search {
        betas.push(if first_party && exp {
            ADVANCED_TOOL_USE_1P
        } else {
            TOOL_SEARCH_TOOL_3P
        });
    }
    betas
}

/// Append Native AFK only after the automatic thinking-display beta has been
/// resolved, preserving Native header order. Callers must pass an admitted main
/// request fact; Bedrock carries AFK in the body array instead of this header.
pub(crate) fn append_afk_mode_after_thinking_display(
    headers: &mut std::collections::BTreeMap<String, String>,
    provider: Provider,
    admitted: bool,
) {
    if admitted
        && matches!(
            provider,
            Provider::Anthropic | Provider::Foundry | Provider::Vertex
        )
    {
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            headers,
            &[AFK_MODE.to_owned()],
        );
    }
}

/// Build the `anthropic-beta` header value for `provider` + `endpoint` + the
/// per-request [`BetaContext`].
///
/// Ports the binary's `e5(model)`: the first-party ordered set from
/// [`model_betas`], then provider/endpoint narrowing:
/// - **`CountTokens`** keeps only [`COUNT_TOKENS_ALLOWED`] (the binary's `ERr`).
/// - **`Bedrock`** drops [`BEDROCK_EXTRA_PARAMS_HEADERS`] (the binary's `bRr`),
///   which move to `extraBodyParams`.
/// - **`Vertex`** drops the 1P-only [`ADVANCED_TOOL_USE_1P`] (not emitted on the
///   first-party path here anyway).
#[must_use]
pub fn assemble_beta_header(provider: Provider, endpoint: Endpoint, ctx: &BetaContext) -> String {
    let mut betas = model_betas(provider, ctx);

    // count_tokens: intersect with the ERr whitelist (any provider).
    if matches!(endpoint, Endpoint::CountTokens) {
        betas.retain(|b| COUNT_TOKENS_ALLOWED.contains(b));
    }

    match provider {
        Provider::Anthropic | Provider::Foundry => {}
        Provider::Vertex => {
            betas.retain(|b| *b != ADVANCED_TOOL_USE_1P);
        }
        Provider::Bedrock => {
            betas.retain(|b| {
                !BEDROCK_EXTRA_PARAMS_HEADERS.contains(b) && *b != ADVANCED_TOOL_USE_1P
            });
        }
    }

    betas.join(",")
}

/// Beta identifiers Bedrock requires in the request body's
/// `anthropic_beta` array rather than the HTTP header.
#[must_use]
pub fn bedrock_extra_body_betas(ctx: &BetaContext) -> Vec<String> {
    let mut betas: Vec<String> = model_betas(Provider::Bedrock, ctx)
        .into_iter()
        .filter(|beta| BEDROCK_EXTRA_PARAMS_HEADERS.contains(beta))
        .map(str::to_string)
        .collect();
    if ctx.afk_mode {
        betas.push(AFK_MODE.to_string());
    }
    betas
}

/// Inject the assembled `anthropic-beta` header into a prepared request,
/// merging with any pre-existing value (e.g. an auth-injected `oauth-2025-04-20`).
///
/// Pre-existing values are preserved first in their original order; assembled
/// entries not already present are appended in declaration order; duplicates
/// are dropped. See the auth-layer note on [`apply_beta_header_with_auth`].
pub fn apply_beta_header(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
) {
    // The SDK discovers this token from typed message controls. The native
    // host owns its position in the model-beta vector, rather than prepending
    // an encoder-discovered token before the ordinary model gates.
    let native_per_turn = provider == Provider::Anthropic
        && ctx.effort
        && ctx.request_kind == Default::default()
        && lingxi_llm_client::providers::anthropic::supports_per_message_effort(&ctx.model);
    if native_per_turn {
        if let Some(existing) = request.headers.get_mut("anthropic-beta") {
            *existing = existing
                .split(',')
                .filter(|value| value.trim() != PER_TURN_CONTROL)
                .collect::<Vec<_>>()
                .join(",");
        }
    }
    lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
        &mut request.headers,
        &[assemble_beta_header(provider, endpoint, ctx)],
    );
}

/// Variant of [`apply_beta_header`] that also appends `oauth-2025-04-20` when
/// `is_oauth_subscriber` is `true`.
///
/// **Parity:** mirrors `betas.ts`'s subscriber push
/// (`if (isClaudeAISubscriber()) betaHeaders.push(OAUTH_BETA_HEADER)`).
/// Under OAuth subscriber auth, `oauth-2025-04-20` is appended as one element of
/// the comma-joined list — never as a clobbering insert; the dedup pass keeps it
/// at most once. For non-subscriber routes this behaves like [`apply_beta_header`].
pub fn apply_beta_header_with_auth(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
    is_oauth_subscriber: bool,
) {
    apply_beta_header_with_auth_and_custom(
        request,
        provider,
        endpoint,
        ctx,
        is_oauth_subscriber,
        &[],
    );
}

/// Auth-aware beta assembly with an explicit, host-validated CLI beta list.
/// Keeping the custom values as an argument prevents process environment state
/// from leaking them onto a custom Anthropic-wire-compatible provider route.
pub fn apply_beta_header_with_auth_and_custom(
    request: &mut crate::ProviderRequest,
    provider: Provider,
    endpoint: Endpoint,
    ctx: &BetaContext,
    is_oauth_subscriber: bool,
    custom_betas: &[String],
) {
    apply_beta_header(request, provider, endpoint, ctx);

    // CLI `--betas` additions for a Native Anthropic-family Messages create
    // request. The caller passes only route-validated, Native-filtered values;
    // this layer still excludes OAuth and count-token endpoints.
    if !is_oauth_subscriber
        && matches!(
            provider,
            Provider::Anthropic | Provider::Foundry | Provider::Bedrock | Provider::Vertex
        )
        && matches!(
            endpoint,
            Endpoint::MessagesCreate | Endpoint::MessagesCreateStream
        )
        && !custom_betas.is_empty()
    {
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut request.headers,
            custom_betas,
        );
    }

    if is_oauth_subscriber {
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut request.headers,
            &[OAUTH.to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2.1.287 fixture remains immutable. Exact 2.1.295 print captures
    /// override only their matching input tuples; TTY values come from the
    /// separate source-derived/unverified overlay, not a Native TTY capture.
    #[test]
    fn versioned_beta_expectations_keep_287_and_295_evidence_distinct() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/betas_2_1_287.json")).unwrap();
        assert_eq!(fixture["version"], "2.1.287");
        let print_overlay: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native_per_turn_control_2_1_295_print_44.json"
        ))
        .expect("captured Native 2.1.295 print fixture parses");
        let tty_overlay: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native_per_turn_control_2_1_295_tty_source_derived_44.json"
        ))
        .expect("source-derived Native 2.1.295 TTY overlay parses");
        assert_eq!(print_overlay["cases"].as_array().unwrap().len(), 44);
        assert_eq!(tty_overlay["cases"].as_array().unwrap().len(), 44);

        for case in fixture["cases"].as_array().unwrap() {
            let input = &case["input"];
            let provider = match input["provider"].as_str().unwrap() {
                "firstParty" => Provider::Anthropic,
                "foundry" => Provider::Foundry,
                "vertex" => Provider::Vertex,
                "bedrock" => Provider::Bedrock,
                _ => unreachable!(),
            };
            let bool_env = |key: &str| {
                input["env"][key]
                    .as_str()
                    .is_some_and(crate::structured_output::bool_value)
            };
            let env = BetaEnvironment {
                disabled:
                    crate::model::thinking_binding_controls::native_disable_experimental_betas(
                        input["env"]["CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"]
                            .as_str()
                            .map(std::ffi::OsStr::new),
                    ),
                interleaved_disabled: bool_env("DISABLE_INTERLEAVED_THINKING"),
                force_mid_system: bool_env("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM"),
            };
            let model = input["model"].as_str().unwrap();
            let context = BetaContext::for_model(model)
                .with_interactive(input["interactive"].as_bool().unwrap());
            let all = model_betas_in(provider, &context, env);
            let body: Vec<_> = all
                .iter()
                .filter(|beta| BEDROCK_EXTRA_PARAMS_HEADERS.contains(beta))
                .copied()
                .collect();
            let base: Vec<_> = all
                .iter()
                .filter(|beta| {
                    provider != Provider::Bedrock || !BEDROCK_EXTRA_PARAMS_HEADERS.contains(beta)
                })
                .copied()
                .collect();
            let count: Vec<_> = base
                .iter()
                .filter(|beta| COUNT_TOKENS_ALLOWED.contains(beta))
                .copied()
                .collect();
            assert_eq!(
                beta_canonical(model),
                case["expected"]["canonical"],
                "{case}"
            );
            assert_eq!(base.join(","), case["expected"]["base"], "{case}");
            assert_eq!(
                serde_json::to_value(body).unwrap(),
                case["expected"]["body"],
                "{case}"
            );
            assert_eq!(count.join(","), case["expected"]["count"], "{case}");
            let effort = effort_capable_in(
                provider,
                model,
                bool_env("CLAUDE_CODE_ALWAYS_ENABLE_EFFORT"),
            );
            assert_eq!(effort, case["expected"]["effort"], "{case}");
            let automatic = model_betas_in(provider, &context.with_effort(effort), env)
                .into_iter()
                .filter(|beta| {
                    provider != Provider::Bedrock || !BEDROCK_EXTRA_PARAMS_HEADERS.contains(beta)
                })
                .collect::<Vec<_>>()
                .join(",");
            let expected_main = default_main_from_versioned_overlay(
                input,
                &case["expected"]["default_main"],
                &print_overlay,
                &tty_overlay,
            );
            assert_eq!(automatic, expected_main, "{case}");
            assert_eq!(base.join(","), case["expected"]["explicit_main"], "{case}");
        }
    }

    fn default_main_from_versioned_overlay(
        input: &serde_json::Value,
        legacy_expected: &serde_json::Value,
        print_overlay: &serde_json::Value,
        tty_overlay: &serde_json::Value,
    ) -> String {
        if let Some(captured) = print_overlay["cases"]
            .as_array()
            .expect("print overlay cases")
            .iter()
            .find(|case| case.get("input") == Some(input))
        {
            let header = captured["captured"]["beta_header_line_utf8"]
                .as_str()
                .expect("captured raw Native header");
            return header
                .strip_prefix("anthropic-beta: ")
                .and_then(|value| value.strip_suffix("\r\n"))
                .expect("captured beta header framing")
                .to_owned();
        }
        if let Some(source_derived) = tty_overlay["cases"]
            .as_array()
            .expect("TTY overlay cases")
            .iter()
            .find(|case| case.get("input") == Some(input))
        {
            assert_eq!(
                source_derived["evidence_class"],
                "source-derived-unverified"
            );
            return source_derived["expected_default_main_beta_tokens"]
                .as_array()
                .expect("source-derived expected beta tokens")
                .iter()
                .map(|token| token.as_str().expect("beta token"))
                .collect::<Vec<_>>()
                .join(",");
        }
        legacy_expected
            .as_str()
            .expect("historical 2.1.287 default_main expectation")
            .to_owned()
    }

    #[test]
    fn admitted_native_request_betas_keep_capture_order() {
        let context = BetaContext::for_model("claude-sonnet-5-5")
            .with_request_kind(
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
            )
            .with_per_turn_control(true)
            .with_mid_conversation_tool_changes(true)
            .with_inline_tools(true)
            .with_effort(true)
            .with_dangerous_tool_use(true);
        let assembled =
            assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &context);
        let mut headers = std::collections::BTreeMap::new();
        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut headers,
            &[assembled],
        );
        let header = headers.get("anthropic-beta").unwrap().clone();
        let per_turn = header.find(PER_TURN_CONTROL).unwrap();
        let tool_changes = header.find(MID_CONVERSATION_TOOL_CHANGES).unwrap();
        let inline_tools = header.find(INLINE_TOOLS_BETA).unwrap();
        let effort = header.find(EFFORT).unwrap();
        let dangerous = header.find(DANGEROUS_TOOL_USE).unwrap();
        assert!(
            per_turn < tool_changes
                && tool_changes < inline_tools
                && inline_tools < effort
                && effort < dangerous,
            "{header}"
        );

        let mctc_only = BetaContext::for_model("claude-sonnet-5-5")
            .with_request_kind(
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
            )
            .with_mid_conversation_tool_changes(true);
        let mctc_only_header =
            assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &mctc_only);
        assert!(mctc_only_header.contains(MID_CONVERSATION_TOOL_CHANGES));
        assert!(!mctc_only_header.contains(INLINE_TOOLS_BETA));

        let orphaned_inline = mctc_only
            .clone()
            .with_inline_tools(true)
            .with_mid_conversation_tool_changes(false);
        let orphaned_inline_header = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &orphaned_inline,
        );
        assert!(!orphaned_inline_header.contains(INLINE_TOOLS_BETA));

        let foundry_header =
            assemble_beta_header(Provider::Foundry, Endpoint::MessagesCreate, &context);
        assert!(foundry_header.contains(MID_CONVERSATION_TOOL_CHANGES));
        assert!(foundry_header.contains(INLINE_TOOLS_BETA));
        assert!(foundry_header.contains(DANGEROUS_TOOL_USE));

        let side_query = BetaContext::for_model("claude-sonnet-5-5")
            .with_request_kind(
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery,
            )
            .with_mid_conversation_tool_changes(true)
            .with_dangerous_tool_use(true);
        let side_header =
            assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &side_query);
        assert!(!side_header.contains(MID_CONVERSATION_TOOL_CHANGES));
        assert!(!side_header.contains(INLINE_TOOLS_BETA));
        assert!(!side_header.contains(DANGEROUS_TOOL_USE));
        let count_header =
            assemble_beta_header(Provider::Anthropic, Endpoint::CountTokens, &context);
        assert!(!count_header.contains(MID_CONVERSATION_TOOL_CHANGES));
        assert!(!count_header.contains(INLINE_TOOLS_BETA));
        assert!(!count_header.contains(DANGEROUS_TOOL_USE));

        lingxi_llm_client::providers::anthropic::request_policy::merge_beta_header(
            &mut headers,
            &[lingxi_llm_client::providers::anthropic::thinking_display::UPDATES_BETA.to_owned()],
        );
        append_afk_mode_after_thinking_display(&mut headers, Provider::Anthropic, true);
        let ordered = headers.get("anthropic-beta").unwrap();
        let display = ordered
            .find(lingxi_llm_client::providers::anthropic::thinking_display::UPDATES_BETA)
            .unwrap();
        let afk = ordered.find(AFK_MODE).unwrap();
        assert!(dangerous < display && display < afk, "{ordered}");

        let mut foundry = std::collections::BTreeMap::new();
        append_afk_mode_after_thinking_display(&mut foundry, Provider::Foundry, true);
        assert_eq!(
            foundry.get("anthropic-beta").map(String::as_str),
            Some(AFK_MODE)
        );

        let mut vertex = std::collections::BTreeMap::new();
        append_afk_mode_after_thinking_display(&mut vertex, Provider::Vertex, true);
        assert_eq!(
            vertex.get("anthropic-beta").map(String::as_str),
            Some(AFK_MODE)
        );

        let mut bedrock = std::collections::BTreeMap::new();
        append_afk_mode_after_thinking_display(&mut bedrock, Provider::Bedrock, true);
        assert!(!bedrock.contains_key("anthropic-beta"));
        let body_betas = bedrock_extra_body_betas(
            &BetaContext::for_model("claude-opus-4-8").with_afk_mode(true),
        );
        assert!(body_betas.iter().any(|beta| beta == AFK_MODE));

        let mut not_admitted = std::collections::BTreeMap::new();
        append_afk_mode_after_thinking_display(&mut not_admitted, Provider::Anthropic, false);
        assert!(!not_admitted.contains_key("anthropic-beta"));
    }

    #[test]
    fn per_turn_control_precedes_the_general_effort_beta() {
        let context = BetaContext::for_model("claude-sonnet-5-5")
            .with_per_turn_control(true)
            .with_effort(true);
        let header = assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &context);
        let per_turn = header.find(PER_TURN_CONTROL).unwrap();
        let effort = header.find(EFFORT).unwrap();
        assert!(per_turn < effort, "{header}");
    }

    /// The current live constants are byte-exact against the Native registry.
    #[test]
    fn beta_constants_match_binary_byte_for_byte() {
        assert_eq!(CLAUDE_CODE_BETA, "claude-code-20250219");
        assert_eq!(INTERLEAVED_THINKING, "interleaved-thinking-2025-05-14");
        assert_eq!(CONTEXT_1M, "context-1m-2025-08-07");
        assert_eq!(CONTEXT_MANAGEMENT, "context-management-2025-06-27");
        assert_eq!(STRUCTURED_OUTPUTS, "structured-outputs-2025-12-15");
        assert_eq!(WEB_SEARCH, "web-search-2025-03-05");
        assert_eq!(ADVANCED_TOOL_USE_1P, "advanced-tool-use-2025-11-20");
        assert_eq!(TOOL_SEARCH_TOOL_3P, "tool-search-tool-2025-10-19");
        assert_eq!(EFFORT, "effort-2025-11-24");
        assert_eq!(CONTEXT_HINT, "context-hint-2026-04-09");
        assert_eq!(TASK_BUDGETS, "task-budgets-2026-03-13");
        assert_eq!(PROMPT_CACHING_SCOPE, "prompt-caching-scope-2026-01-05");
        assert_eq!(FAST_MODE, "fast-mode-2026-02-01");
        assert_eq!(REDACT_THINKING, "redact-thinking-2026-02-12");
        assert_eq!(THINKING_TOKEN_COUNT, "thinking-token-count-2026-05-13");
        assert_eq!(
            MID_CONVERSATION_SYSTEM,
            "mid-conversation-system-2026-04-07"
        );
        assert_eq!(PER_TURN_CONTROL, "per-turn-control-2026-07-01");
        assert_eq!(
            MID_CONVERSATION_TOOL_CHANGES,
            "mid-conversation-tool-changes-2026-07-01"
        );
        assert_eq!(INLINE_TOOLS_BETA, "inline-tools-2026-09-15");
        assert_eq!(DANGEROUS_TOOL_USE, "dangerous-tool-use-2026-09-03");
        assert_eq!(AFK_MODE, "afk-mode-2026-01-31");
        assert_eq!(ADVISOR_TOOL, "advisor-tool-2026-03-01");
        assert_eq!(OAUTH, "oauth-2025-04-20");
    }

    /// `token-efficient-tools` was fabricated — it must NOT be a live constant
    /// nor appear in any assembled header. (Regression lock for the over-emit fix.)
    #[test]
    fn fabricated_token_efficient_tools_is_gone() {
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(
            !h.contains("token-efficient-tools"),
            "fabricated token-efficient-tools must never be emitted; got: {h}"
        );
    }

    /// Default first-party Opus-4.8 (interactive, experimental on) emits exactly
    /// the `xLr` core set in declaration order — the ~7-beta faithful set, NOT
    /// the prior static 14.
    #[test]
    fn anthropic_opus48_emits_faithful_core_set() {
        // Ensure experimental gate is on for this assertion.
        if crate::model::thinking_binding_controls::native_b6_disabled()
            || env_truthy("DISABLE_INTERLEAVED_THINKING")
        {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        let expected = [
            CLAUDE_CODE_BETA,
            INTERLEAVED_THINKING,
            REDACT_THINKING,
            THINKING_TOKEN_COUNT,
            CONTEXT_MANAGEMENT,
            PROMPT_CACHING_SCOPE,
            MID_CONVERSATION_SYSTEM,
        ]
        .join(",");
        assert_eq!(h, expected, "faithful core set mismatch");
    }

    /// The previously over-emitted request-conditional betas must NOT appear on
    /// a default first-party messages.create (no web-search tool, no schema, no
    /// effort/task-budget/advisor, no `[1m]`, standard speed).
    #[test]
    fn over_emitted_betas_are_gone_by_default() {
        if crate::model::thinking_binding_controls::native_b6_disabled() {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        for gone in [
            WEB_SEARCH,
            STRUCTURED_OUTPUTS,
            CONTEXT_1M,
            EFFORT,
            TASK_BUDGETS,
            FAST_MODE,
            ADVISOR_TOOL,
            TOOL_SEARCH_TOOL_3P,
            ADVANCED_TOOL_USE_1P,
        ] {
            assert!(
                !h.split(',').any(|p| p == gone),
                "{gone} must not be emitted by default; got: {h}"
            );
        }
    }

    /// Haiku drops `claude-code-20250219` (the binary's `!isHaiku` gate).
    #[test]
    fn haiku_drops_claude_code_beta() {
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-haiku-4-5"),
        );
        assert!(
            !h.split(',').any(|p| p == CLAUDE_CODE_BETA),
            "Haiku must not carry claude-code beta; got: {h}"
        );
    }

    /// `mid-conversation-system` is gated by model: emitted for opus-4-8 /
    /// fable-5, NOT for the enumerated older models (sonnet-4-6, opus-4-7).
    #[test]
    fn mid_conversation_system_is_model_gated() {
        assert!(mid_conversation_system("claude-opus-4-8"));
        assert!(mid_conversation_system("claude-fable-5-1"));
        assert!(mid_conversation_system("claude-mythos-5-1"));
        // Native av/lr does not classify Sonnet 5 as an older model.
        assert!(mid_conversation_system("claude-sonnet-5"));
        assert!(!mid_conversation_system("claude-sonnet-4-6"));
        assert!(!mid_conversation_system("claude-sonnet-4-5"));
        assert!(!mid_conversation_system("claude-opus-4-7"));
        assert!(!mid_conversation_system("claude-3-5-sonnet"));
    }

    #[test]
    fn native_mid_conversation_system_matches_native_gate_order() {
        let supports = |model, capabilities| {
            native_mid_conversation_system_supported_with_inputs(
                model,
                true,
                false,
                false,
                None,
                capabilities,
            )
        };
        assert!(!supports("claude-sonnet-4-6", None));
        assert!(supports("claude-opus-4-8", None));
        assert!(supports("claude-mythos-5", None));
        assert!(supports("claude-mythos-5-1", None));
        assert!(supports("claude-sonnet-5-5", None));
        assert!(supports("claude-future-unknown", None));

        // Native QTe is evaluated before the Opus 4.8 age floor, so a matching
        // model-option capability may admit this older model explicitly.
        assert!(native_mid_conversation_system_supported_with_inputs(
            "claude-sonnet-4-6",
            true,
            false,
            false,
            Some(true),
            None,
        ));
        assert!(!native_mid_conversation_system_supported_with_inputs(
            "claude-opus-4-8",
            true,
            false,
            false,
            Some(false),
            None,
        ));
        assert!(!native_mid_conversation_system_supported_with_inputs(
            "claude-opus-4-8",
            true,
            false,
            false,
            None,
            Some("claude-opus-4-8=-mid_conv_system"),
        ));
        assert!(!native_mid_conversation_system_supported_with_inputs(
            "claude-sonnet-4-6",
            true,
            false,
            false,
            None,
            Some("claude-sonnet-4-6=mid_conv_system"),
        ));
        assert!(!native_mid_conversation_system_supported_with_inputs(
            "claude-sonnet-4-6",
            true,
            true,
            false,
            None,
            None,
        ));
        assert!(!native_mid_conversation_system_supported_with_inputs(
            "claude-sonnet-4-6",
            true,
            true,
            true,
            Some(true),
            Some("claude-sonnet-4-6=mid_conv_system"),
        ));
        assert!(native_mid_conversation_system_supported_with_inputs(
            "claude-sonnet-4-6",
            true,
            false,
            true,
            None,
            None,
        ));
        assert!(native_mid_conversation_system_supported_with_inputs(
            "claude-opus-4-8",
            false,
            false,
            false,
            Some(false),
            None,
        ));
    }

    #[test]
    fn native_tool_change_feature_gates_keep_mctc_and_inline_inputs_distinct() {
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, false, None, false),
            (true, false),
            "brisk-meadow is the unset inline override fallback"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, false, Some("true"), false),
            (true, true)
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, false, Some(" YES "), false,),
            (true, true),
            "the explicit override uses the existing structured_output::bool_value parser"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, false, Some("false"), true),
            (true, false),
            "an explicit false override wins over the server flag"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, false, Some("off"), true),
            (true, false),
            "unrecognized explicit values use the existing boolean helper's false result"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, true, true, Some("true"), true),
            (true, false),
            "a session profile disables only the additional inline gate"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(true, true, false, Some("true"), true),
            (false, false),
            "B6 suppresses both experimental beta gates"
        );
        assert_eq!(
            native_tool_change_feature_gates_for_inputs(false, false, false, Some("true"), true),
            (false, false),
            "the foamy-spring MCTC gate is required before inline tools"
        );
    }

    #[test]
    fn native_mid_conversation_tool_change_uses_explicit_known_and_unknown_order() {
        let evaluate = |model, force, unsupported, explicit, baked, known| {
            native_mid_conversation_tool_change_supported_with_inputs(
                model,
                force,
                unsupported,
                explicit,
                baked,
                known,
            )
        };
        assert!(evaluate("claude-opus-4-8", false, false, None, true, true));
        assert!(!evaluate(
            "claude-sonnet-4-6",
            false,
            false,
            None,
            false,
            true
        ));
        assert!(evaluate(
            "claude-future-model",
            false,
            false,
            None,
            false,
            false
        ));
        assert!(evaluate("claude-mythos-5", false, false, None, false, true));
        assert!(!evaluate(
            "claude-opus-4-8",
            false,
            true,
            Some(true),
            true,
            true
        ));
        assert!(evaluate(
            "claude-opus-4-8",
            true,
            true,
            Some(false),
            false,
            true
        ));
        assert!(!evaluate(
            "claude-opus-4-8",
            false,
            false,
            Some(false),
            true,
            true
        ));
    }

    #[test]
    fn native_mid_system_force_setting_uses_javascript_truthiness() {
        assert!(!native_environment_truthy(None));
        assert!(!native_environment_truthy(Some(std::ffi::OsStr::new(""))));
        assert!(native_environment_truthy(Some(std::ffi::OsStr::new(
            "false"
        ))));
        assert!(native_environment_truthy(Some(std::ffi::OsStr::new("0"))));
        assert!(native_environment_truthy(Some(std::ffi::OsStr::new(" "))));
    }

    #[test]
    fn native_model_option_capability_matches_native_order_and_parsing() {
        let env = [
            ("ANTHROPIC_DEFAULT_FABLE_MODEL", " "),
            (
                "ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES",
                "mid_conversation_system",
            ),
            ("ANTHROPIC_DEFAULT_OPUS_MODEL", " CLAUDE-OPUS-4-8 "),
            (
                "ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES",
                "FAST_MODE, MID_CONVERSATION_SYSTEM",
            ),
            ("ANTHROPIC_CUSTOM_MODEL_OPTION", "claude-opus-4-8"),
            (
                "ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES",
                "-mid_conversation_system",
            ),
        ];
        let lookup = |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
        };
        assert_eq!(
            native_model_option_capability_in(
                "claude-opus-4-8",
                true,
                "mid_conversation_system",
                lookup,
            ),
            Some(true)
        );
        assert_eq!(
            native_model_option_capability_in(
                "claude-opus-4-8",
                false,
                "mid_conversation_system",
                lookup,
            ),
            None
        );
        assert_eq!(
            native_model_option_capability_in(
                "other-model",
                true,
                "mid_conversation_system",
                lookup,
            ),
            None
        );
    }

    /// `context-1m` rides only when the model id carries `[1m]`.
    #[test]
    fn context_1m_only_when_model_marked() {
        if crate::model::thinking_binding_controls::native_b6_disabled() {
            return;
        }
        let off = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-sonnet-4-6"),
        );
        assert!(!off.split(',').any(|p| p == CONTEXT_1M));
        let on = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-sonnet-4-6[1m]"),
        );
        assert!(on.split(',').any(|p| p == CONTEXT_1M), "got: {on}");
    }

    /// `redact-thinking` rides only in interactive mode without thinking summaries.
    #[test]
    fn redact_thinking_gated_on_interactive() {
        if crate::model::thinking_binding_controls::native_b6_disabled() {
            return;
        }
        let interactive = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(interactive.split(',').any(|p| p == REDACT_THINKING));

        let headless = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_interactive(false),
        );
        assert!(
            !headless.split(',').any(|p| p == REDACT_THINKING),
            "got: {headless}"
        );

        let summarized = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_show_thinking_summaries(true),
        );
        assert!(
            !summarized.split(',').any(|p| p == REDACT_THINKING),
            "got: {summarized}"
        );
    }

    /// `fast-mode` rides only when the request sets speed:"fast".
    #[test]
    fn fast_mode_gated_on_request() {
        let off = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        assert!(!off.split(',').any(|p| p == FAST_MODE));
        let on = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8").with_fast_mode(true),
        );
        assert!(on.split(',').any(|p| p == FAST_MODE), "got: {on}");
    }

    /// count_tokens keeps only the `ERr` whitelist (claude-code,
    /// interleaved-thinking, context-management) — never thinking-token-count,
    /// redact-thinking, prompt-caching-scope, mid-conversation-system.
    #[test]
    fn count_tokens_filters_to_err_whitelist() {
        if crate::model::thinking_binding_controls::native_b6_disabled()
            || env_truthy("DISABLE_INTERLEAVED_THINKING")
        {
            return;
        }
        let h = assemble_beta_header(
            Provider::Anthropic,
            Endpoint::CountTokens,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        let expected = [CLAUDE_CODE_BETA, INTERLEAVED_THINKING, CONTEXT_MANAGEMENT].join(",");
        assert_eq!(h, expected, "count_tokens must equal the ERr-filtered set");
        for excluded in [
            REDACT_THINKING,
            THINKING_TOKEN_COUNT,
            PROMPT_CACHING_SCOPE,
            MID_CONVERSATION_SYSTEM,
        ] {
            assert!(
                !h.split(',').any(|p| p == excluded),
                "{excluded} leaked into count_tokens: {h}"
            );
        }
    }

    /// COUNT_TOKENS_ALLOWED is exactly the binary's 4-entry `ERr` set
    /// (now including OAUTH).
    #[test]
    fn count_tokens_whitelist_is_err_set() {
        assert_eq!(COUNT_TOKENS_ALLOWED.len(), 4);
        for b in [
            CLAUDE_CODE_BETA,
            INTERLEAVED_THINKING,
            CONTEXT_MANAGEMENT,
            OAUTH,
        ] {
            assert!(
                COUNT_TOKENS_ALLOWED.contains(&b),
                "{b} missing from ERr whitelist"
            );
        }
    }

    #[test]
    fn bedrock_extra_params_headers_is_exactly_three() {
        assert_eq!(BEDROCK_EXTRA_PARAMS_HEADERS.len(), 3);
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&INTERLEAVED_THINKING));
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&CONTEXT_1M));
        assert!(BEDROCK_EXTRA_PARAMS_HEADERS.contains(&TOOL_SEARCH_TOOL_3P));
    }

    /// Bedrock drops the `bRr` header-only entries (interleaved-thinking etc.).
    #[test]
    fn bedrock_excludes_b_rr_entries() {
        let h = assemble_beta_header(
            Provider::Bedrock,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );
        for excluded in BEDROCK_EXTRA_PARAMS_HEADERS {
            assert!(
                !h.split(',').any(|p| p == *excluded),
                "Bedrock must not carry {excluded}; got: {h}"
            );
        }
    }

    /// The `context_hint` body key must actually light its beta header.
    ///
    /// Without this, deleting the push compiles and every other test passes —
    /// the request would carry the body key and no header, which the server
    /// rejects as an unknown field rather than negotiating.
    #[test]
    fn context_hint_body_lights_the_beta_header() {
        let model = "claude-opus-4-6";
        let off = model_betas(Provider::Anthropic, &BetaContext::for_model(model));
        assert!(
            !off.contains(&CONTEXT_HINT),
            "no `context_hint` in the body ⇒ no beta: {off:?}"
        );
        let on = model_betas(
            Provider::Anthropic,
            &BetaContext::for_model(model).with_context_hint(true),
        );
        assert!(
            on.contains(&CONTEXT_HINT),
            "`context_hint` in the body ⇒ the beta rides along: {on:?}"
        );
    }

    #[test]
    fn tool_search_uses_provider_specific_beta_locations() {
        if crate::model::thinking_binding_controls::native_b6_disabled() {
            return;
        }
        let ctx = BetaContext::for_model("claude-sonnet-4-5").with_tool_search(true);
        let first_party = assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &ctx);
        assert!(first_party
            .split(',')
            .any(|beta| beta == ADVANCED_TOOL_USE_1P));

        let vertex = assemble_beta_header(Provider::Vertex, Endpoint::MessagesCreate, &ctx);
        assert!(vertex.split(',').any(|beta| beta == TOOL_SEARCH_TOOL_3P));
        assert!(!vertex.split(',').any(|beta| beta == ADVANCED_TOOL_USE_1P));

        let bedrock = bedrock_extra_body_betas(&ctx);
        assert!(bedrock.iter().any(|beta| beta == TOOL_SEARCH_TOOL_3P));
        assert!(!bedrock.iter().any(|beta| beta == ADVANCED_TOOL_USE_1P));
    }

    #[test]
    fn native_tool_search_beta_falls_back_when_native_vh_is_false() {
        let context = BetaContext::for_model("claude-sonnet-4-5").with_tool_search(true);
        for provider in [Provider::Anthropic, Provider::Foundry] {
            let betas = model_betas_in(
                provider,
                &context,
                BetaEnvironment {
                    disabled: false,
                    ..BetaEnvironment::default()
                },
            );
            assert!(betas.contains(&ADVANCED_TOOL_USE_1P));
            assert!(!betas.contains(&TOOL_SEARCH_TOOL_3P));

            let betas = model_betas_in(
                provider,
                &context,
                BetaEnvironment {
                    disabled: true,
                    ..BetaEnvironment::default()
                },
            );
            assert!(!betas.contains(&ADVANCED_TOOL_USE_1P));
            assert!(betas.contains(&TOOL_SEARCH_TOOL_3P));
        }

        for provider in [Provider::Vertex, Provider::Bedrock] {
            for disabled in [false, true] {
                let betas = model_betas_in(
                    provider,
                    &context,
                    BetaEnvironment {
                        disabled,
                        ..BetaEnvironment::default()
                    },
                );
                assert!(!betas.contains(&ADVANCED_TOOL_USE_1P));
                assert!(betas.contains(&TOOL_SEARCH_TOOL_3P));
            }
        }
    }

    /// A pre-existing `anthropic-beta` value (e.g. auth-injected oauth) is
    /// preserved and the assembled set appended after it (no clobber, no dup).
    #[test]
    fn apply_beta_header_preserves_existing_values() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            lingxi_llm_client::protocol::JsonPayload::parse(
                r#"{"model":"claude-opus-4-8","max_tokens":1024}"#,
            )
            .unwrap(),
        );
        req.headers
            .insert("anthropic-beta".to_string(), "oauth-2025-04-20".to_string());

        apply_beta_header(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
        );

        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(value.starts_with("oauth-2025-04-20,"), "got: {value}");
        assert_eq!(
            value
                .split(',')
                .filter(|p| *p == "oauth-2025-04-20")
                .count(),
            1
        );
    }

    /// Subscriber path appends oauth exactly once alongside the model betas.
    #[test]
    fn oauth_beta_appended_for_subscriber() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            lingxi_llm_client::protocol::JsonPayload::parse(
                r#"{"model":"claude-opus-4-8","max_tokens":1024}"#,
            )
            .unwrap(),
        );
        apply_beta_header_with_auth(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
            true,
        );
        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(
            value.split(',').any(|p| p == CLAUDE_CODE_BETA),
            "got: {value}"
        );
        assert!(value.split(',').any(|p| p == OAUTH), "got: {value}");
        assert_eq!(value.split(',').filter(|p| *p == OAUTH).count(), 1);
    }

    /// Non-subscriber path does NOT add oauth.
    #[test]
    fn oauth_beta_absent_for_non_subscriber() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            lingxi_llm_client::protocol::JsonPayload::parse(
                r#"{"model":"claude-opus-4-8","max_tokens":1024}"#,
            )
            .unwrap(),
        );
        apply_beta_header_with_auth(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-opus-4-8"),
            false,
        );
        let value = req.headers.get("anthropic-beta").expect("header present");
        assert!(!value.split(',').any(|p| p == OAUTH), "got: {value}");
    }

    #[test]
    fn custom_cli_betas_apply_to_stream_and_are_rejected_for_oauth() {
        let make = || {
            crate::ProviderRequest::post_json(
                "https://api.anthropic.com/v1/messages",
                lingxi_llm_client::protocol::JsonPayload::parse(
                    r#"{"model":"claude-opus-4-8","max_tokens":1024}"#,
                )
                .unwrap(),
            )
        };
        let custom = vec!["example-beta-1".to_string(), "example-beta-2".to_string()];

        let mut api_key_req = make();
        apply_beta_header_with_auth_and_custom(
            &mut api_key_req,
            Provider::Anthropic,
            Endpoint::MessagesCreateStream,
            &BetaContext::for_model("claude-opus-4-8"),
            false,
            &custom,
        );
        let header = api_key_req.headers["anthropic-beta"].as_str();
        assert!(header.split(',').any(|part| part == "example-beta-1"));
        assert!(header.split(',').any(|part| part == "example-beta-2"));

        let mut oauth_req = make();
        apply_beta_header_with_auth_and_custom(
            &mut oauth_req,
            Provider::Anthropic,
            Endpoint::MessagesCreateStream,
            &BetaContext::for_model("claude-opus-4-8"),
            true,
            &custom,
        );
        let header = oauth_req.headers["anthropic-beta"].as_str();
        assert!(!header.split(',').any(|part| part == "example-beta-1"));
        assert!(header.split(',').any(|part| part == OAUTH));
    }

    #[test]
    fn native_cli_betas_project_to_all_native_provider_headers_after_route_filtering() {
        use crate::model::thinking_binding_controls::{
            native_cli_beta_additions, ProviderRoute as NativeProviderRoute,
        };

        let cases = [
            (
                Provider::Anthropic,
                NativeProviderRoute::FirstParty,
                Endpoint::MessagesCreate,
                vec![
                    "cli-private-beta".to_owned(),
                    "thinking-display-updates-2026-08-18".to_owned(),
                ],
            ),
            (
                Provider::Foundry,
                NativeProviderRoute::Foundry,
                Endpoint::MessagesCreateStream,
                vec![
                    "cli-private-beta".to_owned(),
                    "thinking-display-updates-2026-08-18".to_owned(),
                ],
            ),
            (
                Provider::Bedrock,
                NativeProviderRoute::Bedrock,
                Endpoint::MessagesCreate,
                vec!["thinking-display-updates-2026-08-18".to_owned()],
            ),
            (
                Provider::Vertex,
                NativeProviderRoute::Vertex,
                Endpoint::MessagesCreateStream,
                vec!["thinking-display-updates-2026-08-18".to_owned()],
            ),
        ];
        let requested = vec![
            "cli-private-beta".to_owned(),
            "thinking-display-updates-2026-08-18".to_owned(),
        ];

        for (provider, native_route, endpoint, expected_custom) in cases {
            let filtered = native_cli_beta_additions(native_route, false, false, &requested);
            assert_eq!(filtered, expected_custom, "{provider:?} CLI filter");

            let mut request = crate::ProviderRequest::post_json(
                "https://provider.example/v1/messages",
                lingxi_llm_client::protocol::JsonPayload::parse(
                    r#"{"model":"claude-sonnet-4-5","max_tokens":1024}"#,
                )
                .unwrap(),
            );
            apply_beta_header_with_auth_and_custom(
                &mut request,
                provider,
                endpoint,
                &BetaContext::for_model("claude-sonnet-4-5"),
                false,
                &filtered,
            );

            let header = request
                .headers
                .get("anthropic-beta")
                .expect("Native Anthropic provider beta header");
            for beta in expected_custom {
                assert!(
                    header.split(',').any(|part| part == beta.as_str()),
                    "{provider:?} missing CLI beta {beta}: {header}"
                );
            }
            assert_eq!(
                header
                    .split(',')
                    .filter(|part| *part == "thinking-display-updates-2026-08-18")
                    .count(),
                1,
                "{provider:?} must deduplicate the CLI suffix"
            );
            if matches!(provider, Provider::Bedrock | Provider::Vertex) {
                assert!(
                    !header.split(',').any(|part| part == "cli-private-beta"),
                    "{provider:?} must keep the Native static allowlist filter"
                );
            }
            assert_eq!(
                request.body_json.raw_source_at("/anthropic_beta"),
                None,
                "CLI header projection does not synthesize a Bedrock body-beta field"
            );
        }
    }

    /// `apply_beta_header` on a clean request equals `assemble_beta_header`.
    #[test]
    fn apply_beta_header_inserts_assembled_header() {
        let mut req = crate::ProviderRequest::post_json(
            "https://api.anthropic.com/v1/messages",
            lingxi_llm_client::protocol::JsonPayload::parse(
                r#"{"model":"claude-opus-4-8","max_tokens":1024}"#,
            )
            .unwrap(),
        );
        assert!(!req.headers.contains_key("anthropic-beta"));
        let ctx = BetaContext::for_model("claude-opus-4-8");
        apply_beta_header(
            &mut req,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &ctx,
        );
        let expected = assemble_beta_header(Provider::Anthropic, Endpoint::MessagesCreate, &ctx);
        assert_eq!(
            req.headers.get("anthropic-beta").map(String::as_str),
            Some(expected.as_str())
        );
    }
}

#[cfg(test)]
mod native_per_turn_beta_tests {
    use super::*;
    #[test]
    fn encoder_discovered_per_turn_beta_keeps_native_model_order() {
        let mut request = crate::ProviderRequest::post_json(
            "http://configured-base.test/v1/messages",
            lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
        );
        request
            .headers
            .insert("anthropic-beta".into(), PER_TURN_CONTROL.into());
        apply_beta_header(
            &mut request,
            Provider::Anthropic,
            Endpoint::MessagesCreate,
            &BetaContext::for_model("claude-sonnet-5-5").with_effort(true),
        );
        let betas: Vec<_> = request.headers["anthropic-beta"].split(',').collect();
        let at = |name| betas.iter().position(|value| *value == name).unwrap();
        assert!(at(MID_CONVERSATION_SYSTEM) < at(PER_TURN_CONTROL));
        assert!(at(PER_TURN_CONTROL) < at(EFFORT));
        assert_eq!(
            betas
                .iter()
                .filter(|value| **value == PER_TURN_CONTROL)
                .count(),
            1
        );
        let side=BetaContext::for_model("claude-sonnet-5-5").with_effort(true).with_request_kind(lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::SideQuery);
        assert!(!model_betas(Provider::Anthropic, &side).contains(&PER_TURN_CONTROL));
    }
}

#[cfg(test)]
mod native_per_turn_disabled_beta_tests {
    use super::*;

    #[test]
    fn disabled_experimental_betas_suppresses_automatic_per_turn_control_only() {
        let context = BetaContext::for_model("claude-sonnet-5-5")
            .with_effort(true)
            .with_request_kind(
                lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
            );
        for provider in [Provider::Anthropic, Provider::Foundry] {
            let betas = model_betas_in(
                provider,
                &context,
                BetaEnvironment {
                    disabled: true,
                    ..BetaEnvironment::default()
                },
            );
            assert!(
                !betas.contains(&PER_TURN_CONTROL),
                "{provider:?}: {betas:?}"
            );
            assert!(betas.contains(&EFFORT), "{provider:?}: {betas:?}");
        }
    }
}

#[cfg(test)]
mod native_per_turn_control_295_capture_tests {
    use super::*;
    use serde_json::Value;

    fn captured_bool_env(input: &Value, key: &str) -> bool {
        input["env"][key]
            .as_str()
            .is_some_and(crate::structured_output::bool_value)
    }

    #[test]
    fn model_betas_in_matches_native_2_1_295_public_print_header_bytes() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/native_per_turn_control_2_1_295_print_44.json"
        ))
        .expect("captured Native fixture parses");
        assert_eq!(fixture["native"]["version"], "2.1.295");
        let cases = fixture["cases"].as_array().expect("capture fixture cases");
        assert_eq!(cases.len(), 44, "public-print capture count");

        let mut mismatches = Vec::new();
        for case in cases {
            let input = &case["input"];
            assert_eq!(case["mode"], "public-print", "{case}");
            assert_eq!(input["interactive"], false, "{case}");
            let provider = match input["provider"].as_str().expect("provider") {
                "firstParty" => Provider::Anthropic,
                "foundry" => Provider::Foundry,
                other => panic!("unexpected captured provider {other:?}"),
            };
            let model = input["model"].as_str().expect("request model");
            let captured = &case["captured"];
            let has_effort = captured["top_level_output_config_effort"]
                .as_str()
                .is_some();
            let has_per_message_effort = captured["per_message_effort_markers"]
                .as_array()
                .is_some_and(|rows| !rows.is_empty());
            let context = BetaContext::for_model(model)
                .with_interactive(false)
                .with_effort(has_effort)
                .with_per_turn_control(has_per_message_effort)
                .with_request_kind(
                    lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
                );
            let environment = BetaEnvironment {
                disabled:
                    crate::model::thinking_binding_controls::native_disable_experimental_betas(
                        input["env"]["CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS"]
                            .as_str()
                            .map(std::ffi::OsStr::new),
                    ),
                interleaved_disabled: captured_bool_env(input, "DISABLE_INTERLEAVED_THINKING"),
                force_mid_system: captured_bool_env(
                    input,
                    "CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM",
                ),
            };

            // The production header merge keeps first occurrence order and
            // removes duplicates from the automatic + encoder-discovered path.
            let mut actual_tokens = Vec::new();
            for beta in model_betas_in(provider, &context, environment) {
                if !actual_tokens.contains(&beta) {
                    actual_tokens.push(beta);
                }
            }
            let actual_line = format!("anthropic-beta: {}\r\n", actual_tokens.join(","));
            let expected_line = captured["beta_header_line_utf8"]
                .as_str()
                .expect("raw captured header line");
            let captured_per_turn_count = actual_tokens
                .iter()
                .filter(|beta| **beta == PER_TURN_CONTROL)
                .count();
            let expected_per_turn_count = captured["per_turn_control_header_count"]
                .as_u64()
                .expect("captured per-turn token count")
                as usize;
            if actual_line.as_bytes() != expected_line.as_bytes()
                || captured_per_turn_count != expected_per_turn_count
            {
                mismatches.push(format!(
                    "row {} ({:?}, {:?}, env={:?}): actual={:?}, Native raw={:?}",
                    case["matrix_row"],
                    input["provider"],
                    input["model"],
                    input["env"],
                    actual_line,
                    expected_line,
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "Native 2.1.295 beta header mismatches:\n{}",
            mismatches.join("\n")
        );
    }
}
