//! Model context-window and max-output-token tables.
//!
//! Ports the model → window / output lookups from the claude-code reference:
//! `src/utils/context.ts` (`getContextWindowForModel`, `getModelMaxOutputTokens`)
//! and `src/services/api/claude.ts` (`getMaxOutputTokensForModel`).
//!
//! ## Divergences from TS
//!
//! These functions take a `model: &str` (and `betas: &[String]`) and reproduce
//! the byte-faithful canonical-name table and env-override handling. A few TS
//! branches depend on subsystems not reachable from the `compaction` crate
//! without new dependencies; they are intentionally omitted and noted here:
//!
//! - `getModelCapability(model)` (litellm-style capability table) — the TS
//!   `max_input_tokens` / `max_tokens` override paths are skipped. No Rust
//!   capability registry is reachable from `compaction/`.
//! - `resolveAntModel` / `USER_TYPE === 'ant'` ant-model context-window and
//!   max-token overrides — the ant-model registry lives elsewhere. The
//!   `LINGXI_MAX_CONTEXT_TOKENS` override (which TS *also* gates on
//!   `USER_TYPE === 'ant'`) IS honored here because it is a pure env read.
//! - `getSonnet1mExpTreatmentEnabled` (`GrowthBook` `coral_reef_sonnet`
//!   client-data-cache flag) — `GrowthBook` config has no Rust equivalent here;
//!   omitted.
//! - `isMaxTokensCapEnabled` (`GrowthBook` `tengu_otk_slot_v1` slot-reservation
//!   cap → drop default to [`CAPPED_DEFAULT_MAX_TOKENS`]) — `GrowthBook` flag is
//!   unwired in Rust, so the cap is NOT applied; [`max_output_tokens_for_model`]
//!   returns the model's native default. The `LINGXI_MAX_OUTPUT_TOKENS`
//!   env override (a pure env read) IS honored, clamped to the upper limit.

use platform_api::env::is_env_truthy;

/// Default model context window (200k tokens for all models right now).
/// Mirrors `MODEL_CONTEXT_WINDOW_DEFAULT` in `utils/context.ts`.
pub const MODEL_CONTEXT_WINDOW_DEFAULT: u64 = 200_000;

/// Default max output tokens. Mirrors `MAX_OUTPUT_TOKENS_DEFAULT`.
const MAX_OUTPUT_TOKENS_DEFAULT: u64 = 32_000;
/// Upper limit for max output tokens (the unknown-model fallback). Mirrors the
/// binary `YCe` else-branch `NWu` = 128_000 (v2.1.183).
const MAX_OUTPUT_TOKENS_UPPER_LIMIT: u64 = 128_000;

/// 1M-context beta header. Mirrors `CONTEXT_1M_BETA_HEADER` in `constants/betas.ts`.
pub const CONTEXT_1M_BETA_HEADER: &str = "context-1m-2025-08-07";

/// `true` if 1M context is disabled via `CLAUDE_CODE_DISABLE_1M_CONTEXT`.
/// Mirrors `is1mContextDisabled`.
fn is_1m_context_disabled() -> bool {
    is_env_truthy(
        std::env::var("CLAUDE_CODE_DISABLE_1M_CONTEXT")
            .ok()
            .as_deref(),
    )
}

/// `true` if `model` carries an explicit `[1m]` suffix (case-insensitive),
/// unless 1M context is disabled. Mirrors `has1mContext`.
pub fn has_1m_context(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    // TS uses /\[1m\]/i — a case-insensitive substring of the literal "[1m]".
    model.to_lowercase().contains("[1m]")
}

/// `true` if the canonical model family supports the 1M-context beta, unless
/// 1M context is disabled. Mirrors `modelSupports1M` (2.1.198 binary `hG`
/// @208698905: excludes claude-3-*/opus-4-0/4-1/4-5/haiku-4-5 via `QRn`, then
/// `KL(canonical)?.context?.supports_1m_beta`). Older Sonnet 4 and Opus 4.6+
/// families retain their beta support; current 5-series models are native 1M.
fn model_supports_1m(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    let canonical = canonical_name(model);
    canonical.contains("claude-sonnet-4")
        || matches!(
            canonical.as_str(),
            "claude-sonnet-5"
                | "claude-opus-4-6"
                | "claude-opus-4-7"
                | "claude-opus-4-8"
                | "claude-opus-5"
                | "claude-fable-5-1"
                | "claude-mythos-5-1"
                | "claude-mythos-preview"
        )
}

