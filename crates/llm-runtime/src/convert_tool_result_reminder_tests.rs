use super::{append_anthropic_tool_result_reminder, AnthropicToolResultReminderSuffix};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};

fn root_string(units: Vec<u16>) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units).unwrap()
}

fn tool_result_message(
    id: MessageId,
    tool_use_id: &str,
    content: Utf16JsonProjection,
) -> ConversationMessage {
    ConversationMessage::User {
        id,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: ToolUseId::from(tool_use_id),
            content,
            is_error: Some(true),
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
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
    }
}

fn reminder_message(id: MessageId, text: &Utf16JsonProjection) -> ConversationMessage {
    ConversationMessage::user_meta_js_utf16(
        id,
        text.string_at("").unwrap().to_owned(),
        text.string_units("").unwrap(),
    )
}

#[test]
fn exact_target_appends_utf16_suffix_and_only_removes_its_reminder_from_the_clone() {
    let suffix_units = vec![b'<' as u16, 0xd800, b'>' as u16];
    let suffix_text = root_string(suffix_units.clone());
    let reminder_id = MessageId::new();
    let unrelated_id = MessageId::new();
    let target_message_id = MessageId::new();
    let original_units = vec![b'e' as u16, b'r' as u16, b'r' as u16, 0xdfff];
    let mut messages = vec![
        tool_result_message(
            unrelated_id,
            "toolu_unrelated",
            Utf16JsonProjection::from("unrelated"),
        ),
        tool_result_message(
            target_message_id,
            "toolu_target",
            root_string(original_units.clone()),
        ),
        reminder_message(reminder_id, &suffix_text),
    ];
    let durable_history = messages.clone();
    let suffix = AnthropicToolResultReminderSuffix {
        tool_use_id: ToolUseId::from("toolu_target"),
        reminder_message_id: reminder_id,
        text: suffix_text.clone(),
    };

    assert!(append_anthropic_tool_result_reminder(
        &mut messages,
        &suffix
    ));
    assert_eq!(messages.len(), 2);
    let ConversationMessage::User { content, .. } = &messages[1] else {
        panic!("expected the targeted tool result message");
    };
    let ContentBlock::ToolResult { content, .. } = &content[0] else {
        panic!("expected tool result block");
    };
    let mut expected = original_units;
    expected.extend([b'\n' as u16, b'\n' as u16]);
    expected.extend_from_slice(&suffix_units);
    assert_eq!(content.string_units(""), Some(expected));

    let ConversationMessage::User { content, .. } = &durable_history[1] else {
        panic!("expected the original durable tool result message");
    };
    let ContentBlock::ToolResult { content, .. } = &content[0] else {
        panic!("expected original tool result block");
    };
    assert_eq!(
        content.string_units(""),
        Some(vec![b'e' as u16, b'r' as u16, b'r' as u16, 0xdfff])
    );
    assert_eq!(durable_history[2].id(), reminder_id);
}

#[test]
fn structured_result_adds_a_line_separator_before_a_source_text_reminder_block() {
    let reminder_id = MessageId::new();
    let target_message_id = MessageId::new();
    let suffix_units = vec![b'r' as u16, 0xd800, b'!' as u16];
    let suffix_text = root_string(suffix_units.clone());
    let original_blocks =
        Utf16JsonProjection::parse(r#"[{"type":"text","text":"before \ud800"}]"#).unwrap();
    let mut target = tool_result_message(
        target_message_id,
        "toolu_structured",
        Utf16JsonProjection::from("display fallback"),
    );
    let ConversationMessage::User { content, .. } = &mut target else {
        unreachable!();
    };
    let ContentBlock::ToolResult { content_blocks, .. } = &mut content[0] else {
        unreachable!();
    };
    *content_blocks = Some(original_blocks);
    let mut messages = vec![target, reminder_message(reminder_id, &suffix_text)];
    let suffix = AnthropicToolResultReminderSuffix {
        tool_use_id: ToolUseId::from("toolu_structured"),
        reminder_message_id: reminder_id,
        text: suffix_text,
    };

    assert!(append_anthropic_tool_result_reminder(
        &mut messages,
        &suffix
    ));
    assert_eq!(messages.len(), 1);
    let ConversationMessage::User { content, .. } = &messages[0] else {
        panic!("expected tool result message");
    };
    let ContentBlock::ToolResult {
        content,
        content_blocks: Some(blocks),
        ..
    } = &content[0]
    else {
        panic!("expected structured result blocks");
    };
    assert_eq!(content.string_at(""), Some("display fallback"));
    assert_eq!(blocks.array_len(""), Some(2));
    assert_eq!(
        blocks.subprojection("/0/text").unwrap().string_units(""),
        Some(vec![
            b'b' as u16,
            b'e' as u16,
            b'f' as u16,
            b'o' as u16,
            b'r' as u16,
            b'e' as u16,
            b' ' as u16,
            0xd800,
            b'\n' as u16,
        ]),
    );
    assert_eq!(blocks.string_at("/1/type"), Some("text"));
    assert_eq!(
        blocks.subprojection("/1/text").unwrap().string_units(""),
        Some(suffix_units)
    );
}

#[test]
fn missing_target_keeps_the_standalone_reminder_untouched() {
    let reminder_id = MessageId::new();
    let suffix_text = Utf16JsonProjection::from("<system-reminder>tokens</system-reminder>");
    let mut messages = vec![
        tool_result_message(
            MessageId::new(),
            "toolu_other",
            Utf16JsonProjection::from("original"),
        ),
        reminder_message(reminder_id, &suffix_text),
    ];
    let original = messages.clone();
    let suffix = AnthropicToolResultReminderSuffix {
        tool_use_id: ToolUseId::from("toolu_missing"),
        reminder_message_id: reminder_id,
        text: suffix_text,
    };

    assert!(!append_anthropic_tool_result_reminder(
        &mut messages,
        &suffix
    ));
    assert_eq!(messages.len(), original.len());
    assert_eq!(messages[1].id(), reminder_id);
    let ConversationMessage::User { content, .. } = &messages[0] else {
        panic!("expected original tool result");
    };
    let ContentBlock::ToolResult { content, .. } = &content[0] else {
        panic!("expected original tool result block");
    };
    assert_eq!(content.string_at(""), Some("original"));
}
