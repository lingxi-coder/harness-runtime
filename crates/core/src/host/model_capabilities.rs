//! Current Claude Code 2.1.319 baked model capabilities. Native model/env/served
//! lookups are separate consumers: an absent baked capability is not an explicit
//! negative observation. The current table preserves every capability string.

/// A model capability, spelled exactly as the oracle's wire string.
///
/// Only the variants the port actually consults are enumerated; the rest of
/// each model's list is preserved in [`capabilities_for`] so adding a consumer
/// later is a lookup, not another archaeology pass over the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelCapability {
    /// Model accepts `output_config.effort`.
    Effort,
    /// Model accepts the `max` effort level.
    MaxEffort,
    /// Model accepts the `xhigh` effort level.
    XHighEffort,
    /// Model supports adaptive thinking.
    AdaptiveThinking,
    /// Model takes the LEAN (short) system prompt — the 2.1.219+ "new rules of
    /// context engineering" path.
    LeanPrompt,
    /// Model supports a mid-conversation system-prompt swap.
    MidConvSystem,
    /// Model supports mid-conversation tool changes.
    MidConvToolChange,
    /// Model participates in refusal-fallback routing.
    RefusalFallback,
    /// Model ships the Opus-5 prompt bundle.
    Opus5PromptBundle,
    /// Model ships the Fable-5 mitigations.
    Fable5Mitigations,
    /// Model supports fast mode.
    FastMode,
}

impl ModelCapability {
    /// The oracle's wire string for this capability.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Effort => "effort",
            Self::MaxEffort => "max_effort",
            Self::XHighEffort => "xhigh_effort",
            Self::AdaptiveThinking => "adaptive_thinking",
            Self::LeanPrompt => "lean_prompt",
            Self::MidConvSystem => "mid_conv_system",
            Self::MidConvToolChange => "mid_conv_tool_change",
            Self::RefusalFallback => "refusal_fallback",
            Self::Opus5PromptBundle => "opus_5_prompt_bundle",
            Self::Fable5Mitigations => "fable_5_mitigations",
            Self::FastMode => "fast_mode",
        }
    }
}

/// Current native baked capability array for a canonical model id.
/// Unknown identities have no baked observations.
#[must_use]
pub fn capabilities_for(model_id: &str) -> &'static [&'static str] {
    match model_id {
        "claude-3-5-haiku" => &["total_tokens_reminder", "tool_loaded_text"],
        "claude-haiku-4-5" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-haiku-5-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_tool_change",
            "context_management",
            "rejects_disabled_thinking",
            "per_turn_effort",
            "lean_prompt",
            "org_locked_thinking",
            "haiku_5_5_early_stopping_guidance",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-3-5-sonnet" => &["total_tokens_reminder", "tool_loaded_text"],
        "claude-3-7-sonnet" => &["total_tokens_reminder", "tool_loaded_text"],
        "claude-sonnet-4-0" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-sonnet-4-5" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-sonnet-4-6" => &[
            "effort",
            "max_effort",
            "adaptive_thinking",
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-sonnet-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-sonnet-5-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_tool_change",
            "context_management",
            "rejects_disabled_thinking",
            "per_turn_effort",
            "per_turn_timing",
            "lean_prompt",
            "refusal_fallback",
            "silent_turn_reminder",
            "org_locked_thinking",
            "tool_loaded_text",
        ],
        "claude-opus-4-0" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-4-1" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-4-5" => &[
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-4-6" => &[
            "effort",
            "max_effort",
            "adaptive_thinking",
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-4-7" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "context_management",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-4-8" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_tool_change",
            "context_management",
            "fast_mode",
            "lean_prompt",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_tool_change",
            "context_management",
            "thinking_disabled_effort_cap",
            "fast_mode",
            "lean_prompt",
            "refusal_fallback",
            "opus_5_prompt_bundle",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-opus-5-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "rejects_disabled_thinking",
            "mid_conv_tool_change",
            "per_turn_effort",
            "per_turn_timing",
            "context_management",
            "fast_mode",
            "lean_prompt",
            "refusal_fallback",
            "opus_5_5_prompt_bundle",
            "tool_loaded_text",
        ],
        "claude-fable-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "rejects_disabled_thinking",
            "mid_conv_tool_change",
            "context_management",
            "lean_prompt",
            "fable_5_mitigations",
            "refusal_fallback",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-fable-5-1" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "rejects_disabled_thinking",
            "mid_conv_tool_change",
            "per_turn_effort",
            "per_turn_timing",
            "context_management",
            "lean_prompt",
            "fable_5_mitigations",
            "refusal_fallback",
            "fable_5_1_prompt_bundle",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        "claude-mythos-5" => &["total_tokens_reminder", "tool_loaded_text"],
        "claude-mythos-5-1" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "rejects_disabled_thinking",
            "mid_conv_tool_change",
            "per_turn_timing",
            "context_management",
            "lean_prompt",
            "fable_5_mitigations",
            "fable_5_1_prompt_bundle",
            "total_tokens_reminder",
            "tool_loaded_text",
        ],
        _ => &[],
    }
}