/// `true` if the model's registry entry marks it natively 1M (2.1.198 binary
/// `Hx` @208698511: `KL(canonical)?.context?.native_1m` on the first-party
/// path — no beta header and no `[1m]` suffix required). Fable is intentionally
/// recognized only as `claude-fable-5-1`; the removed 5.0 id is not an alias.
/// Honors the same `CLAUDE_CODE_DISABLE_1M_CONTEXT` kill switch (`Aye()` guard
/// inside `Hx`).
pub fn model_native_1m(model: &str) -> bool {
    if is_1m_context_disabled() {
        return false;
    }
    let canonical = canonical_name(model);
    matches!(
        canonical.as_str(),
        "claude-sonnet-5"
            | "claude-opus-4-7"
            | "claude-opus-4-8"
            | "claude-opus-5"
            | "claude-fable-5-1"
            | "claude-mythos-5-1"
            | "claude-mythos-preview"
    )
}

/// Resolve a full model id to a shorter canonical family name.
///
/// Ports `firstPartyNameToCanonical` (`utils/model/model.ts:217-270`). The
/// Bedrock-ARN `resolveOverriddenModel` indirection in `getCanonicalName` is a
/// no-op for the substring checks we perform, so it is folded in here.
fn canonical_name(model: &str) -> String {
    let name = model.to_lowercase();
    if name.contains("claude-fable-5-1") {
        return "claude-fable-5-1".to_string();
    }
    if name.contains("claude-mythos-5-1") {
        return "claude-mythos-5-1".to_string();
    }
    if name.contains("claude-opus-5") {
        return "claude-opus-5".to_string();
    }
    // Order matters: check more specific versions first (4-8/4-7/4-6 before 4-5 before 4).
    // opus-4-8/4-7 must precede the bare `claude-opus-4` catch so they resolve to
    // their own canonical (binary `getCanonicalName` preserves them) and get the
    // 128k max-output tier (binary `YCe`), not the bare-family 32k/32k.
    if name.contains("claude-opus-4-8") {
        return "claude-opus-4-8".to_string();
    }
    if name.contains("claude-opus-4-7") {
        return "claude-opus-4-7".to_string();
    }
    if name.contains("claude-opus-4-6") {
        return "claude-opus-4-6".to_string();
    }
    if name.contains("claude-opus-4-5") {
        return "claude-opus-4-5".to_string();
    }
    if name.contains("claude-opus-4-1") {
        return "claude-opus-4-1".to_string();
    }
    if name.contains("claude-opus-4") {
        return "claude-opus-4".to_string();
    }
    // sonnet-5 before the sonnet-4-x catches (2.1.198 registry adds
    // claude-sonnet-5; binary canonicalization checks `sonnet-5` first —
    // note "claude-sonnet-4-5" does NOT contain "sonnet-5", see tests).
    if name.contains("claude-sonnet-5") {
        return "claude-sonnet-5".to_string();
    }
    if name.contains("claude-sonnet-4-6") {
        return "claude-sonnet-4-6".to_string();
    }
    if name.contains("claude-sonnet-4-5") {
        return "claude-sonnet-4-5".to_string();
    }
    if name.contains("claude-sonnet-4") {
        return "claude-sonnet-4".to_string();
    }
    if name.contains("claude-haiku-4-5") {
        return "claude-haiku-4-5".to_string();
    }
    if name.contains("claude-3-7-sonnet") {
        return "claude-3-7-sonnet".to_string();
    }
    if name.contains("claude-3-5-sonnet") {
        return "claude-3-5-sonnet".to_string();
    }
    if name.contains("claude-3-5-haiku") {
        return "claude-3-5-haiku".to_string();
    }
    if name.contains("claude-3-opus") {
        return "claude-3-opus".to_string();
    }
    if name.contains("claude-3-sonnet") {
        return "claude-3-sonnet".to_string();
    }
    if name.contains("claude-3-haiku") {
        return "claude-3-haiku".to_string();
    }
    // Fall back to the original (lowercased) name if no pattern matches. The TS
    // `/(claude-(\d+-\d+-)?\w+)/` regex only narrows the string for the unmatched
    // case; substring checks against the full lowercased name are equivalent for
    // our purposes, so we return it unmodified.
    name
}

