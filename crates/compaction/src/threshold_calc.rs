//! Pure threshold arithmetic for the orchestrator pass (Batch 3).
//!
//! These are the model-independent kernels behind the model-aware helpers in
//! [`crate::thresholds`]. The orchestrator works with already-resolved token
//! numbers (a window, a max-output budget, a precomputed threshold), so it
//! needs the raw math without re-resolving the model/env each call. Each
//! function mirrors the corresponding TS computation in
//! `src/services/compact/autoCompact.ts`:
//!
//! - [`effective_context_window`] ↔ `getEffectiveContextWindowSize`
//!   (`autoCompact.ts:33-49`), the `contextWindow − min(maxOutput, …)` step.
//! - [`auto_compact_threshold`] ↔ `getAutoCompactThreshold`
//!   (`autoCompact.ts:72-91`), the `effective − AUTOCOMPACT_BUFFER_TOKENS` step.
//! - [`should_auto_compact`] ↔ the threshold test inside `shouldAutoCompact`
//!   (`autoCompact.ts:225-238`), including the `snipTokensFreed` subtraction.
//!
//! The env-override branches (`LINGXI_AUTO_COMPACT_WINDOW`,
//! `LINGXI_AUTOCOMPACT_PCT_OVERRIDE`) live in the model-aware
//! [`crate::thresholds`] functions; these kernels take post-resolution numbers.

use crate::thresholds::{AUTOCOMPACT_BUFFER_TOKENS, MAX_OUTPUT_TOKENS_FOR_SUMMARY};

/// `context_window − min(max_output_tokens, MAX_OUTPUT_TOKENS_FOR_SUMMARY)`.
///
/// Mirrors `getEffectiveContextWindowSize` (`autoCompact.ts:33-49`): the model's
/// context window minus the output budget reserved for the summary call, capped
/// at [`MAX_OUTPUT_TOKENS_FOR_SUMMARY`]. Saturating so an out-of-range
/// `max_output` never underflows.
#[must_use]
pub fn effective_context_window(max_output_tokens: u64, context_window: u64) -> u64 {
    let reserved = max_output_tokens.min(MAX_OUTPUT_TOKENS_FOR_SUMMARY);
    context_window.saturating_sub(reserved)
}

/// `effective_window − AUTOCOMPACT_BUFFER_TOKENS`.
///
/// Mirrors `getAutoCompactThreshold` (`autoCompact.ts:72-91`) without the
/// `LINGXI_AUTOCOMPACT_PCT_OVERRIDE` env branch (that lives in the model-aware
/// [`crate::thresholds::auto_compact_threshold`]). Saturating so a tiny window
/// never underflows.
#[must_use]
pub fn auto_compact_threshold(effective_window: u64) -> u64 {
    effective_window.saturating_sub(AUTOCOMPACT_BUFFER_TOKENS)
}

/// `true` when `estimated_tokens − snip_freed >= threshold`.
///
/// Mirrors the proactive auto-compact gate `l3p`→`KRe`/`pFi` in claude-code
/// v2.1.183: `l3p` subtracts the snip offset (`SC(...)-o`) before calling the
/// level classifier, and the classifier's compact arm is `n.enabled&&e>=o`
/// (`>=`, inclusive at the boundary). Snip already removed messages but the
/// surviving assistant usage still reflects pre-snip context, so the freed
/// delta is subtracted before comparing against the threshold. Saturating
/// subtraction so an over-large `snip_freed` clamps to zero rather than wrapping.
#[must_use]
pub fn should_auto_compact(estimated_tokens: u64, snip_freed: u64, threshold: u64) -> bool {
    estimated_tokens.saturating_sub(snip_freed) >= threshold
}

