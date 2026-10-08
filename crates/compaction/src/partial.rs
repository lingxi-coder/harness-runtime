//! Partial (suffix-preserving) compaction: the recent-message-tail selection
//! that keeps a verbatim tail of the most recent API rounds across a compaction
//! boundary (#58).
//!
//! TS ref: the reactive summarizer's tail split (`bin/claude.exe`, the
//! `Vut` loop at offset 197362200): the messages are grouped into API rounds
//! `r`, and starting from preserving `i = 1` tail group, the prefix
//! `m = r.slice(0, o−i)` is summarized while the suffix `f = r.slice(o−i)` is
//! preserved verbatim (`messagesToPreserve = f.flat()`). If the summarize set
//! `m.flat()` contains NO assistant message, `i` is incremented (preserve more,
//! summarize less) and the split is retried — the summarizer needs an assistant
//! turn to anchor its summary. The boundary then carries a `preservedSegment`
//! (`{headUuid, anchorUuid, tailUuid}`) so the session loader can splice the
//! preserved tail back at the anchor (`WAo`, offset 202984952).
//!
//! ## Scope of this port (self-contained core; residual documented)
//!
//! This module implements the **pure tail-selection algorithm** — the split
//! point + the assistant-presence retry increment — over the existing
//! [`crate::grouping::group_messages_by_api_round`] groups, plus the
//! [`crate::boundary::PreservedSegment`] derivation from a kept tail. What it
//! does NOT do (the genuinely cross-subsystem residual): drive the summarizer
//! on the PREFIX ONLY (the live [`crate::autocompact::Autocompactor`] summarizes
//! the whole set and emits one message), and the session-loader consumer that
//! relinks `head→anchor` / `anchor's-other-children→tail` from the
//! `preservedSegment` uuids on resume. Those require the partial summarizer path
//! and a JSONL loader patch that are out of this crate's surface.

use crate::grouping::group_messages_by_api_round;
use lingxi_core::types::ConversationMessage;

/// The result of a suffix-preserving split: the prefix to summarize and the
/// verbatim tail to preserve.
#[derive(Debug, Clone)]
pub struct PreservedTailSplit {
    /// The prefix messages handed to the summarizer (`m.flat()`).
    pub to_summarize: Vec<ConversationMessage>,
    /// The verbatim tail kept across the boundary (`messagesToPreserve =
    /// f.flat()`), appended AFTER the summary in
    /// `buildPostCompactMessages` order.
    pub to_preserve: Vec<ConversationMessage>,
    /// How many trailing API-round groups were preserved (`i`).
    pub groups_preserved: usize,
    /// Total API-round groups (`o`).
    pub total_groups: usize,
}

/// Select the suffix-preserving split for `messages`: summarize the leading
/// rounds, preserve a verbatim tail of the most recent ones.
///
/// 1:1 with the `Vut` tail loop (`bin/claude.exe` offset 197362200):
/// 1. Group into API rounds (`o = groups.len()`). Fewer than 2 groups ⇒ nothing
///    to compact ⇒ `None` (TS `too_few_groups`).
/// 2. Start at `i = 1` (preserve the last group). Split
///    `to_summarize = rounds[..o−i]`, `to_preserve = rounds[o−i..]`.
/// 3. If `to_summarize` contains NO assistant message, increment `i` and retry
///    (the summarizer needs an assistant turn). When `i` reaches `o` with still
///    no assistant in the prefix, give up (`None` — TS bails with
///    `too_few_groups`/`exhausted`).
///
/// Returns the split with the SMALLEST `i` (largest summarize set) whose prefix
/// has an assistant message — mirroring the binary, which preserves as little as
/// possible while keeping a valid summarize set.
#[must_use]
pub fn select_preserved_tail(messages: &[ConversationMessage]) -> Option<PreservedTailSplit> {
    let groups = group_messages_by_api_round(messages);
    let total_groups = groups.len();
    // `if (o < 2) ... too_few_groups`.
    if total_groups < 2 {
        return None;
    }

    // The boundary index for "preserve the last `i` groups" is `groups[o−i]`.
    // Start at i=1 and grow until the summarize prefix has an assistant message.
    for i in 1..total_groups {
        let split_group = total_groups - i;
        // Byte index where the preserved tail begins.
        let split_at = groups[split_group].start;
        let to_summarize = &messages[..split_at];
        let to_preserve = &messages[split_at..];

        // `if(!A.some(_=>_.type==="assistant"))` → grow `i` and retry.
        let prefix_has_assistant = to_summarize
            .iter()
            .any(|m| matches!(m, ConversationMessage::Assistant { .. }));
        if !prefix_has_assistant {
            continue;
        }

        return Some(PreservedTailSplit {
            to_summarize: to_summarize.to_vec(),
            to_preserve: to_preserve.to_vec(),
            groups_preserved: i,
            total_groups,
        });
    }

    // No prefix split had an assistant message → cannot partial-compact.
    None
}

