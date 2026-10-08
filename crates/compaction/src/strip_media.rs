//! Pre-summarization media stripping — port of `stripImagesFromMessages`
//! (`compact.ts:145-200`).
//!
//! Media recovery replaces images and documents in user messages, including
//! nested tool-result blocks, with `[image]` / `[document]`. Cache-sharing
//! summary requests preserve the original media until this recovery is needed.

use lingxi_core::types::{ContentBlock, ConversationMessage};

/// Text placeholder substituted for a stripped image block — byte-locked to
/// claude-code (`compact.ts:160`).
pub const STRIPPED_IMAGE_PLACEHOLDER: &str = "[image]";

/// Text placeholder substituted for a stripped document block — byte-locked to
/// claude-code's `document` strip placeholder.
pub const STRIPPED_DOCUMENT_PLACEHOLDER: &str = "[document]";

/// Replace image blocks in user messages with a `[image]` text placeholder.
/// Non-user messages and non-image blocks pass through unchanged. 1:1 with
/// `stripImagesFromMessages` for the cases the Rust protocol can represent.
#[must_use]
pub fn strip_images_from_messages(messages: Vec<ConversationMessage>) -> Vec<ConversationMessage> {
    messages.into_iter().map(strip_one).collect()
}

fn strip_one(message: ConversationMessage) -> ConversationMessage {
    // TS strips only `user` messages (`compact.ts:147`); others pass through.
    let ConversationMessage::User {
        id,
        content,
        is_meta,
        is_compact_summary,
        is_visible_in_transcript_only,
     .. } = message
    else {
        return message;
    };
    let new_content = content
        .into_iter()
        .map(|block| match block {
            ContentBlock::Image { .. } => ContentBlock::Text {
                text: STRIPPED_IMAGE_PLACEHOLDER.to_string(), citations: None,
            },
            ContentBlock::Document { .. } => ContentBlock::Text {
                text: STRIPPED_DOCUMENT_PLACEHOLDER.to_string(), citations: None,
            },
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                content_blocks: Some(blocks),
             .. } => ContentBlock::ToolResult { content_projection: None,
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                content_blocks: Some(blocks.into_iter().map(|block| match block.get("type").and_then(serde_json::Value::as_str) {
                    Some("image") => serde_json::json!({"type": "text", "text": STRIPPED_IMAGE_PLACEHOLDER}),
                    Some("document") => serde_json::json!({"type": "text", "text": STRIPPED_DOCUMENT_PLACEHOLDER}),
                    _ => block,
                }).collect()),
            },
            other => other,
        })
        .collect();
    ConversationMessage::User { api_message_override: None,
        id,
        content: new_content,
        is_meta,
        is_compact_summary,
        is_visible_in_transcript_only,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::{ContentBlock, ConversationMessage, ImageSource, MessageId};

    fn image_block() -> ContentBlock {
        ContentBlock::Image {
            source: ImageSource::Url {
                url: "https://img.example/x.png".to_string(),
            },
        }
    }

    #[test]
    fn user_image_block_becomes_placeholder_text() {
        let msg = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![
                ContentBlock::Text {
                    text: "before".to_string(), citations: None,
                },
                image_block(),
                ContentBlock::Text {
                    text: "after".to_string(), citations: None,
                },
            ],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let out = strip_images_from_messages(vec![msg]);
        let ConversationMessage::User { content, .. } = &out[0] else {
            panic!("expected user message");
        };
        // The image became a `[image]` text block; surrounding text is intact
        // and block order/count is preserved.
        assert_eq!(content.len(), 3);
        assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "before"));
        assert!(matches!(&content[1], ContentBlock::Text { text, .. } if text == "[image]"));
        assert!(matches!(&content[2], ContentBlock::Text { text, .. } if text == "after"));
    }

    #[test]
    fn message_without_media_is_untouched() {
        let msg = ConversationMessage::user(MessageId::new(), "just text".to_string());
        let out = strip_images_from_messages(vec![msg.clone()]);
        assert_eq!(out[0], msg);
    }

    #[test]
    fn assistant_messages_are_not_stripped() {
        // TS only strips `user` messages; an assistant message is returned as-is
        // even if it (hypothetically) carried an image block.
        let assistant = ConversationMessage::Assistant { per_turn_effort: None,
            id: MessageId::new(),
            content: vec![image_block()],
            stop_reason: None,
        };
        let out = strip_images_from_messages(vec![assistant.clone()]);
        assert_eq!(out[0], assistant);
    }

    #[test]
    fn strips_nested_media_without_changing_tool_result_identity_or_text() {
        let id = lingxi_core::types::ToolUseId::new();
        let input = ConversationMessage::User { api_message_override: None,
            id: MessageId::new(),
            content: vec![ContentBlock::ToolResult { content_projection: None,
                tool_use_id: id.clone(),
                content: "read result".into(),
                is_error: Some(false),
                provider_tool_use_id: Some("provider-read".into()),
                content_blocks: Some(vec![
                    serde_json::json!({"type":"text", "text":"page 1"}),
                    serde_json::json!({"type":"image", "source":{"data":"large image"}}),
                    serde_json::json!({"type":"document", "source":{"data":"large document"}}),
                ]),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let output = strip_images_from_messages(vec![input]);
        let ConversationMessage::User { content, .. } = &output[0] else {
            panic!("user");
        };
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            content_blocks,
            provider_tool_use_id,
            ..
        } = &content[0]
        else {
            panic!("tool result");
        };
        assert_eq!(tool_use_id, &id);
        assert_eq!(content, "read result");
        assert_eq!(provider_tool_use_id.as_deref(), Some("provider-read"));
        assert_eq!(
            content_blocks.as_ref().unwrap(),
            &vec![
                serde_json::json!({"type":"text", "text":"page 1"}),
                serde_json::json!({"type":"text", "text":"[image]"}),
                serde_json::json!({"type":"text", "text":"[document]"}),
            ]
        );
    }
}