/// The immovable-prefix descriptor returned by [`compaction_prefix_overflow`]
/// when the fixed prefix already exceeds the autocompact threshold.
///
/// 1:1 with the object `a3p` returns in claude-code v2.1.183
/// (offset 203004969): `{prefixTokens, thresholdTokens, totalInputTokens,
/// messagesEstimate, snipTokensFreed, documentBlockCount, imageBlockCount}`.
/// The block counts are diagnostic carry for the
/// `tengu_auto_compact_prefix_overflow` telemetry payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrefixOverflow {
    /// The fixed-prefix token estimate that cannot be compacted away
    /// (`a = max(0, totalInput − snipFreed − messagesEstimate)`).
    pub prefix_tokens: u64,
    /// The autocompact threshold this prefix was compared against (`l`).
    pub threshold_tokens: u64,
    /// `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`
    /// from the last usage snapshot (`s`).
    pub total_input_tokens: u64,
    /// The token estimate of the in-history messages (`i`).
    pub messages_estimate: u64,
    /// Tokens already freed by snip (`r`, subtracted before the prefix).
    pub snip_tokens_freed: u64,
    /// Number of `document` content blocks across the messages.
    pub document_block_count: u32,
    /// Number of `image` content blocks across the messages.
    pub image_block_count: u32,
}

