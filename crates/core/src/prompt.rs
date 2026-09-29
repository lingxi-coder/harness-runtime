//! Build the `Anthropic`-shape request body from session state + new user input.
//!
//! M1.1: minimal — no system prompt yet, no tools, no thinking config.
//! Later plans extend with `system_prompt`, tools list, thinking budget, etc.

use crate::session::SessionState;
use serde_json::{json, Value};

/// Assemble the JSON request body for one turn given the current session and a new user message.
#[must_use]
pub fn assemble_request(session: &SessionState, user_message: &str) -> Value {
    let mut messages: Vec<Value> = session.history.iter().map(message_to_api_shape).collect();
    messages.push(json!({"role": "user", "content": user_message}));

    json!({
        "model": session.model,
        "max_tokens": 8192,
        "messages": messages,
    })
}

fn message_to_api_shape(m: &crate::types::ConversationMessage) -> Value {
    use crate::types::ConversationMessage;
    match m {
        ConversationMessage::User { content, .. } => {
            json!({"role": "user", "content": content_blocks_to_api(content)})
        }
        ConversationMessage::Assistant { content, .. } => {
            json!({"role": "assistant", "content": content_blocks_to_api(content)})
        }
        ConversationMessage::System { content, .. } => {
            json!({"role": "system", "content": content})
        }
    }
}

fn content_blocks_to_api(blocks: &[crate::types::ContentBlock]) -> Value {
    use crate::types::ContentBlock;
    let arr: Vec<Value> = blocks
        .iter()
        .map(|b| match b {
            ContentBlock::ProviderContent { value, .. } => value.clone(),
            ContentBlock::Text { text } | ContentBlock::TextJsUtf16 { text, .. } => {
                json!({"type": "text", "text": text})
            }
            ContentBlock::ToolUse {
                id,
                name,
                input,
                provider_id,
            } => {
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
            } => {
                let wire_id = provider_tool_use_id
                    .clone()
                    .map_or_else(|| json!(tool_use_id), Value::String);
                // claude-code passes a structured content-block array VERBATIM as
                // `tool_result.content` (`mapToolResultToToolResultBlockParam`);
                // fall back to the stringified text when absent.
                let wire_content = content_blocks
                    .as_ref()
                    .map_or_else(|| json!(content), |b| json!(b));
                json!({"type": "tool_result", "tool_use_id": wire_id,
                       "content": wire_content, "is_error": is_error})
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
        })
        .collect();
    Value::Array(arr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;
    use crate::types::SessionId;

    #[test]
    fn assemble_includes_history_and_new_user_message() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let req = assemble_request(&session, "what's 2+2?");
        let body = req.as_object().unwrap();
        assert_eq!(body["model"], "claude-opus-4-6");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn assemble_appends_prior_history() {
        use crate::types::{ConversationMessage, MessageId};
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.history.push(ConversationMessage::user(
            MessageId::nil(),
            "earlier".into(),
        ));
        let req = assemble_request(&session, "now");
        let messages = req["messages"].as_array().unwrap();
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
        }]);
        assert_eq!(v[0]["type"], "image");
        assert_eq!(v[0]["source"]["type"], "base64");
        assert_eq!(v[0]["source"]["media_type"], "image/png");
        assert_eq!(v[0]["source"]["data"], "YQ==");
    }
}
