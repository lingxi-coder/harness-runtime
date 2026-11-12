//! Group messages into "API rounds" (one API round-trip per group). Used by
//! PTL retry to truncate by-the-round instead of by-the-message.
//!
//! Port of TS `groupMessagesByApiRound` (`grouping.ts:22-63`): a boundary fires
//! when a **new assistant response** begins — i.e. an `Assistant` message whose
//! `id` differs from the most-recently-seen assistant `id`, with the current
//! group non-empty. Streaming chunks from the same API response share an id, so
//! interleaved `[tool_use(id=X), tool_result, tool_use(id=X)]` stays one group;
//! two distinct assistant ids produce two groups; a preamble (no assistant, or a
//! single assistant) collapses to one group.
//!
//! This replaces the prior "split on every user message" boundary, matching TS
//! so PTL head-truncation drops the correct round boundaries.

use crate::post_compact::estimate_content_tokens;
use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId};

/// A contiguous range of messages forming one API round.
#[derive(Debug, Clone)]
pub struct ApiRoundGroup {
    /// Inclusive start index into the messages slice.
    pub start: usize,
    /// Exclusive end index into the messages slice.
    pub end: usize,
    /// Cheap token estimate for the round.
    pub estimated_tokens: u64,
}

/// Split a flat message list into [`ApiRoundGroup`]s on each **new assistant
/// message id** (TS `groupMessagesByApiRound`).
///
/// A boundary is placed before an `Assistant` message whose `id` differs from
/// the previously-seen assistant `id`, provided the current group is non-empty.
#[must_use]
pub fn group_messages_by_api_round(messages: &[ConversationMessage]) -> Vec<ApiRoundGroup> {
    api_round_ranges(messages)
        .map(|range| ApiRoundGroup {
            start: range.start,
            end: range.end,
            estimated_tokens: estimate_tokens_for_range(&messages[range]),
        })
        .collect()
}

/// Count the same round boundaries without examining content. The manual
/// compaction guard only needs this count; its compactor estimates the input.
pub(crate) fn api_round_count(messages: &[ConversationMessage]) -> usize {
    api_round_ranges(messages).count()
}

fn api_round_ranges(
    messages: &[ConversationMessage],
) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    // `id` of the most-recently-seen assistant message. Sole boundary gate:
    // streaming chunks from one API response share an id, so a boundary only
    // fires at the start of a genuinely new round.
    let mut last_assistant_id: Option<MessageId> = None;
    let ends = messages
        .iter()
        .enumerate()
        .filter_map(move |(index, message)| {
            if let ConversationMessage::Assistant { id, .. } = message {
                let begins_round = last_assistant_id != Some(*id);
                last_assistant_id = Some(*id);
                // Every prior boundary is before an already-visited message,
                // so any new assistant after index zero has a non-empty group.
                if begins_round && index > 0 {
                    return Some(index);
                }
            }
            None
        });

    let mut start = 0;
    ends.chain((!messages.is_empty()).then_some(messages.len()))
        .map(move |end| {
            let range = start..end;
            start = end;
            range
        })
}