/// `true` when `model` resolves to a first-party Claude family — including
/// Claude served via Bedrock/Vertex/OpenRouter, whose canonical name still
/// begins `claude-` (e.g. `anthropic/claude-3-5-sonnet`).
///
/// LingXi is multi-provider, but the window / max-output tables ported below are
/// byte-faithful to claude-code and only correct for Claude. This gate keeps
/// those tables authoritative for Claude while letting non-Claude models draw
/// their real limits from the catalog-fed [`model_limits`](super::model_limits)
/// registry. Anything whose canonical name is not a `claude-*` family (gpt-*,
/// gemini-*, deepseek-*, glm-*, …) is treated as non-Claude.
///
/// Also used by the request builder to gate Anthropic-shaped thinking/reasoning
/// (the `Adaptive` intent) off non-Claude routes.
#[must_use]
pub fn is_claude_family(model: &str) -> bool {
    canonical_name(model).starts_with("claude")
}

/// Returns the context window size for `model`, honoring `betas`.
///
/// Mirrors `getContextWindowForModel` (`utils/context.ts:51-98`), minus the
/// model-capability and ant-model registry branches (see module docs).
#[must_use]
pub fn context_window_for_model(model: &str, betas: &[String]) -> u64 {
    // LINGXI_MAX_CONTEXT_TOKENS override (TS gates on USER_TYPE === 'ant').
    // Takes precedence over all other resolution, including 1M detection.
    if std::env::var("USER_TYPE").ok().as_deref() == Some("ant") {
        if let Ok(raw) = std::env::var("LINGXI_MAX_CONTEXT_TOKENS") {
            // `Z.CLAUDE_CODE_MAX_CONTEXT_TOKENS` (coerced by the shared `hp`
            // helper) then `!== void 0 && > 0`; a `NaN`/non-positive value falls
            // through (unset/empty ⇒ `NaN` ⇒ skip).
            let n = platform_api::env::parse_int_env(&raw);
            if !n.is_nan() && n > 0.0 {
                return n as u64;
            }
        }
    }

    // [1m] suffix — explicit client-side opt-in, respected over all detection.
    if has_1m_context(model) {
        return 1_000_000;
    }

    // CONTEXT_1M_BETA_HEADER beta + 1M-capable model → 1M.
    if betas.iter().any(|b| b == CONTEXT_1M_BETA_HEADER) && model_supports_1m(model) {
        return 1_000_000;
    }

    // Native 1M (2.1.198 binary `XHi`: `if(Hx(e))return 1e6` — after the
    // suffix/beta checks, before the default). Current 1M Claude families are
    // native 1M (registry `native_1m:!0`), not beta-gated.
    if model_native_1m(model) {
        return 1_000_000;
    }

    // Multi-provider fix: non-Claude models use their real catalog window
    // (models.dev `Limit.context`) instead of the Claude 200k default. Claude
    // ids bypass this so their byte-faithful behavior above is untouched.
    if !is_claude_family(model) {
        if let Some(limits) = crate::model::model_limits::lookup(model) {
            return limits.context_window;
        }
    }

    MODEL_CONTEXT_WINDOW_DEFAULT
}

