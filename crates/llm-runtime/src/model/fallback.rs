//! Non-custom Opus model identification for the consecutive-529 fallback gate.
//!
//! Ports claude-code `model.ts:40-48` (`isNonCustomOpusModel`). The TS
//! implementation compares the request model against the *provider-resolved*
//! Opus model strings returned by `getModelStrings()`
//! (`{ opus40, opus41, opus45, opus46 }`), which dispatch on the active
//! provider (firstParty / bedrock / vertex / foundry — see `configs.ts:51-77`).
//!
//! ## Documented divergence (1:1 fidelity note)
//!
//! Rust hard-codes only the **firstParty** literal IDs because firstParty is
//! the only provider wired today (`betas::Provider`/`anthropic.rs`). When
//! Bedrock / Vertex / Foundry land, their Opus IDs MUST be added here to stay
//! faithful — e.g. `us.anthropic.claude-opus-4-5-20251101-v1:0` (bedrock),
//! `claude-opus-4-5@20251101` (vertex). Until then a Bedrock/Vertex Opus model
//! would not trip the `!is_subscriber && is_non_custom_opus` fallback gate; the
//! `FALLBACK_FOR_ALL_PRIMARY_MODELS` env override (which bypasses this check
//! entirely, claude-code `withRetry.ts:331`) is the escape hatch in that case.
//!
//! The four firstParty literals below are copied byte-for-byte from
//! claude-code `configs.ts`:
//! * `CLAUDE_OPUS_4_CONFIG.firstParty`   = `claude-opus-4-20250514`
//! * `CLAUDE_OPUS_4_1_CONFIG.firstParty` = `claude-opus-4-1-20250805`
//! * `CLAUDE_OPUS_4_5_CONFIG.firstParty` = `claude-opus-4-5-20251101`
//! * `CLAUDE_OPUS_4_6_CONFIG.firstParty` = `claude-opus-4-6`

#![forbid(unsafe_code)]

/// The four first-party (Anthropic API) non-custom Opus model IDs, byte-locked
/// to claude-code `configs.ts:51-77` (`firstParty` field of each Opus config).
///
/// Order matches the TS `isNonCustomOpusModel` disjunction
/// (`opus40 || opus41 || opus45 || opus46`).
pub const NON_CUSTOM_OPUS_MODELS: &[&str] = &[
    "claude-opus-4-20250514",   // CLAUDE_OPUS_4_CONFIG.firstParty
    "claude-opus-4-1-20250805", // CLAUDE_OPUS_4_1_CONFIG.firstParty
    "claude-opus-4-5-20251101", // CLAUDE_OPUS_4_5_CONFIG.firstParty
    "claude-opus-4-6",          // CLAUDE_OPUS_4_6_CONFIG.firstParty
];

/// Whether `model` is a non-custom (first-party) Opus model.
///
/// 1:1 with claude-code `isNonCustomOpusModel` (`model.ts:40-48`): an exact
/// match against one of the four first-party Opus IDs. The TS comparison is a
/// strict `===` against the provider-resolved strings, so this is an exact
/// (not substring) match — a custom alias or a Bedrock/Vertex-shaped ID does
/// NOT match (see module-level divergence note).
#[must_use]
pub fn is_non_custom_opus(model: &str) -> bool {
    NON_CUSTOM_OPUS_MODELS.contains(&model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_first_party_opus_ids_match() {
        // Byte-locked table from claude-code configs.ts:51-77.
        assert!(is_non_custom_opus("claude-opus-4-20250514"));
        assert!(is_non_custom_opus("claude-opus-4-1-20250805"));
        assert!(is_non_custom_opus("claude-opus-4-5-20251101"));
        assert!(is_non_custom_opus("claude-opus-4-6"));
    }

    #[test]
    fn non_opus_models_do_not_match() {
        assert!(!is_non_custom_opus("claude-sonnet-4-5-20250929"));
        assert!(!is_non_custom_opus("claude-haiku-4-5"));
        assert!(!is_non_custom_opus("claude-sonnet-4-6"));
        assert!(!is_non_custom_opus("gpt-4o"));
        assert!(!is_non_custom_opus(""));
    }

    #[test]
    fn match_is_exact_not_substring() {
        // A custom alias or a provider-shaped (bedrock/vertex) Opus id is NOT a
        // first-party literal, so it must not match (documented divergence: the
        // TS getModelStrings() would resolve those per-provider; Rust hard-codes
        // firstParty only until those providers wire up).
        assert!(!is_non_custom_opus("claude-opus-4-5-20251101-v1:0"));
        assert!(!is_non_custom_opus(
            "us.anthropic.claude-opus-4-5-20251101-v1:0"
        ));
        assert!(!is_non_custom_opus("claude-opus-4-5@20251101"));
        assert!(!is_non_custom_opus("claude-opus-4-6-custom"));
        assert!(!is_non_custom_opus("my-claude-opus-4-6"));
    }

    #[test]
    fn const_table_is_byte_locked() {
        assert_eq!(
            NON_CUSTOM_OPUS_MODELS,
            &[
                "claude-opus-4-20250514",
                "claude-opus-4-1-20250805",
                "claude-opus-4-5-20251101",
                "claude-opus-4-6",
            ]
        );
    }
}
