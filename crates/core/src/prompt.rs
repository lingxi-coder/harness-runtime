//! Build the `Anthropic`-shape request body from session state + new user input.
//!
//! M1.1: minimal — no system prompt yet, no tools, no thinking config.
//! Later plans extend with `system_prompt`, tools list, thinking budget, etc.

use crate::session::SessionState;
use crate::types::utf16_json::{Utf16JsonProjection, Utf16JsonProjectionError, Utf16JsonString};
use serde_json::{json, Value};

/// Assemble a projected JSON request body from the current session and one
/// exact JavaScript user string. The returned carrier must stay typed until
/// the provider request is serialized.
#[must_use]
pub fn assemble_request(
    session: &SessionState,
    user_message: &Utf16JsonProjection,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    user_message.validate()?;
    let user_text = user_message
        .value
        .as_str()
        .ok_or(Utf16JsonProjectionError::RootStringRequired)?;
    let user_units = user_message
        .string_units("")
        .ok_or(Utf16JsonProjectionError::RootStringRequired)?;

    let mut messages = Vec::with_capacity(session.history.len() + 1);
    let mut strings = Vec::new();
    for (index, message) in session.history.iter().enumerate() {
        let projection = message_to_api_shape(message)?;
        let prefix = format!("/messages/{index}");
        strings.extend(projection.strings.into_iter().map(|sidecar| Utf16JsonString {
            pointer: format!("{prefix}{}", sidecar.pointer),
            code_units: sidecar.code_units,
        }));
        messages.push(projection.value);
    }

    let user_index = messages.len();
    messages.push(json!({"role": "user", "content": user_text}));
    if user_units != user_text.encode_utf16().collect::<Vec<_>>() {
        strings.push(Utf16JsonString {
            pointer: format!("/messages/{user_index}/content"),
            code_units: user_units,
        });
    }

    let projection = Utf16JsonProjection {
        value: json!({
            "model": session.model,
            "max_tokens": 8192,
            "messages": messages,
        }),
        strings,
        keys: Vec::new(),
    };
    projection.validate()?;
    Ok(projection)
}

fn message_to_api_shape(
    m: &crate::types::ConversationMessage,
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    use crate::types::ConversationMessage;
    let (value, content) = match m {
        ConversationMessage::User { content, .. } => {
            (json!({"role": "user"}), Some(content_blocks_to_api(content)?))
        }
        ConversationMessage::Assistant { content, .. } => {
            (
                json!({"role": "assistant"}),
                Some(content_blocks_to_api(content)?),
            )
        }
        ConversationMessage::System { content, .. } => {
            (json!({"role": "system", "content": content}), None)
        }
    };
    let mut projection = Utf16JsonProjection::plain(value);
    if let Some(content) = content {
        projection.set_field("content", content)?;
    }
    Ok(projection)
}