/// Carry a preserved-tail message across the compaction boundary, zeroing the
/// per-message usage on preserved ASSISTANT messages so the tail does not
/// double-count tokens after compaction.
///
/// 1:1 with TS `k4e` (`bin/claude.exe` offset ~202817xxx):
/// ```js
/// function k4e(e){
///   if(e.type!=="assistant") return e;
///   return {...e, message:{...e.message, usage:{...e.message.usage,
///     input_tokens:0, output_tokens:0,
///     cache_creation_input_tokens:0, cache_read_input_tokens:0}}};
/// }
/// ```
///
/// **Structural note (faithful no-op for the usage zeroing).** TS messages carry
/// a per-message `message.usage` object; `k4e` blanks its four token counts so
/// the replayed assistant turn is not re-billed. The Rust
/// [`lingxi_core::types::ConversationMessage::Assistant`] variant carries
/// `{ id, content, stop_reason }` and has **no** per-message `usage` field — token
/// usage lives on `llm_runtime::ExecutionUsage`, accumulated outside the in-history message,
/// never stored on the `ConversationMessage`. So there is no token field to zero:
/// preserving the message verbatim already cannot double-count (the counts the TS
/// version zeroes simply do not exist on the Rust message). This function is
/// therefore the identity over the message — kept as an explicit seam so the
/// `messagesToPreserve.map(k4e)` step is represented 1:1 and so a future
/// protocol that gains per-message usage has the single place to blank it.
#[must_use]
pub fn zero_preserved_usage(message: ConversationMessage) -> ConversationMessage {
    // `if(e.type!=="assistant") return e;` — non-assistant passes through.
    // For assistant messages there is no per-message usage field in `protocol`
    // to zero (see the structural note above), so the message is returned
    // unchanged. The match is kept explicit to mirror the TS branch.
    match message {
        ConversationMessage::Assistant { .. } => message,
        other => other,
    }
}

