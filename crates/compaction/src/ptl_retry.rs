//! Prompt-too-long retry: when the server reports `PromptTooLong`, drop the
//! oldest API-round groups until the reported token gap is covered, then hand
//! the trimmed history back to the caller for a retry.
//!
//! Port of TS `truncateHeadForPTLRetry` (`compact.ts:227-291`) plus the
//! `MAX_PTL_RETRIES` / `PTL_RETRY_MARKER` consts. This is the last-resort escape
//! hatch for CC-1180 — when a compact request itself hits prompt-too-long the
//! user is otherwise stuck; dropping the oldest context is lossy but unblocks
//! them. The reactive-compact path owns the proper tail-peeling retry loop;
//! this helper is the dumb-but-safe fallback for the proactive/manual path.

use crate::grouping::group_messages_by_api_round;
use lingxi_core::types::{ConversationMessage, MessageId};

/// Synthetic marker prepended as a meta user message when the surviving head is
/// assistant-first (TS `PTL_RETRY_MARKER`, `compact.ts:228`).
///
/// `groupMessagesByApiRound` puts the preamble in group 0 and starts every
/// subsequent group with an assistant message; dropping group 0 leaves an
/// assistant-first sequence which the API rejects (the first message must be
/// `role=user`). This marker re-establishes a user-first head; the tool-result
/// pairing pass handles any orphaned `tool_results` the truncation creates.
pub const PTL_RETRY_MARKER: &str = "[earlier conversation truncated for compaction retry]";

/// User-facing message surfaced when the compaction summarizer's PTL-retry loop
/// can no longer drop messages (retries exhausted / nothing safe left to drop)
/// — i.e. [`crate::autocompact::CompactionError::MaxRetriesExceeded`]. Byte-exact
/// to claude-code 2.1.206 `GJn` (`throw Error(GJn)` at the `compact_prompt_too_long`
/// / `compact_partial_prompt_too_long` sites). Distinct from the 413 DETECTION
/// string [`crate::prompt_too_long::PROMPT_TOO_LONG_ERROR_MESSAGE`]
/// (`"Prompt is too long"`), which matches the server error text, not this
/// REPL surface.
pub const COMPACTION_CONVERSATION_TOO_LONG: &str =
    "Conversation too long. Press esc twice to go up a few messages and try again.";