const KNOWN_MODEL_IDS: &[&str] = &[
    "claude-3-5-sonnet",
    "claude-3-7-sonnet",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5-5",
    "claude-mythos-5-1",
    "claude-3-5-haiku",
    "claude-haiku-4-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
    "claude-sonnet-5",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5-5",
    "claude-mythos-5",
    "claude-fable-5",
    "claude-opus-5",
    "claude-3-opus",
    "claude-3-sonnet",
    "claude-3-haiku",
];

/// Exact model ids in the pinned 2.1.319 baked catalog. Transport aliases
/// are normalized independently from the entries' capability observations.
const NATIVE_BAKED_MODEL_CATALOG_IDS: &[&str] = &[
    "claude-3-5-haiku",
    "claude-haiku-4-5",
    "claude-haiku-5-5",
    "claude-3-5-sonnet",
    "claude-3-7-sonnet",
    "claude-sonnet-4-0",
    "claude-sonnet-4-5",
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-sonnet-5-5",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-opus-5-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];

fn catalog_canonical_id(model_id: &str) -> Option<&'static str> {
    match model_id {
        "claude-3-5-haiku-20241022"
        | "claude-3-5-haiku-20241022-v1:0"
        | "claude-3-5-haiku@20241022" => Some("claude-3-5-haiku"),
        "claude-haiku-4-5-20251001"
        | "claude-haiku-4-5-20251001-v1:0"
        | "claude-haiku-4-5@20251001" => Some("claude-haiku-4-5"),
        "claude-3-5-sonnet-20241022"
        | "claude-3-5-sonnet-20241022-v2:0"
        | "claude-3-5-sonnet-v2@20241022" => Some("claude-3-5-sonnet"),
        "claude-3-7-sonnet-20250219"
        | "claude-3-7-sonnet-20250219-v1:0"
        | "claude-3-7-sonnet@20250219" => Some("claude-3-7-sonnet"),
        "claude-sonnet-4"
        | "claude-sonnet-4-20250514"
        | "claude-sonnet-4-20250514-v1:0"
        | "claude-sonnet-4@20250514" => Some("claude-sonnet-4-0"),
        "claude-sonnet-4-5-20250929"
        | "claude-sonnet-4-5-20250929-v1:0"
        | "claude-sonnet-4-5@20250929" => Some("claude-sonnet-4-5"),
        "claude-opus-4"
        | "claude-opus-4-20250514"
        | "claude-opus-4-20250514-v1:0"
        | "claude-opus-4@20250514" => Some("claude-opus-4-0"),
        "claude-opus-4-1-20250805"
        | "claude-opus-4-1-20250805-v1:0"
        | "claude-opus-4-1@20250805" => Some("claude-opus-4-1"),
        "claude-opus-4-5-20251101"
        | "claude-opus-4-5-20251101-v1:0"
        | "claude-opus-4-5@20251101" => Some("claude-opus-4-5"),
        "claude-opus-4-6-v1" => Some("claude-opus-4-6"),
        _ => None,
    }
}

fn known_wrapper_suffix(suffix: &str) -> bool {
    if suffix.is_empty() || suffix == "-eap" {
        return true;
    }
    if let Some(date) = suffix.strip_prefix('@') {
        return date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit());
    }
    if let Some(version) = suffix.strip_prefix("-v") {
        let mut parts = version.split(':');
        return parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            && parts
                .next()
                .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
            && parts.next().is_none();
    }
    if let Some(date) = suffix.strip_prefix('-') {
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
        if let Some((date, version)) = date.split_once("-v") {
            let mut version_parts = version.split(':');
            return date.len() == 8
                && date.bytes().all(|b| b.is_ascii_digit())
                && version_parts.next().is_some_and(|part| {
                    !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                })
                && version_parts.next().is_some_and(|part| {
                    !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())
                })
                && version_parts.next().is_none();
        }
    }
    false
}