/// Whether the immovable prefix (everything that compaction *cannot* shrink:
/// the system prompt, tools, attachments — i.e. `totalInput − messages`)
/// already exceeds the autocompact threshold, in which case compaction can
/// never bring usage below the threshold.
///
/// 1:1 with `a3p` (claude-code v2.1.183, offset 203004969):
/// ```text
/// function a3p(e,t,n,r=0){
///   let o=Xtt(e); if(!o) return null;
///   let s=o.input_tokens+o.cache_read_input_tokens+o.cache_creation_input_tokens,
///       i=Nv(e,Rw(t)),
///       a=Math.max(0, s - r - i),
///       l=lOt(t,n);
///   if(a<=l) return null;
///   ... count document/image blocks ...
///   return {prefixTokens:a, thresholdTokens:l, totalInputTokens:s,
///           messagesEstimate:i, snipTokensFreed:r, documentBlockCount:c, imageBlockCount:u};
/// }
/// ```
/// Here `s` (the last usage snapshot's input total), `i` (the messages
/// estimate), `r` (snip tokens freed) and `l` (the threshold) are all
/// resolved by the caller — the orchestrator owns the model/usage seam — so
/// this kernel takes the post-resolution numbers, matching the rest of this
/// module. `document_block_count` / `image_block_count` are caller-counted
/// (the orchestrator walks message content) and folded into the descriptor
/// for the `tengu_auto_compact_prefix_overflow` telemetry.
///
/// Returns `Some(PrefixOverflow)` only when the prefix STRICTLY exceeds the
/// threshold (`a > l`, i.e. `a <= l → None`), exactly like the binary. The
/// auto path in claude-code only WARNS + emits telemetry on a non-`None`
/// result and still proceeds (`wouldHaveBlocked:!0`); the reactive PTL path is
/// where it is surfaced to the user as `compactionImpossible`.
#[must_use]
pub fn compaction_prefix_overflow(
    total_input_tokens: u64,
    messages_estimate: u64,
    threshold: u64,
    snip_tokens_freed: u64,
    document_block_count: u32,
    image_block_count: u32,
) -> Option<PrefixOverflow> {
    // a = max(0, totalInput − snipFreed − messagesEstimate). Saturating
    // subtraction mirrors `Math.max(0, …)`.
    let prefix_tokens = total_input_tokens
        .saturating_sub(snip_tokens_freed)
        .saturating_sub(messages_estimate);

    // `if (a <= l) return null;` — only a STRICT overflow yields a descriptor.
    if prefix_tokens <= threshold {
        return None;
    }

    Some(PrefixOverflow {
        prefix_tokens,
        threshold_tokens: threshold,
        total_input_tokens,
        messages_estimate,
        snip_tokens_freed,
        document_block_count,
        image_block_count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec values: window 200k, max_output 20k → effective 180k, threshold 167k.
    #[test]
    fn threshold_arithmetic_matches_spec() {
        let effective = effective_context_window(20_000, 200_000);
        assert_eq!(effective, 180_000);
        assert_eq!(auto_compact_threshold(effective), 167_000);
    }

    #[test]
    fn effective_window_caps_reserved_at_summary_budget() {
        // max_output above the summary cap → reserve only the cap (20k).
        assert_eq!(effective_context_window(64_000, 200_000), 180_000);
        // max_output below the cap → reserve the smaller amount.
        assert_eq!(effective_context_window(8_192, 200_000), 191_808);
    }

    #[test]
    fn effective_window_saturates() {
        // Degenerate window smaller than the reserve → 0, never underflow.
        assert_eq!(effective_context_window(20_000, 10_000), 0);
    }

    #[test]
    fn auto_compact_threshold_saturates() {
        assert_eq!(auto_compact_threshold(5_000), 0);
    }

    #[test]
    fn should_auto_compact_inclusive_at_threshold() {
        // Above the threshold fires.
        assert!(should_auto_compact(167_001, 0, 167_000));
        // Exactly at the threshold fires (inclusive `>=`, matching the
        // binary's `e>=o` compact arm in pFi).
        assert!(should_auto_compact(167_000, 0, 167_000));
        // One below the threshold does NOT fire.
        assert!(!should_auto_compact(166_999, 0, 167_000));
        // Well below does not fire.
        assert!(!should_auto_compact(100_000, 0, 167_000));
    }

    #[test]
    fn should_auto_compact_subtracts_snip_freed() {
        // 170k tokens, but snip already freed 5k → 165k < 167k → no compact.
        assert!(!should_auto_compact(170_000, 5_000, 167_000));
        // Snip freed exactly down to the threshold → 167k >= 167k → compact.
        assert!(should_auto_compact(170_000, 3_000, 167_000));
        // Only 2k freed → 168k >= 167k → compact.
        assert!(should_auto_compact(170_000, 2_000, 167_000));
        // Over-large snip_freed clamps to zero, not above threshold.
        assert!(!should_auto_compact(100_000, 999_999, 167_000));
    }

    // --- a3p prefix-overflow guard ----------------------------------------- //

    #[test]
    fn prefix_overflow_none_when_prefix_at_or_below_threshold() {
        // totalInput 200k, messages 50k → prefix 150k. Threshold 167k.
        // 150k <= 167k → null (binary `if(a<=l)return null`).
        assert_eq!(
            compaction_prefix_overflow(200_000, 50_000, 167_000, 0, 0, 0),
            None
        );
        // Exactly at the threshold is still None (inclusive `<=`).
        // prefix = 167k, threshold 167k → None.
        assert_eq!(
            compaction_prefix_overflow(217_000, 50_000, 167_000, 0, 0, 0),
            None
        );
    }

    #[test]
    fn prefix_overflow_some_when_prefix_strictly_exceeds_threshold() {
        // totalInput 250k, messages 50k → prefix 200k > 167k → overflow.
        let o = compaction_prefix_overflow(250_000, 50_000, 167_000, 0, 3, 2)
            .expect("prefix 200k > 167k must overflow");
        assert_eq!(o.prefix_tokens, 200_000);
        assert_eq!(o.threshold_tokens, 167_000);
        assert_eq!(o.total_input_tokens, 250_000);
        assert_eq!(o.messages_estimate, 50_000);
        assert_eq!(o.snip_tokens_freed, 0);
        assert_eq!(o.document_block_count, 3);
        assert_eq!(o.image_block_count, 2);
    }

    #[test]
    fn prefix_overflow_subtracts_snip_freed() {
        // totalInput 250k, snip freed 40k, messages 50k → prefix = 160k.
        // 160k <= 167k → None (snip already shrank the prefix below threshold).
        assert_eq!(
            compaction_prefix_overflow(250_000, 50_000, 167_000, 40_000, 0, 0),
            None
        );
        // Only 30k freed → prefix 170k > 167k → overflow.
        let o = compaction_prefix_overflow(250_000, 50_000, 167_000, 30_000, 0, 0)
            .expect("prefix 170k > 167k");
        assert_eq!(o.prefix_tokens, 170_000);
        assert_eq!(o.snip_tokens_freed, 30_000);
    }

    #[test]
    fn prefix_overflow_saturates_when_messages_exceed_input() {
        // messages estimate larger than total input → prefix clamps to 0 (never
        // wraps), 0 <= threshold → None.
        assert_eq!(
            compaction_prefix_overflow(50_000, 200_000, 167_000, 0, 0, 0),
            None
        );
    }
}