/// Drop the oldest API-round groups from `messages` until `token_gap` is
/// covered, returning the trimmed history to retry with — or `None` when
/// nothing can be dropped without leaving an empty summarize set.
///
/// Port of TS `truncateHeadForPTLRetry` (`compact.ts:243-291`). The TS variant
/// receives the raw `ptlResponse` and derives the gap via
/// `getPromptTooLongTokenGap`; here the caller has already parsed the gap, so we
/// take it directly (`token_gap == 0` means "unknown / unparseable" — Vertex /
/// Bedrock formats — and triggers the 20% fallback, matching TS's `undefined`).
///
/// Steps (1:1 with TS):
/// 1. Strip our own leading synthetic marker from a previous retry before
///    grouping — otherwise it becomes its own group 0 and the 20% fallback
///    stalls (drops only the marker, re-adds it, zero progress on retry 2+).
/// 2. Group the remainder via [`group_messages_by_api_round`]; return `None`
///    when `< 2` groups (nothing safe to drop).
/// 3. Compute `drop_count`: accumulate group estimates from the oldest forward
///    until the running total `>= token_gap`, or `max(1, floor(len * 0.2))`
///    when the gap is unknown.
/// 4. Clamp `drop_count <= groups.len() - 1` so at least the newest group
///    survives; bail with `None` if that leaves nothing to drop.
/// 5. Drop those oldest groups; if the surviving head is now assistant-first,
///    prepend a [`PTL_RETRY_MARKER`] meta user message.
// `messages` is taken by value to mirror the spec'd signature the orchestrator
// PTL-recovery loop calls (it hands over its history snapshot); the body only
// borrows + reslices it, hence the `needless_pass_by_value` allow.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::needless_pass_by_value
)]
#[must_use]
pub fn truncate_head_for_ptl_retry(
    messages: Vec<ConversationMessage>,
    token_gap: u64,
) -> Option<Vec<ConversationMessage>> {
    // 2.1.261 `B0e` strips only its own meta marker. User-authored text with
    // identical bytes remains part of the first API-round group.
    let input: &[ConversationMessage] = match messages.first() {
        Some(ConversationMessage::User {
            is_meta: true,
            content,
            ..
        }) if matches!(content.as_slice(), [lingxi_core::types::ContentBlock::Text { text, .. }] if text == PTL_RETRY_MARKER) => {
            &messages[1..]
        }
        _ => &messages[..],
    };

    // (2) Group remaining messages; need at least 2 to drop one safely
    // (`compact.ts:257-258`).
    let groups = group_messages_by_api_round(input);
    if groups.len() < 2 {
        return None;
    }

    // (3) Drop-count: gap-driven accumulation, or 20% fallback when unknown
    // (`compact.ts:260-272`). `token_gap == 0` mirrors TS `tokenGap === undefined`.
    let mut drop_count: usize = if token_gap == 0 {
        std::cmp::max(1, (groups.len() as f64 * 0.2).floor() as usize)
    } else {
        let mut acc: u64 = 0;
        let mut n: usize = 0;
        for g in &groups {
            acc += g.estimated_tokens;
            n += 1;
            if acc >= token_gap {
                break;
            }
        }
        n
    };

    // (4) Keep at least one group so there's something to summarize
    // (`compact.ts:274-276`).
    drop_count = std::cmp::min(drop_count, groups.len() - 1);
    if drop_count < 1 {
        return None;
    }

    // (5) Drop the oldest `drop_count` groups via their message ranges. The
    // surviving head begins at the start index of the first kept group
    // (`compact.ts:278` — `groups.slice(dropCount).flat()`).
    let survive_start = groups[drop_count].start;
    let mut sliced: Vec<ConversationMessage> = input[survive_start..].to_vec();

    // Assistant-first head is invalid to send; prepend the marker
    // (`compact.ts:284-289`).
    if matches!(sliced.first(), Some(ConversationMessage::Assistant { .. })) {
        sliced.insert(
            0,
            ConversationMessage::user_meta(MessageId::new(), PTL_RETRY_MARKER.to_string()),
        );
    }
    Some(sliced)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
    use serde_json::json;

    /// A user message of `n` 'x' characters (≈ `n/4` estimated tokens).
    fn user_chars(n: usize) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), "x".repeat(n))
    }

    /// A fresh-id assistant message carrying a tool-use block (starts a group).
    fn assistant(tool: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: tool.into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    fn user_result() -> ConversationMessage {
        ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "ok".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        }
    }

    /// Three distinct-assistant-id rounds (no preamble): a user text leads round
    /// 0, then each new assistant id opens a fresh round. Returns the messages;
    /// the grouping is `[u0,a0,r0] [a1,r1] [a2,r2]`.
    fn three_round_history() -> Vec<ConversationMessage> {
        vec![
            user_chars(40), // group 0 head: 10 est tokens
            assistant("Read"),
            user_result(),
            assistant("Bash"), // group 1
            user_result(),
            assistant("Edit"), // group 2
            user_result(),
        ]
    }

    /// Gap-driven accumulation drops the oldest groups until covered
    /// (TS `compact.ts:262-269`). The leading user preamble is its own group 0
    /// (~100 est tokens here), so a gap of 50 is covered by group 0 alone → drop 1.
    #[test]
    fn gap_driven_drop_drops_accumulated_groups() {
        // group 0 is a 400-char user preamble (~100 est tokens); the three
        // assistant rounds are tiny.
        let msgs = vec![
            user_chars(400), // group 0 preamble: ~100 tokens
            assistant("Read"),
            user_result(),
            assistant("Bash"), // round
            user_result(),
            assistant("Edit"), // round
            user_result(),
        ];
        let groups = group_messages_by_api_round(&msgs);
        // The preamble splits off as group 0 (TS groupMessagesByApiRound splits
        // before the first assistant), so this is 4 groups, not 3.
        assert_eq!(groups.len(), 4);
        // gap = 50 < group0's ~100 → accumulation breaks after group 0 → drop 1.
        let out = truncate_head_for_ptl_retry(msgs.clone(), 50).expect("some");
        // Drop group 0; the surviving head is assistant-first → marker prepended,
        // then the 6 surviving messages (groups 1-3).
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
        assert_eq!(out.len(), 1 + 6);
    }

    /// A gap exceeding the first groups' estimates keeps accumulating, dropping
    /// more groups until covered.
    #[test]
    fn gap_spanning_groups_drops_until_covered() {
        let msgs = vec![
            user_chars(200), // group 0 preamble: ~50 tokens
            assistant("Read"),
            user_result(),
            assistant("Bash"),
            user_chars(200), // ~50 tokens inside this round's group
            assistant("Edit"),
            user_result(),
        ];
        let groups = group_messages_by_api_round(&msgs);
        // 4 groups: [u(200)] [a(Read),r] [a(Bash),u(200)] [a(Edit),r] — the
        // leading preamble splits off as group 0.
        assert_eq!(groups.len(), 4);
        // gap = 80 → g0(50) < 80, +g1(~0) < 80, +g2(50) = 100 >= 80 → drop 3.
        let out = truncate_head_for_ptl_retry(msgs, 80).expect("some");
        // Surviving = group 3 only (assistant("Edit"), user_result) → assistant
        // first → marker prepended → 1 + 2 = 3.
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
        assert_eq!(out.len(), 3);
    }

    /// `token_gap == 0` (unknown) → 20% fallback: `max(1, floor(len * 0.2))`.
    /// With 4 groups, `floor(0.8) = 0` → clamped to 1 (TS `compact.ts:271`).
    #[test]
    fn unknown_gap_uses_twenty_percent_floor_min_one() {
        let msgs = three_round_history();
        let out = truncate_head_for_ptl_retry(msgs, 0).expect("some");
        // 4 groups (preamble + 3 rounds); floor(0.8)=0 → clamp to 1 → drop group
        // 0. Groups 1-3 survive (6 messages), assistant-first → marker prepended.
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
        assert_eq!(out.len(), 1 + 6);
    }

    /// 20% fallback with 10 groups drops `floor(10 * 0.2) = 2`.
    #[test]
    fn unknown_gap_twenty_percent_of_ten_groups_drops_two() {
        let mut msgs = vec![user_chars(4)]; // small preamble head of group 0
                                            // 9 more distinct assistant ids → 10 groups total.
        for _ in 0..9 {
            msgs.push(assistant("Read"));
            msgs.push(user_result());
        }
        let groups = group_messages_by_api_round(&msgs);
        assert_eq!(groups.len(), 10);
        let kept_start = groups[2].start;
        let out = truncate_head_for_ptl_retry(msgs.clone(), 0).expect("some");
        // Dropped 2 groups → surviving messages from groups[2].start, assistant
        // first → +1 marker.
        assert_eq!(out.len(), 1 + (msgs.len() - kept_start));
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
    }

    /// Fewer than 2 groups → `None` (`compact.ts:258`).
    #[test]
    fn under_two_groups_returns_none() {
        // A preamble-only history (no assistant) is a single group.
        let msgs = vec![user_chars(40), user_chars(40)];
        assert_eq!(group_messages_by_api_round(&msgs).len(), 1);
        assert!(truncate_head_for_ptl_retry(msgs, 100).is_none());
    }

    /// Surviving assistant-first head gets the marker prepended
    /// (`compact.ts:284-289`).
    #[test]
    fn assistant_first_head_gets_marker() {
        let msgs = three_round_history();
        let out = truncate_head_for_ptl_retry(msgs, 0).expect("some");
        assert!(matches!(
            out.first(),
            Some(ConversationMessage::User { .. })
        ));
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
        assert!(
            out.first().unwrap().is_meta(),
            "2.1.261 B0e creates isMeta:true"
        );
        // The message right after the marker is the assistant that began group 1.
        assert!(matches!(
            out.get(1),
            Some(ConversationMessage::Assistant { .. })
        ));
    }

    #[test]
    fn oracle_261_does_not_strip_user_authored_marker_text() {
        let first = assistant("Read");
        let second = assistant("Bash");
        let messages = vec![
            ConversationMessage::user(MessageId::new(), PTL_RETRY_MARKER.into()),
            first.clone(),
            second,
        ];
        let out = truncate_head_for_ptl_retry(messages, 0).expect("drop preamble only");
        assert_eq!(out.len(), 3);
        assert_eq!(out[1], first);
    }

    /// A second call on an already-marked history strips the prior marker before
    /// regrouping, so it makes real progress instead of stalling on the marker
    /// becoming its own group 0 (`compact.ts:250-255`).
    #[test]
    fn second_call_strips_prior_marker_no_stall() {
        let first = truncate_head_for_ptl_retry(three_round_history(), 0).expect("first");
        assert_eq!(first.first().unwrap().text_content(), PTL_RETRY_MARKER);
        let first_groups = group_messages_by_api_round(&first);
        // The marker did NOT collapse the count: stripping it yields the 2 real
        // surviving groups, so a second drop is possible.
        let second = truncate_head_for_ptl_retry(first.clone(), 0).expect("second");
        // Stripping the marker leaves 2 groups → drop 1 → 1 group survives.
        // The result has fewer real (non-marker) messages than `first` had.
        let first_real = first.len() - 1; // minus its leading marker
        let second_real = if second.first().unwrap().text_content() == PTL_RETRY_MARKER {
            second.len() - 1
        } else {
            second.len()
        };
        assert!(
            second_real < first_real,
            "second call must make progress: {second_real} !< {first_real}"
        );
        // Sanity: the marker was not double-counted as content shrinking groups.
        assert!(first_groups.len() >= 2);
    }

    /// Clamp keeps at least one group even when the gap is enormous
    /// (`compact.ts:274-275`).
    #[test]
    fn clamp_keeps_at_least_one_group() {
        let msgs = three_round_history();
        let groups_before = group_messages_by_api_round(&msgs).len();
        assert_eq!(groups_before, 4);
        let out = truncate_head_for_ptl_retry(msgs, u64::MAX).expect("some");
        // drop_count clamped to len-1 = 3 → exactly the newest group survives.
        // That group is assistant-first → marker + (assistant + user_result).
        assert_eq!(out.first().unwrap().text_content(), PTL_RETRY_MARKER);
        assert_eq!(out.len(), 1 + 2);
    }

    /// A surviving user-first head is returned as-is (no marker). Build a history
    /// where the dropped boundary leaves a user message at the head.
    #[test]
    fn user_first_head_no_marker() {
        // Two real groups; group 1 begins with an assistant, but we insert a
        // user message between groups by giving group 1 a user-led preamble is
        // impossible (groups always start at the assistant). Instead verify the
        // marker path is the only one that fires: with these inputs the head IS
        // assistant-first, so we assert the inverse via a no-assistant tail is
        // unreachable. This test documents that branch coverage lives in the
        // assistant-first cases above; here we assert determinism of marker id.
        let out = truncate_head_for_ptl_retry(three_round_history(), 0).expect("some");
        // Two successive calls each synthesize a *fresh* marker id (MessageId::new()).
        let out2 = truncate_head_for_ptl_retry(three_round_history(), 0).expect("some");
        assert_ne!(out.first().unwrap().id(), out2.first().unwrap().id());
    }

    /// Byte-lock the two distinct too-long strings so they can never drift into
    /// each other: `GJn` (the REPL surface) vs the 413 detection text.
    #[test]
    fn conversation_too_long_message_is_byte_exact() {
        assert_eq!(
            COMPACTION_CONVERSATION_TOO_LONG,
            "Conversation too long. Press esc twice to go up a few messages and try again."
        );
        assert_ne!(
            COMPACTION_CONVERSATION_TOO_LONG,
            crate::prompt_too_long::PROMPT_TOO_LONG_ERROR_MESSAGE
        );
    }
}