fn claude_transport_candidate(model_id: &str) -> Option<&str> {
    if model_id.starts_with("claude-") {
        return Some(model_id);
    }
    if let Some(candidate) = model_id.strip_prefix("anthropic.") {
        return candidate.starts_with("claude-").then_some(candidate);
    }
    let (region, candidate) = model_id.split_once(".anthropic.")?;
    matches!(region, "us" | "eu" | "apac" | "global")
        .then_some(candidate)
        .filter(|candidate| candidate.starts_with("claude-"))
}

/// Strip the decorations a model id can carry before a registry lookup:
/// a provider/profile prefix (`openrouter/anthropic/…`), a `[1m]` context
/// suffix, an `-eap` early-access suffix, and the standard dated/cloud
/// transport wrappers around a canonical Claude id.
#[must_use]
pub fn normalize_model_id(model_id: &str) -> String {
    let bare = model_id.rsplit('/').next().unwrap_or(model_id);
    let bare = bare.split('[').next().unwrap_or(bare);
    let lower = bare.trim().to_ascii_lowercase();
    let Some(candidate) = claude_transport_candidate(&lower) else {
        return lower;
    };
    let without_eap = candidate.strip_suffix("-eap").unwrap_or(candidate);
    if let Some(canonical) = catalog_canonical_id(without_eap) {
        return canonical.to_owned();
    }
    KNOWN_MODEL_IDS
        .iter()
        .find(|known| {
            without_eap
                .strip_prefix(**known)
                .is_some_and(known_wrapper_suffix)
        })
        .map_or_else(|| without_eap.to_string(), |known| (*known).to_string())
}

/// [`capabilities_for`] after [`normalize_model_id`].
#[must_use]
pub fn capabilities_for_loose(model_id: &str) -> &'static [&'static str] {
    capabilities_for(&normalize_model_id(model_id))
}

/// Whether the normalized identity has an entry in Native's baked model
/// catalog. This distinguishes an absent capability on a known model from an
/// unknown model whose Native fallback is capability-enabled.
#[must_use]
pub fn is_known_model_id(model_id: &str) -> bool {
    let canonical = normalize_model_id(model_id);
    NATIVE_BAKED_MODEL_CATALOG_IDS.contains(&canonical.as_str())
}

/// Does `model_id` carry `capability`? Oracle `LN(model, capability)`.
///
/// An unknown model is `false` for everything — the conservative answer.
#[must_use]
pub fn has_capability(model_id: &str, capability: ModelCapability) -> bool {
    capabilities_for_loose(model_id).contains(&capability.as_wire())
}

/// Native `Rw`: an explicit environment or served denial differs from absence.
#[must_use]
pub fn prompt_capability(
    model_id: &str,
    capability: &str,
    environment: Option<&str>,
    served: Option<bool>,
) -> Option<bool> {
    let canonical = normalize_model_id(model_id);
    capability_override(&canonical, capability, environment)
        .or(served)
        .or_else(|| {
            capabilities_for(&canonical)
                .contains(&capability)
                .then_some(true)
        })
}

/// Native `jx` defaults, with route-owned served and client-data observations.
#[must_use]
pub fn prompt_feature_enabled(
    model_id: &str,
    capability: &str,
    environment: Option<&str>,
    served: Option<bool>,
    client_data: Option<bool>,
) -> bool {
    if let Some(observed) = prompt_capability(model_id, capability, environment, served) {
        return observed;
    }
    let family_default =
        (matches!(
            capability,
            "turn_updates"
                | "bash_output_audience_note"
                | "silent_turn_reminder"
                | "thinking_display_updates"
        ) && prompt_capability(model_id, "fable_5_1_prompt_bundle", environment, None)
            == Some(true))
            || (matches!(capability, "bison_cairn" | "larch_cistern")
                && prompt_capability(model_id, "opus_5_prompt_bundle", environment, None)
                    == Some(true))
            || (matches!(
                capability,
                "silent_turn_reminder" | "quizzical_shore" | "original_games_guidance"
            ) && prompt_capability(model_id, "opus_5_5_prompt_bundle", environment, None)
                == Some(true));
    client_data.unwrap_or(family_default)
}

/// Native `_t`/`ce`, after the owning route has resolved the canonical model.
#[must_use]
pub fn claude_lean_prompt_allowed(
    model_id: &str,
    environment: Option<&str>,
    served: Option<bool>,
    simple_override: Option<&str>,
    configured_simple: bool,
) -> bool {
    if model_id.is_empty() {
        return false;
    }
    if super::env::is_env_truthy(simple_override) {
        return true;
    }
    if super::env::is_env_defined_falsy(simple_override) {
        return false;
    }
    if let Some(observed) = prompt_capability(model_id, "lean_prompt", environment, served) {
        return observed || configured_simple;
    }
    let canonical = normalize_model_id(model_id);
    let standard = canonical.contains("claude-3-")
        || canonical.contains("haiku")
        || canonical.contains("sonnet")
        || matches!(
            canonical.as_str(),
            "claude-opus-4-0"
                | "claude-opus-4-1"
                | "claude-opus-4-5"
                | "claude-opus-4-6"
                | "claude-opus-4-7"
        );
    !standard || configured_simple
}