fn content_blocks_to_api(
    blocks: &[crate::types::ContentBlock],
) -> Result<Utf16JsonProjection, Utf16JsonProjectionError> {
    use crate::types::ContentBlock;
    let mut arr = Vec::with_capacity(blocks.len());
    let mut strings = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        let value = match block {
            ContentBlock::ProviderContent { value, .. } => value.clone(),
            ContentBlock::Text { text, citations } => {
                let mut block = json!({"type": "text", "text": text});
                if let Some(citations) = citations {
                    block["citations"] = citations.clone().unwrap_or(Value::Null);
                }
                block
            }
            ContentBlock::TextJsUtf16 {
                text,
                utf16_code_units,
                citations,
            } => {
                let mut block = json!({"type": "text", "text": text});
                if let Some(citations) = citations {
                    block["citations"] = citations.clone().unwrap_or(Value::Null);
                }
                if text.encode_utf16().ne(utf16_code_units.iter().copied()) {
                    strings.push(Utf16JsonString {
                        pointer: format!("/{index}/text"),
                        code_units: utf16_code_units.clone(),
                    });
                }
                block
            }
            ContentBlock::ToolUse {
                id,
                name,
                input,
                provider_id,
             .. } => {
                // Replay the verbatim provider id when preserved; else the
                // serde form of the minted `ToolUseId` (bare uuid).
                let wire_id = provider_id
                    .clone()
                    .map_or_else(|| json!(id), Value::String);
                json!({"type": "tool_use", "id": wire_id, "name": name, "input": input})
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                provider_tool_use_id,
                content_blocks,
             .. } => {
                let wire_id = provider_tool_use_id
                    .clone()
                    .map_or_else(|| json!(tool_use_id), Value::String);
                // claude-code passes a structured content-block array VERBATIM as
                // `tool_result.content` (`mapToolResultToToolResultBlockParam`);
                // fall back to the stringified text when absent.
                let wire_content = content_blocks
                    .as_ref()
                    .map_or_else(|| json!(content), |b| json!(b));
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": wire_id,
                    "content": wire_content,
                });
                if let Some(is_error) = is_error {
                    block["is_error"] = json!(is_error);
                }
                block
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                json!({"type": "thinking", "thinking": thinking, "signature": signature})
            }
            ContentBlock::Image { source } => {
                json!({"type": "image", "source": source})
            }
            ContentBlock::Document { source } => {
                json!({"type": "document", "source": source})
            }
            ContentBlock::RedactedThinking { data } => {
                json!({"type": "redacted_thinking", "data": data})
            }
            ContentBlock::ServerToolUse { id, name, input } => {
                json!({"type": "server_tool_use", "id": id, "name": name, "input": input})
            }
            ContentBlock::ConnectorText {
                connector_text,
                signature,
            } => {
                json!({"type": "connector_text", "connector_text": connector_text, "signature": signature})
            }
            ContentBlock::AdvisorToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                json!({"type": "advisor_tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
            }
            ContentBlock::MediaAnalysis { analysis } => {
                json!({"type": "text", "text": format!("[Media analysis sidecar]\n{}", serde_json::to_string(analysis).unwrap_or_else(|_| "{}".to_string()))})
            }
        };
        arr.push(value);
    }
    let projection = Utf16JsonProjection {
        value: Value::Array(arr),
        strings,
        keys: Vec::new(),
    };
    projection.validate()?;
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;
    use crate::types::{ContentBlock, ConversationMessage, MessageId, SessionId};

    fn user_text(text: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::root_string(text.to_owned(), text.encode_utf16().collect())
            .expect("valid Rust text projection")
    }

    #[test]
    fn assemble_includes_history_and_new_user_message() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let req = assemble_request(&session, &user_text("what's 2+2?"))
            .expect("request projection");
        let body = req.value.as_object().unwrap();
        assert_eq!(body["model"], "claude-opus-4-6");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn assemble_appends_prior_history() {
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.history.push(ConversationMessage::user(
            MessageId::nil(),
            "earlier".into(),
        ));
        let req = assemble_request(&session, &user_text("now"))
            .expect("request projection");
        let messages = req.value["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1]["role"], "user");
    }

    #[test]
    fn image_block_encodes_to_anthropic_image_shape() {
        use crate::types::{ContentBlock, ImageSource};
        let v = content_blocks_to_api(&[ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data: "YQ==".to_string(),
            },
        }])
        .unwrap()
        .value;
        assert_eq!(v[0]["type"], "image");
        assert_eq!(v[0]["source"]["type"], "base64");
        assert_eq!(v[0]["source"]["media_type"], "image/png");
        assert_eq!(v[0]["source"]["data"], "YQ==");
    }

    #[test]
    fn request_projection_preserves_exact_user_and_history_text() {
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.history.push(ConversationMessage::User { api_message_override: None,
            id: MessageId::nil(),
            content: vec![ContentBlock::TextJsUtf16 {
                text: "past�".into(),
                utf16_code_units: vec![u16::from(b'p'), u16::from(b'a'), u16::from(b's'), u16::from(b't'), 0xD800],
                citations: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        });
        let user = Utf16JsonProjection::root_string(
            "next�".into(),
            vec![u16::from(b'n'), u16::from(b'e'), u16::from(b'x'), u16::from(b't'), 0xDC00],
        )
        .unwrap();

        let request = assemble_request(&session, &user).expect("request projection");
        assert_eq!(
            request.string_units("/messages/0/content/0/text"),
            Some(vec![u16::from(b'p'), u16::from(b'a'), u16::from(b's'), u16::from(b't'), 0xD800])
        );
        assert_eq!(
            request.string_units("/messages/1/content"),
            Some(vec![u16::from(b'n'), u16::from(b'e'), u16::from(b'x'), u16::from(b't'), 0xDC00])
        );
        let exact_json = request.to_json_string().unwrap();
        assert!(exact_json.contains(r#""text":"past\ud800""#), "{exact_json}");
        assert!(exact_json.contains(r#""content":"next\udc00""#), "{exact_json}");
    }

    #[test]
    fn valid_unicode_request_stays_a_plain_json_projection() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let request = assemble_request(&session, &user_text("hello 😀"))
            .expect("request projection");
        assert!(request.strings.is_empty());
        assert!(request.keys.is_empty());
        assert_eq!(request.value["messages"][0]["content"], "hello 😀");
    }
}