/// Returns the model's default and upper limit for max output tokens.
///
/// Ports `getModelMaxOutputTokens` (`utils/context.ts:149-210`), minus the
/// ant-model and model-capability branches (see module docs).
fn model_max_output_tokens(model: &str) -> (u64, u64) {
    // Multi-provider fix: non-Claude models draw their real max-output limit
    // (models.dev `Limit.output`) from the catalog registry rather than the
    // Claude 32k/128k fallback. Both the default and the upper limit collapse to
    // the model's true output cap. Claude ids fall through to the table below.
    if !is_claude_family(model) {
        if let Some(limits) = crate::model::model_limits::lookup(model) {
            return (limits.max_output_tokens, limits.max_output_tokens);
        }
    }

    let m = canonical_name(model);
    // The current 5-series lineup advertises a 128k output limit. Older 4.x
    // models retain their existing defaults and upper limits.
    let (default_tokens, upper_limit) = if matches!(
        m.as_str(),
        "claude-opus-5" | "claude-sonnet-5" | "claude-fable-5-1" | "claude-mythos-5-1"
    ) {
        (128_000, 128_000)
    } else if m.contains("opus-4-8") || m.contains("opus-4-7") || m.contains("opus-4-6") {
        (64_000, 128_000)
    } else if m.contains("sonnet-4-6") {
        (32_000, 128_000)
    } else if m.contains("opus-4-5") || m.contains("sonnet-4") || m.contains("haiku-4") {
        (32_000, 64_000)
    } else if m.contains("opus-4-1") || m.contains("opus-4") {
        (32_000, 32_000)
    } else if m.contains("claude-3-opus") {
        (4_096, 4_096)
    } else if m.contains("claude-3-sonnet") {
        (8_192, 8_192)
    } else if m.contains("claude-3-haiku") {
        (4_096, 4_096)
    } else if m.contains("3-5-sonnet") || m.contains("3-5-haiku") {
        (8_192, 8_192)
    } else if m.contains("3-7-sonnet") {
        (32_000, 64_000)
    } else {
        (MAX_OUTPUT_TOKENS_DEFAULT, MAX_OUTPUT_TOKENS_UPPER_LIMIT)
    };
    (default_tokens, upper_limit)
}

/// Returns the effective hard output ceiling for `model`.
///
/// Unlike [`default_output_tokens_for_model`], this value is only a validation
/// ceiling. It should not be copied into every request as the implicit output
/// budget.
#[must_use]
pub fn output_token_limit_for_model(model: &str) -> u64 {
    model_max_output_tokens(model).1
}

/// Returns a provider-advertised hard output ceiling when model metadata is
/// known. Unknown custom models return `None` instead of inheriting a guessed
/// fallback ceiling.
#[must_use]
pub fn known_output_token_limit_for_model(model: &str) -> Option<u64> {
    if is_claude_family(model) {
        Some(output_token_limit_for_model(model))
    } else {
        super::model_limits::lookup(model).map(|limits| limits.max_output_tokens)
    }
}

/// Returns the ordinary per-request output budget for `model`.
///
/// Catalog `limit.output` values are hard provider ceilings, not sensible
/// defaults for every turn.  Most coding-agent turns need far less output, and
/// using a ceiling that nearly equals the context window leaves no reliable
/// room for provider-side prompt/tool formatting.  Keep Claude's model-specific
/// defaults, but cap the implicit default for catalog-backed non-Claude models
/// at the same 32k budget used by the generic fallback.  An explicit
/// `LINGXI_MAX_OUTPUT_TOKENS` value may still opt in up to the model ceiling.
#[must_use]
pub fn default_output_tokens_for_model(model: &str) -> u64 {
    let (model_default, upper_limit) = model_max_output_tokens(model);
    let default_tokens = if is_claude_family(model) {
        model_default
    } else {
        upper_limit.min(MAX_OUTPUT_TOKENS_DEFAULT)
    };
    configured_output_tokens(default_tokens, upper_limit)
}

fn configured_output_tokens(default_tokens: u64, upper_limit: u64) -> u64 {
    if let Ok(raw) = std::env::var("LINGXI_MAX_OUTPUT_TOKENS") {
        let value = platform_api::env::parse_int_env(&raw);
        if !value.is_nan() && value > 0.0 {
            return value.min(upper_limit as f64) as u64;
        }
    }

    default_tokens
}

