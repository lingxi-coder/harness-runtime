//! Host-created assistant rows for rejected server fallback routes.
//!
//! These rows are query/transcript events, not provider transport errors.
//! Defaults are pinned to Claude Code 2.1.289's `Dyn`/`cs` constructors in
//! `docs/parity/mods-agent-stream-order-2.1.289.json`.

use crate::types::utf16_json::Utf16JsonProjection;
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
    #[serde(
        default,
        serialize_with = "crate::types::utf16_json::raw_json_serde::serialize_option",
        deserialize_with = "crate::types::utf16_json::raw_json_serde::deserialize_option"
    )]
    pub stop_details: Option<Utf16JsonProjection>,
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
    pub fn new(text: &Utf16JsonProjection, timestamp: String) -> Self {
        let uuid = MessageId::new();
        let inner_id = MessageId::new();
        let mut text_units = text
            .string_units("")
            .expect("server fallback row text must be a JSON string");
        if text_units.is_empty() {
            text_units = "(no content)".encode_utf16().collect();
        }
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
                content: vec![ContentBlock::TextJsUtf16 {
                    text: String::from_utf16_lossy(&text_units),
                    utf16_code_units: text_units,
                    citations: None,
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
    pub fn set_refusal(&mut self, request_id: Option<String>, stop_details: Utf16JsonProjection) {
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
            resumable: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_row_matches_native_synthetic_envelope_and_default_usage() {
        let text = Utf16JsonProjection::from("declined");
        let row = ServerFallbackApiErrorRow::new(&text, "2026-10-04T00:00:00.000Z".into());
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
                    "content": [{"type": "text_js_utf16", "text": "declined", "utf16_code_units": [100, 101, 99, 108, 105, 110, 101, 100]}], "context_management": null
                },
                "error": "invalid_request", "isApiErrorMessage": true
            })
        );
        let roundtrip =
            serde_json::from_value::<ServerFallbackApiErrorRow>(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(roundtrip).unwrap(), encoded);
        let roundtrip =
            serde_json::from_value::<ServerFallbackApiErrorRow>(encoded.clone()).unwrap();
        assert!(roundtrip
            .message
            .stop_details
            .as_ref()
            .is_some_and(|details| details.at_pointer("").is_some_and(|root| root.is_null())));

        let mut absent = encoded;
        absent["message"]
            .as_object_mut()
            .expect("message object")
            .remove("stop_details");
        let absent = serde_json::from_value::<ServerFallbackApiErrorRow>(absent).unwrap();
        assert!(absent.message.stop_details.is_none());
    }

    #[test]
    fn refusal_row_retains_request_and_complete_stop_details_without_paid_usage() {
        let text = Utf16JsonProjection::from("refusal text");
        let mut row = ServerFallbackApiErrorRow::new(&text, "created".into());
        let details = json!({
            "type": "refusal", "category": null, "explanation": null,
            "fallback_credit_token": null, "fallback_has_prefill_claim": null,
            "recommended_model": null
        });
        let details_source = Utf16JsonProjection::from_serde_value(&details).unwrap();
        row.set_refusal(Some("request-1".into()), details_source);
        let encoded = serde_json::to_value(&row).unwrap();
        assert_eq!(encoded["requestId"], "request-1");
        assert_eq!(encoded["message"]["stop_reason"], "refusal");
        assert_eq!(encoded["message"]["stop_details"], details);
        assert_eq!(encoded["message"]["usage"]["input_tokens"], 0);
        assert!(
            matches!(row.query_message(), ConversationMessage::Assistant { id, stop_reason: Some(reason), .. } if id == row.uuid && reason == "refusal")
        );
    }

    #[test]
    fn refusal_text_retains_lone_utf16_units_in_the_row_and_query_message() {
        let text = Utf16JsonProjection::parse(r#""\ud800""#).unwrap();
        let row = ServerFallbackApiErrorRow::new(&text, "created".into());

        let Some(ContentBlock::TextJsUtf16 {
            text: display,
            utf16_code_units,
            ..
        }) = row.message.content.first()
        else {
            panic!("server fallback text must carry its UTF-16 source");
        };
        assert_eq!(display, "\u{fffd}");
        assert_eq!(utf16_code_units, &[0xd800]);
        assert!(matches!(
            row.query_message(),
            ConversationMessage::Assistant { content, .. }
                if matches!(content.first(), Some(ContentBlock::TextJsUtf16 { utf16_code_units, .. }) if utf16_code_units == &[0xd800])
        ));
    }

    #[test]
    fn refusal_stop_details_retain_lone_utf16_units_in_serialized_row() {
        let text = Utf16JsonProjection::from("refusal");
        let mut row = ServerFallbackApiErrorRow::new(&text, "created".into());
        let details = Utf16JsonProjection::parse(r#"{"explanation":"\ud800"}"#).unwrap();
        row.set_refusal(Some("request-1".into()), details);

        let serialized = serde_json::to_string(&row).unwrap();
        let parsed = Utf16JsonProjection::parse(&serialized).unwrap();
        assert_eq!(
            parsed.string_units("/message/stop_details/explanation"),
            Some(vec![0xd800])
        );
    }
}