/// Session environment defaults. Route-level served observations use the pure gate.
#[must_use]
pub fn prompt_capabilities_environment() -> Option<String> {
    super::env::var_with_legacy(
        branding::MODEL_CAPABILITIES_ENV,
        "CLAUDE_CODE_MODEL_CAPABILITIES",
    )
}

/// Session environment defaults. Route-level served observations use the pure gate.
#[must_use]
pub fn claude_lean_prompt_for(model_id: &str) -> bool {
    let environment = prompt_capabilities_environment();
    let simple = std::env::var("LINGXI_SIMPLE_SYSTEM_PROMPT")
        .or_else(|_| std::env::var("CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT"))
        .ok();
    claude_lean_prompt_allowed(
        model_id,
        environment.as_deref(),
        None,
        simple.as_deref(),
        false,
    )
}

/// Observations supplied to Native's `ie` temporal-hare selection.
#[derive(Debug, Clone, Copy, Default)]
pub struct LeanRewriteInputs<'a> {
    pub environment: Option<&'a str>,
    pub served: Option<bool>,
    pub client_data: Option<bool>,
    pub compiled_only: bool,
    pub lean_prompt: bool,
    pub cohort: bool,
    pub feature_enabled: bool,
}

#[must_use]
pub fn lean_rewrite_allowed(model_id: &str, input: LeanRewriteInputs<'_>) -> bool {
    let observed = prompt_capability(model_id, "temporal_hare", input.environment, input.served);
    if observed == Some(false) {
        return false;
    }
    if input.compiled_only {
        return observed == Some(true) && input.lean_prompt;
    }
    if observed != Some(true) && input.client_data != Some(true) {
        return input.cohort && input.lean_prompt && input.feature_enabled;
    }
    input.lean_prompt
        && prompt_feature_enabled(
            model_id,
            "temporal_hare",
            input.environment,
            input.served,
            input.client_data,
        )
}

/// Rewritten text is independently gated from the lean prompt itself.
#[must_use]
pub fn lean_rewrite_for(model_id: &str) -> bool {
    let environment = prompt_capabilities_environment();
    lean_rewrite_allowed(
        model_id,
        LeanRewriteInputs {
            environment: environment.as_deref(),
            lean_prompt: claude_lean_prompt_for(model_id),
            ..Default::default()
        },
    )
}

/// Native `kPe` applies the prompt exclusions after the actual route decision.
#[must_use]
pub fn prompt_system_turns_for_route(model_id: &str, route_supports_mid_system: bool) -> bool {
    let canonical = normalize_model_id(model_id);
    route_supports_mid_system
        && !matches!(canonical.as_str(), "claude-sonnet-5" | "claude-opus-4-8")
}

/// Native `kPe`/`w$`: the chronological model threshold precedes capability fallback.
#[must_use]
pub fn prompt_system_turns_allowed(
    model_id: &str,
    environment: Option<&str>,
    served: Option<bool>,
    force: bool,
    configured: Option<bool>,
    hipaa: bool,
) -> bool {
    let canonical = normalize_model_id(model_id);
    if hipaa {
        return false;
    }
    let before_opus48 = canonical.contains("claude-3-")
        || [
            "claude-opus-4-0",
            "claude-sonnet-4-0",
            "claude-opus-4-1",
            "claude-sonnet-4-5",
            "claude-haiku-4-5",
            "claude-opus-4-5",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-opus-4-7",
        ]
        .contains(&canonical.as_str());
    let supported = force
        || configured.unwrap_or_else(|| {
            !before_opus48
                && prompt_capability(model_id, "mid_conv_system", environment, served)
                    .unwrap_or(true)
        });
    prompt_system_turns_for_route(model_id, supported)
}

#[must_use]
pub fn prompt_system_turns_for(model_id: &str) -> bool {
    let environment = prompt_capabilities_environment();
    let force = std::env::var("CLAUDE_CODE_FORCE_MID_CONVERSATION_SYSTEM").ok();
    prompt_system_turns_allowed(
        model_id,
        environment.as_deref(),
        None,
        super::env::is_env_truthy(force.as_deref()),
        None,
        false,
    )
}