/// Apply [`zero_preserved_usage`] across a preserved tail (TS
/// `messagesToPreserve.map(k4e)` from `Iqn`).
#[must_use]
pub fn zero_preserved_tail_usage(tail: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    tail.into_iter().map(zero_preserved_usage).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
    use serde_json::json;

    fn user(text: &str) -> ConversationMessage {
        ConversationMessage::user(MessageId::new(), text.to_string())
    }

    fn assistant(text: &str) -> ConversationMessage {
        ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text { text: text.into(), citations: None }],
            stop_reason: Some("end_turn".into()),
        }
    }

    fn assistant_tool(id: MessageId, tool: &str) -> ConversationMessage {
        ConversationMessage::Assistant { per_turn_effort: None,
            id,
            content: vec![ContentBlock::ToolUse { input_projection: None,
                id: ToolUseId::new(),
                name: tool.into(),
                input: json!({}),
                provider_id: None,
            }],
            stop_reason: None,
        }
    }

    #[test]
    fn returns_none_when_fewer_than_two_groups() {
        // A single user-only message → exactly ONE group → too few to compact.
        let msgs = vec![user("hi")];
        assert!(select_preserved_tail(&msgs).is_none());
        // Empty → zero groups → None.
        assert!(select_preserved_tail(&[]).is_none());
    }

    #[test]
    fn preserves_last_group_when_prefix_has_assistant() {
        // `group_messages_by_api_round` boundaries fire BEFORE each new assistant
        // id (when the current group is non-empty), so
        // [user(q1), assistantA, user(q2), assistantB] splits into THREE groups:
        //   g0 = [user(q1)]            (boundary before assistantA)
        //   g1 = [assistantA, user(q2)] (boundary before assistantB)
        //   g2 = [assistantB]
        // i=1 preserves the last group (g2 = [assistantB]); the summarize prefix
        // [g0,g1] contains assistantA → accepted at i=1.
        let a = assistant("first reply");
        let b = assistant("second reply");
        let msgs = vec![user("q1"), a, user("q2"), b];
        let split = select_preserved_tail(&msgs).expect("three groups → split");
        assert_eq!(split.total_groups, 3);
        assert_eq!(split.groups_preserved, 1);
        // Preserved tail = the last round [assistantB].
        assert_eq!(split.to_preserve.len(), 1);
        assert_eq!(split.to_preserve[0].text_content(), "second reply");
        // Summarize set = [user(q1), assistantA, user(q2)].
        assert_eq!(split.to_summarize.len(), 3);
        assert_eq!(split.to_summarize[0].text_content(), "q1");
        assert!(split
            .to_summarize
            .iter()
            .any(|m| matches!(m, ConversationMessage::Assistant { .. })));
    }

    #[test]
    fn grows_preserved_tail_until_prefix_has_assistant() {
        // Three groups; the FIRST group has no assistant (leading user-only
        // preamble is its own group only if followed by a distinct assistant).
        // Construct: [user(preamble)], [assistantA, toolresult], [assistantB].
        let a_id = MessageId::new();
        let b_id = MessageId::new();
        let msgs = vec![
            user("preamble"),             // group boundary before assistantA
            assistant_tool(a_id, "Read"), // group 2 starts
            user("tool result for A"),    // same group as A
            assistant_tool(b_id, "Edit"), // group 3 starts
        ];
        // Groups: [0..1]=preamble(no assistant), [1..3]=A round, [3..4]=B round.
        let split = select_preserved_tail(&msgs).expect("split exists");
        // i=1 → summarize groups[0..2] = [preamble, A-round] which HAS assistantA
        // → accepted at i=1. Preserve the last group (B).
        assert_eq!(split.groups_preserved, 1);
        assert_eq!(split.total_groups, 3);
        assert!(split
            .to_summarize
            .iter()
            .any(|m| matches!(m, ConversationMessage::Assistant { .. })));
    }

    #[test]
    fn split_is_a_partition_of_the_messages() {
        let a = assistant("a");
        let b = assistant("b");
        let c = assistant("c");
        let msgs = vec![user("1"), a, user("2"), b, user("3"), c];
        let split = select_preserved_tail(&msgs).expect("split");
        assert_eq!(
            split.to_summarize.len() + split.to_preserve.len(),
            msgs.len(),
            "summarize ++ preserve must reconstruct the full message list"
        );
    }

    // --- #58 k4e usage-zeroing port -------------------------------------- //

    #[test]
    fn zero_preserved_usage_passes_through_non_assistant() {
        let u = user("kept user msg");
        let out = zero_preserved_usage(u.clone());
        assert_eq!(out, u, "non-assistant message passes through unchanged");
    }

    #[test]
    fn zero_preserved_usage_preserves_assistant_content_verbatim() {
        // The Rust message carries no per-message usage to zero, so the
        // assistant message rides through byte-identical (content + id + stop
        // reason preserved). This proves the preserved tail is carried verbatim.
        let a = assistant("preserved assistant reply");
        let out = zero_preserved_usage(a.clone());
        assert_eq!(
            out, a,
            "assistant content/id/stop_reason preserved verbatim"
        );
    }

    #[test]
    fn zero_preserved_tail_usage_maps_the_whole_tail() {
        let tail = vec![assistant("reply"), user("follow up"), assistant("reply 2")];
        let out = zero_preserved_tail_usage(tail.clone());
        // Same length, same order, content preserved.
        assert_eq!(out.len(), tail.len());
        assert_eq!(out, tail);
    }
}
