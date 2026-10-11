use super::total_token_reminder_suffixes_from_history;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use std::collections::HashMap;

fn tool_result_message(tool_use_id: &str, text: &str) -> ConversationMessage {
    ConversationMessage::User {
        api_message_override: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from(tool_use_id),
            content: Utf16JsonProjection::from(text),
            is_error: None,
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

fn total_tokens_reminder(id: MessageId, body: &str) -> ConversationMessage {
    ConversationMessage::user_meta(id, format!("<system-reminder>\n{body}\n</system-reminder>"))
}

fn attachment(body: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&format!(
        r#"{{"type":"total_tokens_reminder","text":"{body}"}}"#
    ))
    .unwrap()
}

#[test]
fn cold_replayed_sidecars_rebuild_multiple_suffixes_for_a_later_request() {
    let first_reminder_id = MessageId::new();
    let second_reminder_id = MessageId::new();
    let first_body = "<total_tokens>140 tokens left</total_tokens>";
    let second_body = "<total_tokens>90 tokens left</total_tokens>";
    let mut snapshot = vec![
        tool_result_message("toolu_first", "first result"),
        total_tokens_reminder(first_reminder_id, first_body),
        ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: Vec::new(),
            stop_reason: Some("end_turn".into()),
            resumable: None,
        },
        tool_result_message("toolu_second", "second result"),
        total_tokens_reminder(second_reminder_id, second_body),
    ];
    let sidecars = HashMap::from([
        (first_reminder_id, attachment(first_body)),
        (second_reminder_id, attachment(second_body)),
    ]);
    let cold_sidecars = sidecars
        .iter()
        .map(|(id, source)| {
            let persisted = source.to_json_string().unwrap();
            (*id, Utf16JsonProjection::parse(&persisted).unwrap())
        })
        .collect::<HashMap<_, _>>();
    // The restored sidecar payload has only Native's existing attachment fields;
    // tool-use targeting is reconstructed from typed history order, not JSONL text.
    assert!(!cold_sidecars[&first_reminder_id].contains_property("tool_use_id"));
    assert!(!cold_sidecars[&second_reminder_id].contains_property("tool_use_id"));

    let suffixes = total_token_reminder_suffixes_from_history(&snapshot, &cold_sidecars);
    assert_eq!(suffixes.len(), 2);
    assert_eq!(suffixes[0].tool_use_id.as_str(), "toolu_first");
    assert_eq!(suffixes[0].reminder_message_id, first_reminder_id);
    assert_eq!(suffixes[1].tool_use_id.as_str(), "toolu_second");
    assert_eq!(suffixes[1].reminder_message_id, second_reminder_id);

    for suffix in &suffixes {
        assert!(llm_runtime::convert::append_anthropic_tool_result_reminder(
            &mut snapshot,
            suffix,
        ));
    }
    assert_eq!(snapshot.len(), 3);
    let targeted_outputs = snapshot
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, .. } => content.iter().find_map(|block| {
                if let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } = block
                {
                    Some((tool_use_id.as_str(), content.string_at("")))
                } else {
                    None
                }
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(targeted_outputs.len(), 2);
    let first_reminder_source = format!("<system-reminder>\n{first_body}\n</system-reminder>");
    let second_reminder_source = format!("<system-reminder>\n{second_body}\n</system-reminder>");
    assert!(targeted_outputs[0]
        .1
        .unwrap()
        .ends_with(first_reminder_source.as_str()));
    assert!(targeted_outputs[1]
        .1
        .unwrap()
        .ends_with(second_reminder_source.as_str()));
    assert_eq!(
        snapshot
            .iter()
            .filter(
                |message| message.id() == first_reminder_id || message.id() == second_reminder_id
            )
            .count(),
        0,
    );
}

#[test]
fn source_scanner_does_not_attach_across_a_human_message_boundary() {
    let reminder_id = MessageId::new();
    let body = "<total_tokens>90 tokens left</total_tokens>";
    let snapshot = vec![
        tool_result_message("toolu_old", "old result"),
        ConversationMessage::user(MessageId::new(), "next human turn".into()),
        total_tokens_reminder(reminder_id, body),
    ];
    let sidecars = HashMap::from([(reminder_id, attachment(body))]);

    assert!(total_token_reminder_suffixes_from_history(&snapshot, &sidecars).is_empty());
}
