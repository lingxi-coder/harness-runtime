use super::redact_ephemeral_tool_result_images;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, ToolUseId};
use serde_json::json;

fn tool_result(
    content: String,
    content_blocks: Option<Vec<serde_json::Value>>,
) -> ConversationMessage {
    ConversationMessage::User { api_message_override: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolResult { content_projection: None,
            tool_use_id: ToolUseId::new(),
            content,
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    }
}

#[test]
fn ephemeral_images_are_removed_only_from_the_persisted_clone() {
    let original = tool_result(
        json!({
            "_lingxi_ephemeral": true,
            "summary": "Temporary screenshot omitted."
        })
        .to_string(),
        Some(vec![json!({
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": "secret"}
        })]),
    );
    let sanitized = redact_ephemeral_tool_result_images(&original);
    let ConversationMessage::User {
        content: sanitized_blocks,
        ..
    } = &sanitized
    else {
        panic!("expected user message");
    };
    let ContentBlock::ToolResult {
        content,
        content_blocks,
        ..
    } = &sanitized_blocks[0]
    else {
        panic!("expected tool result");
    };
    assert_eq!(content, "Temporary screenshot omitted.");
    assert!(content_blocks.is_none());
    let ConversationMessage::User {
        content: original_blocks,
        ..
    } = &original
    else {
        panic!("expected user message");
    };
    let ContentBlock::ToolResult { content_blocks, .. } = &original_blocks[0] else {
        panic!("expected tool result");
    };
    assert!(
        content_blocks.is_some(),
        "live in-memory message must stay intact"
    );
}

#[test]
fn unmarked_tool_results_are_byte_for_byte_unchanged() {
    let original = tool_result(
        "ordinary result".into(),
        Some(vec![json!({"type": "text", "text": "ordinary"})]),
    );
    assert_eq!(redact_ephemeral_tool_result_images(&original), original);
}