/// Native AM/ny built-in wakeup mitigation with an explicit capability override.
/// The caller supplies the selected canonical identity. Served catalog observations
/// stay separate; an explicit false must override both catalog and Mythos fallback.
#[must_use]
pub fn wakeup_ends_turn(model: &str, environment: Option<&str>, served: bool) -> bool {
    let identity = model.replace("[1m]", "").replace("[1M]", "");
    capability_override(&identity, "fable_5_mitigations", environment).unwrap_or_else(|| {
        served
            || model == "claude-mythos-5"
            || capabilities_for(&identity).contains(&"fable_5_mitigations")
    })
}

/// Native N8r: later matching capability entries replace earlier observations.
/// The model identity is supplied by the caller; selectors remain case-sensitive.
#[must_use]
pub fn capability_override(
    model: &str,
    capability: &str,
    environment: Option<&str>,
) -> Option<bool> {
    let identity = model.replace("[1m]", "").replace("[1M]", "");
    let mut observed = None;
    for entry in environment.into_iter().flat_map(|value| value.split(';')) {
        let values = if let Some((selector, values)) = entry.split_once('=') {
            let selector = super::effort::trim_js_whitespace(selector);
            if selector.is_empty()
                || !selector.strip_suffix('*').map_or_else(
                    || identity == selector,
                    |prefix| identity.starts_with(prefix),
                )
            {
                continue;
            }
            values
        } else {
            entry
        };
        for value in values.split(',') {
            let value = super::effort::trim_js_whitespace(value);
            let enabled = !value.starts_with('-');
            if value.strip_prefix('-').unwrap_or(value) == capability {
                observed = Some(enabled);
            }
        }
    }
    observed
}

/// Native Ry/ny with host-supplied identity, global admission and served observation.
/// Baked absence falls through to the native raw-name check; explicit denial does not.
#[must_use]
pub fn fast_model_allowed(
    model: &str,
    canonical: &str,
    environment: Option<&str>,
    global: bool,
    served: bool,
) -> bool {
    if !global {
        return false;
    }
    if let Some(value) = capability_override(canonical, "fast_mode", environment) {
        return value;
    }
    if served || capabilities_for(canonical).contains(&"fast_mode") {
        return true;
    }
    let lower = model.to_lowercase();
    lower.contains("opus-4-8") || lower.contains("opus-5")
}

/// Model-selected system-prompt family.
///
/// Claude transports share Claude's model-specific prompt behavior. Every
/// non-Claude provider/model stays on LingXi's complete harness; a future
/// third-party model can never inherit a short Claude prompt merely because it
/// is absent from the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptProfile {
    /// Claude's capability-gated short prompt.
    ClaudeLean,
    /// Claude's standard prompt.
    ClaudeStandard,
    /// LingXi's complete multi-provider prompt.
    FullHarness,
}

/// Resolve the prompt family for a model id, including provider/cloud wrappers.
#[must_use]
pub fn prompt_profile_for(model_id: &str) -> PromptProfile {
    let canonical = normalize_model_id(model_id);
    if !canonical.starts_with("claude-") {
        return PromptProfile::FullHarness;
    }
    if claude_lean_prompt_for(&canonical) {
        PromptProfile::ClaudeLean
    } else {
        PromptProfile::ClaudeStandard
    }
}

/// Initialize-response capability projection derived from the same registry
/// used by request and prompt gates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelInitializationCapabilities {
    /// Whether the model accepts an explicit effort level.
    pub supports_effort: bool,
    /// Ordered effort values advertised to SDK clients.
    pub supported_effort_levels: &'static [&'static str],
    /// Whether the model supports adaptive thinking.
    pub supports_adaptive_thinking: bool,
    /// Whether the model supports the first-party fast tier.
    pub supports_fast_mode: bool,
    /// Whether the model supports Claude Code's automatic mode.
    pub supports_auto_mode: bool,
}

