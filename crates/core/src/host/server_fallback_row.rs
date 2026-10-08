//! Host-created assistant rows for rejected server fallback routes.
//!
//! These rows are query/transcript events, not provider transport errors.
//! Defaults are pinned to Claude Code 2.1.289's `Dyn`/`cs` constructors in
//! `docs/parity/mods-agent-stream-order-2.1.289.json`.

use crate::types::{ContentBlock, ConversationMessage, MessageId};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Complete outer row created by the host's API-error constructor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerFallbackApiErrorRow {
    #[serde(rename = "type")]
    pub message_type: String,
    pub uuid: MessageId,
    pub timestamp: String,
    pub message: ServerFallbackApiErrorMessage,
    #[serde(rename = "requestId", skip_serializing_if = "Option::is_none", default)]
    pub request_id: Option<String>,
    pub error: String,
    #[serde(rename = "isApiErrorMessage")]
    pub is_api_error_message: bool,
}

/// Synthetic inner message. Null fields are deliberately serialized because
/// Native's constructor includes them; they differ from omitted outer fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerFallbackApiErrorMessage {
    pub diagnostics: Option<Value>,
    pub id: MessageId,
    pub container: Option<Value>,
    pub model: String,
    pub role: String,
    pub stop_details: Option<Value>,
    pub stop_reason: String,
    pub stop_sequence: String,
    #[serde(rename = "type")]
    pub message_type: String,
    pub usage: Value,
    pub content: Vec<ContentBlock>,
    pub context_management: Option<Value>,
}

impl ServerFallbackApiErrorRow {
    /// Construct Native's generic invalid-request row at its creation time.
    /// The caller supplies the query clock; outer and inner UUIDs are separate.
    #[must_use]
    pub fn new(text: &str, timestamp: String) -> Self {
        let uuid = MessageId::new();
        let inner_id = MessageId::new();
        Self {
            message_type: "assistant".into(),
            uuid,
            timestamp,
            message: ServerFallbackApiErrorMessage {
                diagnostics: None,
                id: inner_id,
                container: None,
                model: "<synthetic>".into(),
                role: "assistant".into(),
                stop_details: None,
                stop_reason: "stop_sequence".into(),
                stop_sequence: String::new(),
                message_type: "message".into(),
                usage: json!({
                    "output_tokens_details": null,
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                    "service_tier": null,
                    "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                    "inference_geo": null,
                    "iterations": null,
                    "speed": null,
                    "fallback_credit": null
                }),
                content: vec![ContentBlock::Text {
                    text: if text.is_empty() {
                        "(no content)"
                    } else {
                        text
                    }
                    .into(), citations: None,
                }],
                context_management: None,
            },
            request_id: None,
            error: "invalid_request".into(),
            is_api_error_message: true,
        }
    }

    /// Apply the refusal helper's changes to the generic row. The text must
    /// already come from the host's actual model/interactive refusal formatter.
    pub fn set_refusal(&mut self, request_id: Option<String>, stop_details: Value) {
        self.request_id = request_id;
        self.message.stop_reason = "refusal".into();
        self.message.stop_details = Some(stop_details);
    }

    /// Raw query row. Complete envelope metadata remains available separately
    /// for append hooks, output events and persistence.
    #[must_use]
    pub fn query_message(&self) -> ConversationMessage {
        ConversationMessage::Assistant {
            id: self.uuid,
            content: self.message.content.clone(),
            stop_reason: Some(self.message.stop_reason.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_row_matches_native_synthetic_envelope_and_default_usage() {
        let row = ServerFallbackApiErrorRow::new("declined", "2026-10-04T00:00:00.000Z".into());
        assert_ne!(row.uuid, row.message.id);
        let encoded = serde_json::to_value(&row).unwrap();
        assert_eq!(
            encoded,
            json!({
                "type": "assistant", "uuid": row.uuid, "timestamp": "2026-10-04T00:00:00.000Z",
                "message": {
                    "diagnostics": null, "id": row.message.id, "container": null,
                    "model": "<synthetic>", "role": "assistant", "stop_details": null,
                    "stop_reason": "stop_sequence", "stop_sequence": "", "type": "message",
                    "usage": {
                        "output_tokens_details": null, "input_tokens": 0, "output_tokens": 0,
                        "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
                        "server_tool_use": {"web_search_requests": 0, "web_fetch_requests": 0},
                        "service_tier": null,
                        "cache_creation": {"ephemeral_1h_input_tokens": 0, "ephemeral_5m_input_tokens": 0},
                        "inference_geo": null, "iterations": null, "speed": null, "fallback_credit": null
                    },
                    "content": [{"type": "text", "text": "declined"}], "context_management": null
                },
                "error": "invalid_request", "isApiErrorMessage": true
            })
        );
        assert_eq!(
            serde_json::from_value::<ServerFallbackApiErrorRow>(encoded).unwrap(),
            row
        );
    }

    #[test]
    fn refusal_row_retains_request_and_complete_stop_details_without_paid_usage() {
        let mut row = ServerFallbackApiErrorRow::new("refusal text", "created".into());
        let details = json!({
            "type": "refusal", "category": null, "explanation": null,
            "fallback_credit_token": null, "fallback_has_prefill_claim": null,
            "recommended_model": null
        });
        row.set_refusal(Some("request-1".into()), details.clone());
        let encoded = serde_json::to_value(&row).unwrap();
        assert_eq!(encoded["requestId"], "request-1");
        assert_eq!(encoded["message"]["stop_reason"], "refusal");
        assert_eq!(encoded["message"]["stop_details"], details);
        assert_eq!(encoded["message"]["usage"]["input_tokens"], 0);
        assert!(
            matches!(row.query_message(), ConversationMessage::Assistant { id, stop_reason: Some(reason), .. } if id == row.uuid && reason == "refusal")
        );
    }
}
