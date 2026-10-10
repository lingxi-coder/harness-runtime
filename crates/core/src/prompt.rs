//! Assemble the provider-neutral model request emitted by the Core reducer.
//!
//! This boundary keeps conversation messages as typed source-bearing DTOs.
//! Provider codecs and request-body projection belong to `llm-client`.

use crate::session::SessionState;
use crate::types::effects::ModelRequest;

/// Current Core output limit used by the reducer's model request effect.
pub const DEFAULT_MODEL_OUTPUT_TOKENS: u32 = 8192;

/// Build the semantic request from the current session state.
///
/// Conversation messages are cloned as typed DTOs so exact UTF-16 strings,
/// arbitrary provider JSON payloads, and current outer metadata retain their
/// source. This is not a provider-wire serializer.
#[must_use]
pub fn assemble_model_request(session: &SessionState) -> ModelRequest {
    ModelRequest {
        model: session.model.clone(),
        profile: session.model_profile.clone(),
        messages: session.history.clone(),
        max_output_tokens: DEFAULT_MODEL_OUTPUT_TOKENS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionState;
    use crate::types::effects::ModelRequest;
    use crate::types::messages::UsageLimitNote;
    use crate::types::utf16_json::Utf16JsonProjection;
    use crate::types::{ContentBlock, ConversationMessage, MessageId, SessionId};

    fn projection(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("valid JSON source")
    }

    #[test]
    fn assembly_copies_current_session_model_and_history() {
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.model_profile = Some("anthropic".into());
        session.history.push(ConversationMessage::user(
            MessageId::nil(),
            "what's 2+2?".into(),
        ));

        let request = assemble_model_request(&session);
        assert_eq!(request.model, "claude-opus-4-6");
        assert_eq!(request.profile.as_deref(), Some("anthropic"));
        assert_eq!(request.max_output_tokens, DEFAULT_MODEL_OUTPUT_TOKENS);
        assert_eq!(request.messages, session.history);
    }

    #[test]
    fn assembly_retains_exact_utf16_and_arbitrary_source_fields() {
        let citations = projection(r#"{"opaque":[{"text":"\ud800"}]}"#);
        let provider_value = projection(r#"{"unknown":{"\ud800":["\ud800",null]}}"#);
        let tool_input = projection(r#"{"unknown":{"deep":[1,{"key":"\ud800"}]}}"#);
        let origin = projection(r#"{"source":"hook","unknown":{"\ud800":true}}"#);
        let turn_origin =
            Utf16JsonProjection::root_string("mod".into(), "mod".encode_utf16().collect())
                .expect("valid turn origin");
        let resumable = projection("null");
        let exact_units = vec![u16::from(b'x'), 0xD800];

        let user = ConversationMessage::User {
            id: MessageId::nil(),
            content: vec![
                ContentBlock::TextJsUtf16 {
                    text: "x�".into(),
                    utf16_code_units: exact_units.clone(),
                    citations: Some(Some(citations.clone())),
                },
                ContentBlock::ProviderContent {
                    protocol: "anthropic".into(),
                    value: provider_value.clone(),
                },
                ContentBlock::ToolUse {
                    id: crate::types::ToolUseId::from("call_1"),
                    name: "custom".into(),
                    input: tool_input.clone(),
                    provider_id: Some("call_provider_1".into()),
                },
            ],
            is_meta: true,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: Some(Some(true)),
            usage_limit_note: Some(Some(UsageLimitNote::NearLimit)),
            origin: Some(origin.clone()),
            turn_origin: Some(turn_origin.clone()),
            api_message_override: None,
        };
        let assistant = ConversationMessage::Assistant {
            id: MessageId::nil(),
            content: vec![ContentBlock::Text {
                text: "answer".into(),
                citations: None,
            }],
            stop_reason: Some("max_tokens".into()),
            resumable: Some(resumable.clone()),
            per_turn_effort: None,
        };
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.history = vec![user, assistant];

        let request = assemble_model_request(&session);
        assert_eq!(request.messages, session.history);

        let ConversationMessage::User {
            content,
            turn_companion,
            usage_limit_note,
            origin: request_origin,
            turn_origin: request_turn_origin,
            ..
        } = &request.messages[0]
        else {
            panic!("user message remains in request history");
        };
        assert_eq!(*turn_companion, Some(Some(true)));
        assert_eq!(*usage_limit_note, Some(Some(UsageLimitNote::NearLimit)));
        assert_eq!(request_origin.as_ref(), Some(&origin));
        assert_eq!(request_turn_origin.as_ref(), Some(&turn_origin));
        assert!(matches!(
            &content[0],
            ContentBlock::TextJsUtf16 { utf16_code_units, citations: Some(Some(value)), .. }
                if utf16_code_units == &exact_units && value == &citations
        ));
        assert!(matches!(
            &content[1],
            ContentBlock::ProviderContent { value, .. } if value == &provider_value
        ));
        assert!(matches!(
            &content[2],
            ContentBlock::ToolUse { input, provider_id: Some(id), .. }
                if input == &tool_input && id == "call_provider_1"
        ));
        assert!(matches!(
            &request.messages[1],
            ConversationMessage::Assistant { resumable: Some(value), .. } if value == &resumable
        ));

        // Exercise the current raw-value Serde boundary as well as in-memory
        // copying: unknown source, exact keys/values, and optional metadata
        // survive without an intermediate recursive projection.
        let wire = serde_json::to_string(&request).expect("serialize semantic request");
        assert!(wire.contains(r#""\ud800":["\ud800",null]"#), "{wire}");
        assert!(wire.contains(r#""resumable":null"#), "{wire}");
        let decoded: ModelRequest =
            serde_json::from_str(&wire).expect("deserialize semantic request");
        assert_eq!(decoded.messages, request.messages);

        let ConversationMessage::User { content, .. } = &decoded.messages[0] else {
            panic!("decoded request retains user source");
        };
        let ContentBlock::ProviderContent { value, .. } = &content[1] else {
            panic!("provider source remains an opaque source carrier");
        };
        let entries = value
            .at_pointer("/unknown")
            .and_then(|object| object.object_entries())
            .expect("unknown nested object");
        assert_eq!(entries[0].code_units, &[0xD800]);
        assert_eq!(
            entries[0]
                .value
                .get_index(0)
                .and_then(|value| value.code_units()),
            Some(&[0xD800][..])
        );
    }
}