/// Returns the effective max output tokens for `model`.
///
/// Mirrors `getMaxOutputTokensForModel` (`api/claude.ts:3399-3419`). The
/// GrowthBook-gated `isMaxTokensCapEnabled` slot cap is unwired in Rust (see
/// module docs), so the native default is used; the
/// `LINGXI_MAX_OUTPUT_TOKENS` env override is honored, clamped to the
/// model's upper limit.
#[must_use]
pub fn max_output_tokens_for_model(model: &str) -> u64 {
    let (default_tokens, upper_limit) = model_max_output_tokens(model);
    configured_output_tokens(default_tokens, upper_limit)
}

/// Returns the maximum thinking-budget tokens for `model`.
///
/// Mirrors claude-code's `getMaxThinkingTokensForModel`
/// (`getModelMaxOutputTokens(model).upperLimit - 1`): the fixed-budget thinking
/// ceiling is one below the model's max-output upper limit. Used by the
/// orchestrator when the model does not support adaptive thinking and a concrete
/// `budget_tokens` must be supplied.
#[must_use]
pub fn max_thinking_tokens_for_model(model: &str) -> u32 {
    let (_default_tokens, upper_limit) = model_max_output_tokens(model);
    u32::try_from(upper_limit.saturating_sub(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_window_for_unknown_and_known_models() {
        // No betas, no [1m] suffix → 200k default for non-native-1M models.
        for model in [
            "claude-sonnet-4-5-20250929",
            "claude-3-5-haiku-20241022",
            "some-unknown-model",
        ] {
            assert_eq!(context_window_for_model(model, &[]), 200_000, "{model}");
        }
    }

    #[test]
    fn one_m_suffix_unlocks_1m_window() {
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6[1m]", &[]),
            1_000_000
        );
        // Case-insensitive on the suffix.
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6[1M]", &[]),
            1_000_000
        );
    }

    #[test]
    fn beta_header_unlocks_1m_only_for_capable_models() {
        let betas = vec![CONTEXT_1M_BETA_HEADER.to_string()];
        // A non-capable model with the beta still gets the default.
        assert_eq!(
            context_window_for_model("claude-3-5-haiku-20241022", &betas),
            200_000
        );
        // Older capable families remain beta-gated.
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6-20251001", &[]),
            200_000
        );
        assert_eq!(
            context_window_for_model("claude-sonnet-4-6-20251001", &betas),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("claude-opus-4-6-20260101", &betas),
            1_000_000
        );
    }

    #[test]
    fn sonnet_5_is_natively_1m_and_128k_output() {
        // Current docs / registry: claude-sonnet-5 is native 1M WITHOUT any
        // beta or [1m] suffix (binary XHi → Hx).
        assert_eq!(context_window_for_model("claude-sonnet-5", &[]), 1_000_000);
        // Dated / provider-shaped ids canonicalize to claude-sonnet-5 too.
        assert_eq!(
            context_window_for_model("us.anthropic.claude-sonnet-5", &[]),
            1_000_000
        );
        // The explicit [1m] suffix still resolves.
        assert_eq!(
            context_window_for_model("claude-sonnet-5[1m]", &[]),
            1_000_000
        );
        // The 1M beta also keeps it at 1M.
        let betas = vec![CONTEXT_1M_BETA_HEADER.to_string()];
        assert_eq!(
            context_window_for_model("claude-sonnet-5", &betas),
            1_000_000
        );
        // Current docs: Sonnet 5 defaults to its 128k output limit.
        assert_eq!(max_output_tokens_for_model("claude-sonnet-5"), 128_000);
        assert_eq!(max_thinking_tokens_for_model("claude-sonnet-5"), 127_999);
    }

    #[test]
    fn current_1m_claude_models_are_native() {
        // Current docs: the current 1M Claude families are native 1M with NO
        // beta header and NO [1m] suffix, exactly like sonnet-5.
        for model in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-mythos-preview",
            // Dated / provider-shaped ids canonicalize to the same entries.
            "claude-opus-4-8-20260115",
            "us.anthropic.claude-opus-4-7",
            "us.anthropic.claude-opus-5",
            "us.anthropic.claude-fable-5-1",
        ] {
            assert_eq!(context_window_for_model(model, &[]), 1_000_000, "{model}");
        }
        // The 1M beta ALSO keeps them at 1M — same result.
        let betas = vec![CONTEXT_1M_BETA_HEADER.to_string()];
        assert_eq!(
            context_window_for_model("claude-opus-4-6", &betas),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("claude-opus-4-8", &betas),
            1_000_000
        );
        assert_eq!(
            context_window_for_model("claude-fable-5-1", &betas),
            1_000_000
        );
        // Fable 5.0 is deliberately not retained as a compatibility alias.
        assert_eq!(context_window_for_model("claude-fable-5", &[]), 200_000);
        assert_eq!(max_output_tokens_for_model("claude-fable-5"), 32_000);
        // pIe max-output is now 128k on the current 1M families; the thinking
        // ceiling rides 128k-1.
        assert_eq!(max_thinking_tokens_for_model("claude-opus-4-7"), 127_999);
        assert_eq!(max_thinking_tokens_for_model("claude-fable-5-1"), 127_999);
        // `Hx` special case: claude-mythos-preview is native-1M despite having
        // no registry entry (`t!=="claude-mythos-preview"` bail).
        assert_eq!(
            context_window_for_model("claude-mythos-preview", &[]),
            1_000_000
        );
    }

    #[test]
    fn sonnet_5_canonicalization_never_bleeds_into_neighbors() {
        // Contains-check hazard lock: neighbor ids must NOT hit the sonnet-5
        // arms ("claude-sonnet-4-5" does not contain "sonnet-5"), and
        // sonnet-5 must NOT hit the sonnet-4-x arms.
        assert_eq!(canonical_name("claude-sonnet-5"), "claude-sonnet-5");
        assert_eq!(canonical_name("claude-sonnet-4-5"), "claude-sonnet-4-5");
        assert_eq!(canonical_name("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(canonical_name("claude-3-5-sonnet"), "claude-3-5-sonnet");
        // Neighbors keep their own windows / outputs.
        assert_eq!(
            context_window_for_model("claude-sonnet-4-5-20250929", &[]),
            200_000
        );
        assert_eq!(
            max_output_tokens_for_model("claude-sonnet-4-5-20250929"),
            32_000
        );
        assert_eq!(context_window_for_model("claude-sonnet-4-6", &[]), 200_000);
        assert_eq!(max_output_tokens_for_model("claude-sonnet-4-6"), 32_000);
        assert_eq!(
            max_output_tokens_for_model("claude-3-5-sonnet-20241022"),
            8_192
        );
    }

    #[test]
    fn max_output_tokens_canonical_table() {
        assert_eq!(
            max_output_tokens_for_model("claude-opus-4-6-20260101"),
            64_000
        );
        assert_eq!(
            max_output_tokens_for_model("claude-sonnet-4-6-20251001"),
            32_000
        );
        assert_eq!(max_output_tokens_for_model("claude-opus-4-5-x"), 32_000);
        assert_eq!(
            max_output_tokens_for_model("claude-sonnet-4-20250101"),
            32_000
        );
        assert_eq!(max_output_tokens_for_model("claude-haiku-4-5-x"), 32_000);
        assert_eq!(max_output_tokens_for_model("claude-opus-4-1-x"), 32_000);
        // Current 1M Claude families → 128k (NOT the bare-opus-4 32k).
        assert_eq!(max_output_tokens_for_model("claude-opus-5"), 128_000);
        assert_eq!(
            max_output_tokens_for_model("claude-opus-4-8-20260115"),
            64_000
        );
        assert_eq!(max_output_tokens_for_model("claude-opus-4-7-x"), 64_000);
        assert_eq!(max_output_tokens_for_model("claude-opus-4-6-x"), 64_000);
        assert_eq!(max_output_tokens_for_model("claude-fable-5-1"), 128_000);
        assert_eq!(max_output_tokens_for_model("claude-mythos-5-1"), 128_000);
        assert_eq!(max_output_tokens_for_model("claude-mythos-preview"), 32_000);
        assert_eq!(
            max_output_tokens_for_model("claude-opus-4-20250514"),
            32_000
        );
        assert_eq!(max_output_tokens_for_model("claude-3-opus-20240229"), 4_096);
        assert_eq!(
            max_output_tokens_for_model("claude-3-sonnet-20240229"),
            8_192
        );
        assert_eq!(
            max_output_tokens_for_model("claude-3-haiku-20240307"),
            4_096
        );
        assert_eq!(
            max_output_tokens_for_model("claude-3-5-sonnet-20241022"),
            8_192
        );
        assert_eq!(
            max_output_tokens_for_model("claude-3-5-haiku-20241022"),
            8_192
        );
        assert_eq!(
            max_output_tokens_for_model("claude-3-7-sonnet-20250219"),
            32_000
        );
        // Unknown model → default.
        assert_eq!(max_output_tokens_for_model("mystery-model"), 32_000);
    }

    #[test]
    fn registry_drives_non_claude_window_and_output() {
        use crate::model::model_limits::{register, ModelLimits};
        // A non-Claude id with real (large) limits registered from the catalog.
        let id = "gpt-test-bigwindow-9";
        register(
            id,
            ModelLimits {
                context_window: 1_050_000,
                max_output_tokens: 128_000,
            },
        );
        assert_eq!(context_window_for_model(id, &[]), 1_050_000);
        assert_eq!(output_token_limit_for_model(id), 128_000);
        assert_eq!(max_output_tokens_for_model(id), 128_000);
        assert_eq!(default_output_tokens_for_model(id), 32_000);
        // Thinking ceiling follows the real output, not the Claude 128k-1.
        assert_eq!(max_thinking_tokens_for_model(id), 127_999);

        // A Claude id MUST ignore the registry and keep byte-faithful tables,
        // even if (defensively) something registered a bogus value for it.
        register(
            "claude-opus-4-8-20260115",
            ModelLimits {
                context_window: 999,
                max_output_tokens: 999,
            },
        );
        // (2.1.198: opus-4-8 is natively 1M — the point here is that the
        // bogus registered 999 is IGNORED, not the specific window.)
        assert_eq!(
            context_window_for_model("claude-opus-4-8-20260115", &[]),
            1_000_000
        );
        assert_eq!(
            max_output_tokens_for_model("claude-opus-4-8-20260115"),
            64_000
        );
    }

    #[test]
    fn openrouter_glm_free_does_not_use_hard_output_limit_as_default() {
        use crate::model::model_limits::{register, ModelLimits};

        let id = "z-ai/glm-5.2:free-test";
        register(
            id,
            ModelLimits {
                context_window: 256_000,
                max_output_tokens: 230_400,
            },
        );

        assert_eq!(context_window_for_model(id, &[]), 256_000);
        assert_eq!(output_token_limit_for_model(id), 230_400);
        assert_eq!(max_output_tokens_for_model(id), 230_400);
        assert_eq!(default_output_tokens_for_model(id), 32_000);
        assert_eq!(max_thinking_tokens_for_model(id), 230_399);
        assert_eq!(
            known_output_token_limit_for_model("unknown-custom-model"),
            None
        );
    }

    #[test]
    fn unregistered_non_claude_still_falls_back_to_claude_defaults() {
        // Parity: an unknown, unregistered non-Claude id keeps the 200k / 32k
        // defaults (the claude-code behavior) — the registry is additive only.
        assert_eq!(
            context_window_for_model("totally-unregistered-xyz", &[]),
            200_000
        );
        assert_eq!(
            max_output_tokens_for_model("totally-unregistered-xyz"),
            32_000
        );
    }

    #[test]
    fn env_truthy_matches_ts() {
        for v in ["1", "true", "TRUE", " yes ", "on", "On"] {
            assert!(is_env_truthy(Some(v)), "{v} should be truthy");
        }
        for v in ["0", "false", "no", "off", "", "anything"] {
            assert!(!is_env_truthy(Some(v)), "{v} should be falsy");
        }
        assert!(!is_env_truthy(None));
    }
}
