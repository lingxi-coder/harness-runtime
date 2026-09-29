//! Per-model capability registry — claude-code's `capabilities: [...]` array.
//!
//! The oracle's model table carries an explicit capability list per model, and
//! consumers ask `LN(model, "<capability>")` rather than pattern-matching the
//! model NAME. 2.1.220 @225786114-225787817:
//!
//! ```text
//! claude-opus-4-8 => [effort, max_effort, xhigh_effort, adaptive_thinking,
//!                     mid_conv_system, context_management, fast_mode, lean_prompt]
//! claude-opus-5   => [... , lean_prompt, refusal_fallback, opus_5_prompt_bundle]
//! claude-fable-5-1 => [... , lean_prompt, fable_5_mitigations, refusal_fallback]
//! ```
//!
//! WHY THIS EXISTS. The port decided "does this model take the lean system
//! prompt" by hardcoding a model-name list. It produced the right answer for
//! the models that were enumerated, and the right answer for `claude-opus-5`
//! only BY ACCIDENT — opus-5 was in neither branch and reached a fallthrough
//! that happens to return the correct result. A name list is also wrong in the
//! dangerous direction for anything added later: an unknown model silently
//! inherits whatever the fallthrough does, and nothing fails.
//!
//! This registry is deliberately NOT a guess-from-the-name function. A model
//! that is not listed returns `false` for every capability, which is the
//! conservative direction: a caller asking "does this support X" gets "no"
//! rather than a wrong "yes" derived from a substring match.

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
            Self::RefusalFallback => "refusal_fallback",
            Self::Opus5PromptBundle => "opus_5_prompt_bundle",
            Self::Fable5Mitigations => "fable_5_mitigations",
            Self::FastMode => "fast_mode",
        }
    }
}

/// The capability list for a first-party model id, verbatim from the 2.1.220
/// table. Unknown ids (including every third-party / non-Anthropic model)
/// return an empty slice.
///
/// Matching is on the BARE model id. A caller holding a provider-qualified id
/// (`openrouter/anthropic/claude-opus-5`) or a `-eap` / `[1m]` suffixed id must
/// normalize before asking — see [`capabilities_for_loose`].
#[must_use]
pub fn capabilities_for(model_id: &str) -> &'static [&'static str] {
    match model_id {
        "claude-sonnet-4-6" => &[
            "effort",
            "max_effort",
            "adaptive_thinking",
            "context_management",
        ],
        "claude-sonnet-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_system",
            "context_management",
        ],
        "claude-opus-4-5" => &["context_management"],
        "claude-opus-4-6" => &[
            "effort",
            "max_effort",
            "adaptive_thinking",
            "context_management",
        ],
        // Verified against the 2.1.220 catalog blob @225785080, which reads
        // `capabilities:["effort","max_effort","xhigh_effort",
        // "adaptive_thinking","context_management","fast_mode"]`. `fast_mode`
        // was missing here and the omission was pinned by two tests below; the
        // env-block prompt ("available on Opus 5/4.8/4.7.", oracle @113736244)
        // had been telling users the opposite all along.
        "claude-opus-4-7" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "context_management",
            "fast_mode",
        ],
        "claude-opus-4-8" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_system",
            "context_management",
            "fast_mode",
            "lean_prompt",
        ],
        "claude-opus-5" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "mid_conv_system",
            "context_management",
            "fast_mode",
            "lean_prompt",
            "refusal_fallback",
            "opus_5_prompt_bundle",
        ],
        "claude-fable-5-1" | "claude-mythos-5-1" => &[
            "effort",
            "max_effort",
            "xhigh_effort",
            "adaptive_thinking",
            "rejects_disabled_thinking",
            "mid_conv_system",
            "context_management",
            "lean_prompt",
            "fable_5_mitigations",
            // 2.1.263 baked-in catalog: enables the default silent-turn nudge.
            "fable_5_1_prompt_bundle",
            "refusal_fallback",
        ],
        _ => &[],
    }
}

const KNOWN_MODEL_IDS: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-sonnet-5",
    "claude-opus-4-5",
    "claude-opus-4-6",
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-fable-5-1",
    "claude-mythos-5-1",
];

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

/// Does `model_id` carry `capability`? Oracle `LN(model, capability)`.
///
/// An unknown model is `false` for everything — the conservative answer.
#[must_use]
pub fn has_capability(model_id: &str, capability: ModelCapability) -> bool {
    capabilities_for_loose(model_id).contains(&capability.as_wire())
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
    if has_capability(&canonical, ModelCapability::LeanPrompt) {
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
        }
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
        // 2.1.220 catalog @225785080 lists `fast_mode` for opus-4-7.
        assert!(has_capability("claude-opus-4-7", ModelCapability::FastMode));
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
            "claude-opus-4-7",
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
        for id in ["claude-sonnet-5", "claude-fable-5-1", "gpt-5.5"] {
            assert!(
                !initialization_capabilities_for(id).supports_fast_mode,
                "{id}"
            );
            assert!(!has_capability(id, ModelCapability::FastMode), "{id}");
        }
    }
}