#[cfg(test)]
thread_local! {
    static BLOCK_ESTIMATE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn block_estimate_calls() -> usize {
    BLOCK_ESTIMATE_CALLS.with(std::cell::Cell::get)
}

/// Claude Code 2.1.261 `Og` / `xno` / `M0` / `HXr`: estimate each API content
/// block independently, using JS UTF-16 length and round-half-up. System
/// transcript markers carry no API message content and contribute zero.
#[must_use]
pub fn estimate_tokens_for_range(msgs: &[ConversationMessage]) -> u64 {
    msgs.iter()
        .map(|message| match message {
            ConversationMessage::User { content, .. }
            | ConversationMessage::Assistant { content, .. } => {
                content.iter().map(estimate_block_tokens).sum()
            }
            ConversationMessage::System { .. } => 0,
        })
        .sum()
}

fn estimate_block_tokens(block: &ContentBlock) -> u64 {
    #[cfg(test)]
    BLOCK_ESTIMATE_CALLS.with(|calls| calls.set(calls.get() + 1));
    match block {
        ContentBlock::Text { text, .. } => estimate_content_tokens(text),
        ContentBlock::TextJsUtf16 {
            utf16_code_units, ..
        } => {
            u64::try_from(utf16_code_units.len())
                .unwrap_or(u64::MAX)
                .saturating_add(2)
                / 4
        }
        ContentBlock::ToolUse { name, input, .. } => estimate_tool_use_tokens(name, input),
        ContentBlock::ToolResult {
            content,
            content_blocks,
            ..
        } => content_blocks.as_ref().map_or_else(
            || estimate_source_string_tokens(content, ""),
            estimate_json_content_tokens,
        ),
        ContentBlock::Thinking { thinking, .. } => estimate_content_tokens(thinking),
        ContentBlock::RedactedThinking { data } => estimate_content_tokens(data),
        ContentBlock::Image { .. } | ContentBlock::Document { .. } => 2_000,
        // Remaining provider blocks use the oracle's JSON-string fallback.
        _ => {
            let mut counter = JsonUnitCounter::default();
            serde_json::to_writer(&mut counter, block).expect("content block serializes");
            units_to_tokens(counter.0)
        }
    }
}

fn units_to_tokens(units: usize) -> u64 {
    u64::try_from(units).unwrap_or(u64::MAX).saturating_add(2) / 4
}

fn estimate_source_string_tokens(source: &Source, pointer: &str) -> u64 {
    source
        .at_pointer(pointer)
        .and_then(|value| value.code_units())
        .map_or(0, |units| units_to_tokens(units.len()))
}

fn estimate_tool_use_tokens(name: &str, input: &Source) -> u64 {
    estimate_tool_use_units(name.encode_utf16().count(), input)
}

fn estimate_tool_use_units(name_units: usize, input: &Source) -> u64 {
    let input_units = if input.at_pointer("").is_some_and(|value| value.is_null()) {
        2 // The oracle substitutes {} for null tool input.
    } else {
        source_json_units(input)
    };
    units_to_tokens(name_units.saturating_add(input_units))
}

/// Count canonical JSON UTF-16 units without allocating the serialized JSON.
/// UTF-8 continuation bytes contribute zero, ASCII/BMP leads one, and astral
/// leads two. This works even if a writer splits one character across writes.
#[derive(Default)]
struct JsonUnitCounter(usize);

impl std::io::Write for JsonUnitCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for &byte in bytes {
            self.0 = self.0.saturating_add(match byte {
                0x00..=0x7f | 0xc0..=0xef => 1,
                0xf0..=0xf7 => 2,
                _ => 0,
            });
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn source_json_units(source: &Source) -> usize {
    // Source is mutable and has no stable content revision/identity. Counting
    // directly avoids both retained input copies and stale ID-based estimates.
    let mut counter = JsonUnitCounter::default();
    source
        .write_json_to(&mut counter)
        .expect("source serializes");
    counter.0
}

fn estimate_json_content_tokens(content: &Source) -> u64 {
    if let Some(units) = content.at_pointer("").and_then(|value| value.code_units()) {
        return units_to_tokens(units.len());
    }
    (0..content.array_len("").unwrap_or(0))
        .map(|index| {
            estimate_json_block_tokens(&content.subprojection(&format!("/{index}")).unwrap())
        })
        .sum()
}

fn estimate_json_block_tokens(block: &Source) -> u64 {
    if let Some(units) = block.at_pointer("").and_then(|value| value.code_units()) {
        return units_to_tokens(units.len());
    }
    match block.string_at("/type") {
        Some("text") => estimate_source_string_tokens(block, "/text"),
        Some("image" | "document") => 2_000,
        Some("tool_result") => block
            .subprojection("/content")
            .ok()
            .as_ref()
            .map_or(0, estimate_json_content_tokens),
        Some("tool_use") => estimate_tool_use_units(
            block
                .at_pointer("/name")
                .and_then(|value| value.code_units())
                .map_or(0, |units| units.len()),
            &block
                .subprojection("/input")
                .unwrap_or_else(|_| Source::parse("null").unwrap()),
        ),
        Some("thinking") => estimate_source_string_tokens(block, "/thinking"),
        Some("redacted_thinking") => estimate_source_string_tokens(block, "/data"),
        _ => units_to_tokens(source_json_units(block)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, MessageId, ToolUseId};
    use serde_json::json;

    fn source(value: &serde_json::Value) -> Source {
        Source::from_serde_value(value).expect("test value is valid JSON")
    }

    #[test]
    fn round_count_skips_content_and_group_totals_match_the_full_estimate() {
        let assistant_id = MessageId::new();
        let system = ConversationMessage::System {
            id: MessageId::new(),
            content: "system markers contribute no API content tokens 😀".into(),
            subtype: None,
            api_system: None,
            compact_metadata: None,
            model_fallback: None,
            refusal_fallback: None,
        };
        let mut unicode = ConversationMessage::user(MessageId::new(), "你好😀".into());
        if let ConversationMessage::User { content, .. } = &mut unicode {
            content.push(ContentBlock::TextJsUtf16 {
                text: "display-safe replacement".into(),
                utf16_code_units: vec![0xd800, 0x61, 0xd83d, 0xde00],
                citations: None,
            });
            content.push(ContentBlock::ProviderContent {
                protocol: "provider-neutral".into(),
                value: Source::parse(r#"{"text":"é😀\ud800","nested":[1,2]}"#).unwrap(),
            });
        }
        let same_round = assistant_with_id(assistant_id, "工具😀");
        let another_round = assistant_with_id(MessageId::new(), "Read");
        let samples = [
            vec![],
            vec![system.clone()],
            vec![unicode.clone(), system.clone()],
            vec![same_round.clone()],
            vec![
                system,
                unicode,
                same_round.clone(),
                user_result(ToolUseId::new()),
                same_round.clone(),
                another_round,
                same_round,
            ],
        ];
        for messages in samples {
            let before = block_estimate_calls();
            let count = api_round_count(&messages);
            assert_eq!(
                block_estimate_calls(),
                before,
                "count reads only boundaries"
            );
            let groups = group_messages_by_api_round(&messages);
            assert_eq!(count, groups.len());
            assert_eq!(
                groups
                    .iter()
                    .map(|group| group.estimated_tokens)
                    .sum::<u64>(),
                estimate_tokens_for_range(&messages)
            );
            assert_eq!(groups.first().map(|group| group.start).unwrap_or(0), 0);
            assert_eq!(
                groups.last().map(|group| group.end).unwrap_or(0),
                messages.len()
            );
            assert!(groups.windows(2).all(|pair| pair[0].end == pair[1].start));
        }
    }

    #[test]
    fn provider_block_count_matches_serialized_utf16_length() {
        let block = ContentBlock::ProviderContent {
            protocol: "test\"protocol".into(),
            value: Source::parse(r#"{"text":"😀\ud800\n"}"#).unwrap(),
        };
        let expected = estimate_content_tokens(&serde_json::to_string(&block).unwrap());
        assert_eq!(estimate_block_tokens(&block), expected);
    }

    #[test]
    fn source_size_tracks_content_changes_and_utf16_json_escaping() {
        let mut input = Source::parse(r#"{"text":"é😀\ud800\n","n":7}"#).unwrap();
        let expected = input.to_json_string().unwrap().encode_utf16().count();
        assert_eq!(source_json_units(&input), expected);
        assert_eq!(source_json_units(&input.clone()), expected);
        input
            .set_field("text", Source::from("different text"))
            .unwrap();
        let changed = input.to_json_string().unwrap().encode_utf16().count();
        assert_eq!(source_json_units(&input), changed);
        assert_ne!(expected, changed);
        // Token rounding includes the name, so count units before rounding
        // and preserve the null-tool-input special case.
        assert_eq!(
            estimate_tool_use_units(1, &Source::parse("null").unwrap()),
            1
        );
    }

    #[test]
    fn json_unit_counter_handles_split_utf8_writes() {
        use std::io::Write;
        let mut counter = JsonUnitCounter::default();
        for byte in "é😀".as_bytes() {
            counter.write_all(&[*byte]).unwrap();
        }
        assert_eq!(counter.0, 3);
    }

    #[test]
    fn oracle_261_estimates_each_wire_block_with_utf16_rounding() {
        let message = ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "你好".into(),
                    citations: None,
                },
                ContentBlock::Text {
                    text: "😀".into(),
                    citations: None,
                },
                ContentBlock::ToolUse {
                    id: ToolUseId::new(),
                    name: "Read".into(),
                    input: source(&json!({})),
                    provider_id: None,
                },
                ContentBlock::Thinking {
                    thinking: "reason".into(),
                    signature: Some("ignored".repeat(100)),
                },
                ContentBlock::RedactedThinking { data: "abc".into() },
            ],
            stop_reason: None,
            resumable: None,
        };
        // vc: round(2/4) + round(2/4) + round(6/4) + round(6/4) + round(3/4).
        assert_eq!(estimate_tokens_for_range(&[message]), 7);
    }

    #[test]
    fn oracle_261_estimates_nested_wire_results_instead_of_display_text() {
        let message = ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: ToolUseId::new(),
                content: "display".repeat(1000).into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: Some(source(&json!([
                    json!({"type":"text", "text":"你好😀"}),
                    json!({"type":"image", "source":{}}),
                    json!({"type":"document", "source":{}}),
                ]))),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        };
        assert_eq!(estimate_tokens_for_range(&[message]), 4_001);
    }

    fn assistant_with_id(id: MessageId, tool: &str) -> ConversationMessage {
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id,
            content: vec![ContentBlock::ToolUse {
                id: ToolUseId::new(),
                name: tool.into(),
                input: source(&json!({})),
                provider_id: None,
            }],
            stop_reason: None,
            resumable: None,
        }
    }

    fn user_result(tool_use_id: ToolUseId) -> ConversationMessage {
        ConversationMessage::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id,
                content: "ok".into(),
                is_error: Some(false),
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        }
    }

    /// Interleaved `[tu_A(id=X), result_A, tu_B(id=X)]` stays one group — the
    /// two assistant blocks share an id, so no boundary fires.
    #[test]
    fn interleaved_same_id_one_group() {
        let id = MessageId::new();
        let tua = ToolUseId::new();
        let msgs = vec![
            assistant_with_id(id, "Read"),
            user_result(tua),
            assistant_with_id(id, "Bash"),
        ];
        let groups = group_messages_by_api_round(&msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].start, 0);
        assert_eq!(groups[0].end, 3);
    }

    /// Two distinct assistant ids → two groups.
    #[test]
    fn two_distinct_ids_two_groups() {
        let id_a = MessageId::new();
        let id_b = MessageId::new();
        let msgs = vec![
            assistant_with_id(id_a, "Read"),
            user_result(ToolUseId::new()),
            assistant_with_id(id_b, "Bash"),
            user_result(ToolUseId::new()),
        ];
        let groups = group_messages_by_api_round(&msgs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].start, 0);
        assert_eq!(groups[0].end, 2);
        assert_eq!(groups[1].start, 2);
        assert_eq!(groups[1].end, 4);
    }

    /// Leading user messages (a preamble) then a *first* assistant: TS splits
    /// the preamble off because the first assistant's id differs from the
    /// (undefined) prior id and `current` is already non-empty. → 2 groups.
    /// This matches TS `groupMessagesByApiRound` byte-for-byte.
    #[test]
    fn preamble_then_first_assistant_splits() {
        let msgs = vec![
            ConversationMessage::user(MessageId::new(), "hello".into()),
            ConversationMessage::user(MessageId::new(), "world".into()),
            assistant_with_id(MessageId::new(), "Read"),
        ];
        let groups = group_messages_by_api_round(&msgs);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].start, 0);
        assert_eq!(groups[0].end, 2);
        assert_eq!(groups[1].start, 2);
        assert_eq!(groups[1].end, 3);
    }

    /// Preamble-only (no assistant message at all) → one group.
    #[test]
    fn no_assistant_one_group() {
        let msgs = vec![
            ConversationMessage::user(MessageId::new(), "a".into()),
            ConversationMessage::user(MessageId::new(), "b".into()),
        ];
        let groups = group_messages_by_api_round(&msgs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].end, 2);
    }

    /// Empty input → no groups.
    #[test]
    fn empty_no_groups() {
        let groups = group_messages_by_api_round(&[]);
        assert!(groups.is_empty());
    }
}