/// Build the public initialize-model capability row from the canonical table.
#[must_use]
pub fn initialization_capabilities_for(model_id: &str) -> ModelInitializationCapabilities {
    const NONE: &[&str] = &[];
    const STANDARD: &[&str] = &["low", "medium", "high"];
    const WITH_MAX: &[&str] = &["low", "medium", "high", "max"];
    const WITH_XHIGH_AND_MAX: &[&str] = &["low", "medium", "high", "xhigh", "max"];

    let canonical = normalize_model_id(model_id);
    let supports_effort = has_capability(&canonical, ModelCapability::Effort);
    let supported_effort_levels = if !supports_effort {
        NONE
    } else if has_capability(&canonical, ModelCapability::XHighEffort) {
        WITH_XHIGH_AND_MAX
    } else if has_capability(&canonical, ModelCapability::MaxEffort) {
        WITH_MAX
    } else {
        STANDARD
    };
    let supports_auto_mode = matches!(
        canonical.as_str(),
        "claude-sonnet-4-6"
            | "claude-sonnet-5"
            | "claude-opus-4-6"
            | "claude-opus-4-7"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-fable-5-1"
            | "claude-mythos-5-1"
    );
    ModelInitializationCapabilities {
        supports_effort,
        supported_effort_levels,
        supports_adaptive_thinking: has_capability(&canonical, ModelCapability::AdaptiveThinking),
        supports_fast_mode: has_capability(&canonical, ModelCapability::FastMode),
        supports_auto_mode,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt_oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/model_capabilities_2_1_319.json"
        ))
        .unwrap()
    }

    #[test]
    fn prompt_capability_observations_match_native_319() {
        for row in prompt_oracle()["observations"].as_array().unwrap() {
            let environment = match row["support"].as_bool() {
                Some(true) => Some("lean_prompt"),
                Some(false) => Some("-lean_prompt"),
                None => None,
            };
            assert_eq!(
                prompt_capability(
                    row["model"].as_str().unwrap(),
                    "lean_prompt",
                    environment,
                    row["served"].as_bool(),
                ),
                row["observed"].as_bool(),
                "{row}"
            );
        }
    }

    #[test]
    fn claude_lean_prompt_matches_native_319() {
        for row in prompt_oracle()["leanCases"].as_array().unwrap() {
            let environment = match row["support"].as_bool() {
                Some(true) => Some("lean_prompt"),
                Some(false) => Some("-lean_prompt"),
                None => None,
            };
            assert_eq!(
                claude_lean_prompt_allowed(
                    row["model"].as_str().unwrap(),
                    environment,
                    row["served"].as_bool(),
                    row["simple"].as_str(),
                    row["configured"].as_bool().unwrap(),
                ),
                row["lean"].as_bool().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn lean_rewrite_matches_native_319_observations() {
        for row in prompt_oracle()["rewriteCases"].as_array().unwrap() {
            let environment = match row["support"].as_bool() {
                Some(true) => Some("temporal_hare"),
                Some(false) => Some("-temporal_hare"),
                None => None,
            };
            assert_eq!(
                lean_rewrite_allowed(
                    "claude-opus-5",
                    LeanRewriteInputs {
                        environment,
                        served: row["served"].as_bool(),
                        client_data: row["client"].as_bool(),
                        compiled_only: row["compiled"].as_bool().unwrap(),
                        lean_prompt: row["lean"].as_bool().unwrap(),
                        cohort: row["cohort"].as_bool().unwrap(),
                        feature_enabled: row["flag"].as_bool().unwrap(),
                    },
                ),
                row["rewrite"].as_bool().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn prompt_feature_defaults_match_native_319() {
        for row in prompt_oracle()["promptTraits"].as_array().unwrap() {
            let model = row["model"].as_str().unwrap();
            assert_eq!(
                claude_lean_prompt_allowed(model, None, None, None, false),
                row["lean"].as_bool().unwrap(),
                "{row}"
            );
            for (key, capability) in [
                ("turnUpdates", "turn_updates"),
                ("corrections", "larch_cistern"),
                ("silentTurn", "silent_turn_reminder"),
                ("originalGames", "original_games_guidance"),
                ("earlyStopping", "haiku_5_5_early_stopping_guidance"),
            ] {
                assert_eq!(
                    prompt_feature_enabled(model, capability, None, None, None),
                    row[key].as_bool().unwrap(),
                    "{model}: {key}"
                );
            }
            assert!(!row["rewrite"].as_bool().unwrap(), "{model}");
            assert_eq!(
                prompt_system_turns_allowed(model, None, None, false, None, false),
                row["systemTurns"].as_bool().unwrap(),
                "{model}: system turns"
            );
        }
    }

    #[test]
    fn prompt_feature_overrides_match_native_319() {
        for row in prompt_oracle()["featureCases"].as_array().unwrap() {
            let capability = row["capability"].as_str().unwrap();
            let environment = row["support"]
                .as_bool()
                .map(|enabled| format!("{}{capability}", if enabled { "" } else { "-" }));
            assert_eq!(
                prompt_feature_enabled(
                    row["model"].as_str().unwrap(),
                    capability,
                    environment.as_deref(),
                    row["served"].as_bool(),
                    row["client"].as_bool(),
                ),
                row["enabled"].as_bool().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn system_turn_threshold_and_route_inputs_match_native_319() {
        for row in prompt_oracle()["systemCases"].as_array().unwrap() {
            let environment = match row["support"].as_bool() {
                Some(true) => Some("mid_conv_system"),
                Some(false) => Some("-mid_conv_system"),
                None => None,
            };
            assert_eq!(
                prompt_system_turns_allowed(
                    row["model"].as_str().unwrap(),
                    environment,
                    row["served"].as_bool(),
                    row["force"].as_bool().unwrap(),
                    row["configured"].as_bool(),
                    row["hipaa"].as_bool().unwrap(),
                ),
                row["explainsSystemTurns"].as_bool().unwrap(),
                "{row}"
            );
        }
        assert!(!prompt_system_turns_for_route("claude-opus-5", false));
        assert!(prompt_system_turns_for_route("claude-sonnet-4-6", true));
        assert!(!prompt_system_turns_for_route("claude-sonnet-5", true));
        assert!(!prompt_system_turns_for_route("claude-opus-4-8", true));
    }

    #[test]
    fn provider_ids_normalize_to_the_native_319_catalog() {
        for row in prompt_oracle()["models"].as_array().unwrap() {
            let model = row["id"].as_str().unwrap();
            for id in row["provider_ids"].as_object().unwrap().values() {
                if let Some(id) = id.as_str() {
                    assert_eq!(normalize_model_id(id), model, "{id}");
                    assert_eq!(
                        normalize_model_id(&format!("openrouter/anthropic/{id}[1m]")),
                        model,
                        "{id}"
                    );
                }
            }
        }
    }

    #[test]
    fn native_fast_gate_matches_current_source_cases() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/model_capabilities_2_1_287.json"
        ))
        .unwrap();
        for row in fixture["fastCases"].as_array().unwrap() {
            let model = row["model"].as_str().unwrap();
            let environment = match row["support"].as_bool() {
                Some(true) => Some("fast_mode"),
                Some(false) => Some("-fast_mode"),
                None => None,
            };
            assert_eq!(
                fast_model_allowed(
                    model,
                    model,
                    environment,
                    row["global"].as_bool().unwrap(),
                    false
                ),
                row["expected"].as_bool().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn current_native_capability_table_is_exact() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/model_capabilities_2_1_319.json"
        ))
        .unwrap();
        for row in fixture["models"].as_array().unwrap() {
            let id = row["id"].as_str().unwrap();
            let expected: Vec<_> = row["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            assert_eq!(capabilities_for(id), expected, "{id}");
            assert_eq!(
                initialization_capabilities_for(id).supports_fast_mode,
                expected.contains(&"fast_mode"),
                "{id}"
            );
        }
    }

    #[test]
    fn current_native_wakeup_model_oracle() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/loop_model_2_1_287.json"))
                .unwrap();
        for row in fixture["cases"].as_array().unwrap() {
            assert_eq!(
                wakeup_ends_turn(
                    row["model"].as_str().unwrap(),
                    row["environment"].as_str(),
                    row["served"].as_bool().unwrap()
                ),
                row["expected"].as_bool().unwrap(),
                "{row}"
            );
        }
    }

    #[test]
    fn current_lean_prompt_models_match_the_capability_table() {
        for id in [
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ] {
            assert!(
                has_capability(id, ModelCapability::LeanPrompt),
                "{id} must have lean_prompt"
            );
        }
        for id in ["claude-opus-4-5", "claude-opus-4-6", "claude-opus-4-7"] {
            assert!(
                !has_capability(id, ModelCapability::LeanPrompt),
                "{id} must NOT have lean_prompt"
            );
        }
    }

    #[test]
    fn opus_5_carries_its_bundle_and_refusal_fallback() {
        assert!(has_capability(
            "claude-opus-5",
            ModelCapability::Opus5PromptBundle
        ));
        assert!(has_capability(
            "claude-opus-5",
            ModelCapability::RefusalFallback
        ));
        // ...and 4-8 does not, despite also having lean_prompt.
        assert!(!has_capability(
            "claude-opus-4-8",
            ModelCapability::Opus5PromptBundle
        ));
        assert!(!has_capability(
            "claude-opus-4-8",
            ModelCapability::RefusalFallback
        ));
    }

    #[test]
    fn mid_conversation_tool_change_matches_haiku_55_catalog_entry() {
        assert!(has_capability(
            "claude-haiku-5-5",
            ModelCapability::MidConvToolChange
        ));
        assert!(!has_capability(
            "claude-haiku-4-5",
            ModelCapability::MidConvToolChange
        ));
    }

    #[test]
    fn fable_5_carries_its_mitigations() {
        assert!(has_capability(
            "claude-fable-5-1",
            ModelCapability::Fable5Mitigations
        ));
        assert!(!has_capability(
            "claude-opus-5",
            ModelCapability::Fable5Mitigations
        ));
    }

    #[test]
    fn an_unknown_model_has_no_capabilities() {
        // The conservative direction: "no" rather than a wrong "yes" inferred
        // from a substring match.
        for id in ["gpt-5", "deepseek/deepseek-chat", "claude-opus-9", ""] {
            assert!(capabilities_for_loose(id).is_empty(), "{id}");
            assert!(!has_capability(id, ModelCapability::LeanPrompt), "{id}");
            assert!(!is_known_model_id(id), "{id}");
        }
        assert!(is_known_model_id("claude-opus-4-8"));
        assert!(is_known_model_id("openrouter/anthropic/claude-opus-5"));
        assert!(is_known_model_id("claude-haiku-5-5"));
        assert!(!is_known_model_id("claude-3-opus"));
    }

    #[test]
    fn decorated_ids_normalize_before_lookup() {
        for id in [
            "claude-opus-5",
            "CLAUDE-OPUS-5",
            "claude-opus-5[1m]",
            "claude-opus-5-eap",
            "openrouter/anthropic/claude-opus-5",
            "  claude-opus-5  ",
            "claude-opus-5@20260728",
            "us.anthropic.claude-opus-5-20260728-v1:0",
        ] {
            assert!(
                has_capability(id, ModelCapability::LeanPrompt),
                "{id} must resolve to claude-opus-5"
            );
        }
    }

    #[test]
    fn unrelated_claude_substrings_stay_on_the_full_harness() {
        for id in [
            "vendor-compat-claude-opus-5",
            "not-anthropic.claude-opus-5",
            "openrouter/vendor/vendor-compat-claude-opus-5",
        ] {
            assert_eq!(
                normalize_model_id(id),
                id.rsplit('/').next().unwrap(),
                "{id}"
            );
            assert!(capabilities_for_loose(id).is_empty(), "{id}");
            assert_eq!(prompt_profile_for(id), PromptProfile::FullHarness, "{id}");
        }
    }

    #[test]
    fn fast_mode_matches_the_table() {
        // Current baked catalog and native Ry no longer admit Opus 4.7.
        assert!(!has_capability(
            "claude-opus-4-7",
            ModelCapability::FastMode
        ));
        assert!(has_capability("claude-opus-5-5", ModelCapability::FastMode));
        assert!(has_capability("claude-opus-4-8", ModelCapability::FastMode));
        assert!(has_capability("claude-opus-5", ModelCapability::FastMode));
        // fable-5's list deliberately omits fast_mode.
        assert!(!has_capability(
            "claude-fable-5-1",
            ModelCapability::FastMode
        ));
    }

    #[test]
    fn prompt_profile_keeps_non_claude_models_on_the_full_harness() {
        for id in [
            "gpt-5.5",
            "deepseek-flash",
            "gemini-3.5-flash",
            "glm-5.1",
            "openrouter/qwen/qwen3-coder",
        ] {
            assert_eq!(prompt_profile_for(id), PromptProfile::FullHarness, "{id}");
        }
    }

    #[test]
    fn prompt_profile_distinguishes_claude_standard_and_lean_models() {
        assert_eq!(
            prompt_profile_for("claude-opus-4-7"),
            PromptProfile::ClaudeStandard
        );
        assert_eq!(
            prompt_profile_for("claude-opus-5"),
            PromptProfile::ClaudeLean
        );
        assert_eq!(
            prompt_profile_for("us.anthropic.claude-opus-5-v1:0"),
            PromptProfile::ClaudeLean
        );
    }

    #[test]
    fn initialize_projection_and_request_gate_share_fast_capability() {
        for id in [
            "claude-opus-5-5",
            "claude-opus-4-8",
            "claude-opus-5",
            "us.anthropic.claude-opus-5-v1:0",
        ] {
            assert!(
                initialization_capabilities_for(id).supports_fast_mode,
                "{id}"
            );
            assert!(has_capability(id, ModelCapability::FastMode), "{id}");
        }
        for id in [
            "claude-opus-4-7",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "gpt-5.5",
        ] {
            assert!(
                !initialization_capabilities_for(id).supports_fast_mode,
                "{id}"
            );
            assert!(!has_capability(id, ModelCapability::FastMode), "{id}");
        }
    }
}
