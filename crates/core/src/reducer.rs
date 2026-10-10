//! Pure-function state machine reducer.
//!
//! Contract: `reduce(state, event) -> (new_state, effects)` is a pure function
//! over `&self`-free inputs. IDs, timestamps, randomness, and I/O are NOT
//! generated here — they arrive in input events or are emitted as effects.

use crate::events::Event;
use crate::prompt::assemble_model_request;
use crate::state_machine::ConversationState;
use crate::types::{ContentBlock, ConversationMessage, Effect};

/// Reduce one (state, event) pair to (new state, effects to emit).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn reduce(state: ConversationState, event: Event) -> (ConversationState, Vec<Effect>) {
    // Terminated is absorbing.
    if let ConversationState::Terminated { .. } = state {
        return (state, Vec::new());
    }

    match (state, event) {
        // Idle + UserMessage → AwaitingApiResponse (append history, request model).
        (
            ConversationState::Idle { mut session },
            Event::UserMessage {
                message_id,
                request_id,
                content,
            },
        ) => {
            session
                .history
                .push(ConversationMessage::user(message_id, content));
            let request = assemble_model_request(&session);
            (
                ConversationState::AwaitingApiResponse {
                    session,
                    request_id,
                },
                vec![Effect::RequestModel {
                    request_id,
                    request,
                }],
            )
        }

        // Preserve exact JS units from hook-generated teammate messages in
        // both the request effect and session history.
        (
            ConversationState::Idle { mut session },
            Event::UserMessageJsUtf16 {
                message_id,
                request_id,
                content,
                utf16_code_units,
            },
        ) => {
            if String::from_utf16_lossy(&utf16_code_units) != content {
                return reject_user_projection(session, "UserMessageJsUtf16");
            }
            session.history.push(ConversationMessage::User {
                id: message_id,
                content: vec![ContentBlock::TextJsUtf16 {
                    text: content,
                    utf16_code_units,
                    citations: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
                turn_companion: None,
                usage_limit_note: None,
                origin: None,
                turn_origin: None,
                api_message_override: None,
            });
            let request = assemble_model_request(&session);
            (
                ConversationState::AwaitingApiResponse {
                    session,
                    request_id,
                },
                vec![Effect::RequestModel {
                    request_id,
                    request,
                }],
            )
        }

        // AwaitingApiResponse + ApiStreamStart → StreamingResponse.
        (
            ConversationState::AwaitingApiResponse {
                session,
                request_id: rid_state,
            },
            Event::ApiStreamStart {
                request_id: rid_evt,
            },
        ) if rid_state == rid_evt => (
            ConversationState::StreamingResponse {
                session,
                request_id: rid_state,
                partial_text: String::new(),
            },
            Vec::new(),
        ),

        // StreamingResponse + ApiStreamDelta → accumulate + emit RenderStreamDelta.
        (
            ConversationState::StreamingResponse {
                session,
                request_id: rid_state,
                mut partial_text,
            },
            Event::ApiStreamDelta {
                request_id: rid_evt,
                text,
            },
        ) if rid_state == rid_evt => {
            partial_text.push_str(&text);
            (
                ConversationState::StreamingResponse {
                    session,
                    request_id: rid_state,
                    partial_text,
                },
                vec![Effect::RenderStreamDelta { text }],
            )
        }

        // StreamingResponse + ApiStreamEnd → Idle (append final assistant message + usage).
        (
            ConversationState::StreamingResponse {
                mut session,
                request_id: rid_state,
                ..
            },
            Event::ApiStreamEnd {
                request_id: rid_evt,
                final_message,
                usage,
            },
        ) if rid_state == rid_evt => {
            session.usage.add(&usage);
            session.history.push(final_message);
            let usage_effect = Effect::RenderTokenUsageUpdate {
                input_tokens: session.usage.0.input_tokens,
                output_tokens: session.usage.0.output_tokens,
            };
            (ConversationState::Idle { session }, vec![usage_effect])
        }

        // AwaitingApiResponse | StreamingResponse + ApiError → Idle + RenderError.
        (
            ConversationState::AwaitingApiResponse { session, .. }
            | ConversationState::StreamingResponse { session, .. },
            Event::ApiError { error, .. },
        ) => (
            ConversationState::Idle { session },
            vec![Effect::RenderError {
                error: error.message,
            }],
        ),

        // Anywhere + UserExit → Terminated.
        (state, Event::UserExit) => {
            let session = state.session().clone();
            (
                ConversationState::Terminated {
                    session,
                    reason: "user_exit".into(),
                },
                vec![Effect::Terminate {
                    reason: "user_exit".into(),
                }],
            )
        }

        // Catch-all: emit a diagnostic effect (no panic, no log call — purity).
        (state, event) => {
            let effect = Effect::RecordUnexpectedEvent {
                state_name: state.kind_name().into(),
                event_name: event_name(&event).into(),
            };
            (state, vec![effect])
        }
    }
}

fn reject_user_projection(
    session: crate::session::SessionState,
    event_name: &str,
) -> (ConversationState, Vec<Effect>) {
    (
        ConversationState::Idle { session },
        vec![Effect::RecordUnexpectedEvent {
            state_name: "Idle".into(),
            event_name: format!("{event_name}InvalidUtf16Projection"),
        }],
    )
}

fn event_name(e: &Event) -> &'static str {
    match e {
        Event::UserMessage { .. } => "UserMessage",
        Event::UserMessageJsUtf16 { .. } => "UserMessageJsUtf16",
        Event::PeerMessage { .. } => "PeerMessage",
        Event::UserInterrupt => "UserInterrupt",
        Event::UserExit => "UserExit",
        Event::ApiStreamStart { .. } => "ApiStreamStart",
        Event::ApiStreamDelta { .. } => "ApiStreamDelta",
        Event::ApiStreamEnd { .. } => "ApiStreamEnd",
        Event::ApiError { .. } => "ApiError",
        Event::SessionLoaded(_) => "SessionLoaded",
        Event::CostRecorded { .. } => "CostRecorded",
        Event::BudgetThresholdReached { .. } => "BudgetThresholdReached",
        Event::BudgetExceeded { .. } => "BudgetExceeded",
        Event::PermissionGranted { .. } => "PermissionGranted",
        Event::PermissionDenied { .. } => "PermissionDenied",
        Event::SecretDetected { .. } => "SecretDetected",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Event;
    use crate::session::SessionState;
    use crate::state_machine::ConversationState;
    use crate::types::{ContentBlock, Effect, MessageId, RequestId, SessionId};

    #[test]
    fn idle_plus_user_message_emits_semantic_model_request() {
        let mut session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        session.model_profile = Some("anthropic".into());
        let state = ConversationState::Idle {
            session: session.clone(),
        };
        let event = Event::UserMessage {
            message_id: MessageId::nil(),
            request_id: RequestId::nil(),
            content: "hi".into(),
        };
        let (next, effects) = reduce(state, event);

        match next {
            ConversationState::AwaitingApiResponse {
                session,
                request_id,
            } => {
                assert_eq!(session.history.len(), 1, "user message appended to history");
                assert_eq!(request_id, RequestId::nil());
            }
            other => panic!("unexpected state: {other:?}"),
        }

        assert_eq!(effects.len(), 1);
        let Effect::RequestModel {
            request_id: effect_request_id,
            request,
        } = &effects[0]
        else {
            panic!("user message must produce a request");
        };
        assert_eq!(*effect_request_id, RequestId::nil());
        assert_eq!(request.model, "claude-opus-4-6");
        assert_eq!(request.profile.as_deref(), Some("anthropic"));
        assert_eq!(request.max_output_tokens, 8192);
        assert_eq!(request.messages.len(), 1);
        assert!(matches!(
            &request.messages[0],
            ConversationMessage::User { content, .. }
                if matches!(content.as_slice(), [ContentBlock::Text { text, .. }] if text == "hi")
        ));
    }

    #[test]
    fn exact_user_message_survives_history_and_model_request_effect() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let (next, effects) = reduce(
            ConversationState::Idle { session },
            Event::UserMessageJsUtf16 {
                message_id: MessageId::nil(),
                request_id: RequestId::nil(),
                content: "x�".into(),
                utf16_code_units: vec![u16::from(b'x'), 0xD800],
            },
        );

        let ConversationState::AwaitingApiResponse { session, .. } = next else {
            panic!("exact user message should start a request");
        };
        let ConversationMessage::User {
            content,
            turn_companion,
            usage_limit_note,
            origin,
            turn_origin,
            ..
        } = &session.history[0]
        else {
            panic!("exact user message should remain in history");
        };
        assert_eq!(turn_companion, &None);
        assert_eq!(usage_limit_note, &None);
        assert!(origin.is_none());
        assert!(turn_origin.is_none());
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 { utf16_code_units, .. }]
                if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));

        let Effect::RequestModel { request, .. } = &effects[0] else {
            panic!("exact user message should produce a request");
        };
        let ConversationMessage::User { content, .. } = &request.messages[0] else {
            panic!("model request retains the exact user message");
        };
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 { utf16_code_units, .. }]
                if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));

        // Exact units remain typed through the semantic effect's serde DTO.
        let wire = serde_json::to_string(&effects[0]).unwrap();
        let decoded: Effect = serde_json::from_str(&wire).unwrap();
        let Effect::RequestModel { request, .. } = decoded else {
            panic!("request model effect");
        };
        let ConversationMessage::User { content, .. } = &request.messages[0] else {
            panic!("decoded request retains the exact user message");
        };
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 { utf16_code_units, .. }]
                if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));
    }

    #[test]
    fn malformed_exact_user_message_is_rejected_without_requesting_model() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
        let (next, effects) = reduce(
            ConversationState::Idle { session },
            Event::UserMessageJsUtf16 {
                message_id: MessageId::nil(),
                request_id: RequestId::nil(),
                content: "x".into(),
                utf16_code_units: vec![u16::from(b'y')],
            },
        );
        assert!(matches!(next, ConversationState::Idle { .. }));
        assert!(matches!(
            effects.as_slice(),
            [Effect::RecordUnexpectedEvent { event_name, .. }]
                if event_name == "UserMessageJsUtf16InvalidUtf16Projection"
        ));
    }

    #[test]
    fn terminated_is_absorbing() {
        let session = SessionState::empty(SessionId::nil(), "x".into());
        let state = ConversationState::Terminated {
            session,
            reason: "ok".into(),
        };
        let (next, effects) = reduce(state, Event::UserInterrupt);
        assert!(next.is_terminal());
        assert!(effects.is_empty());
    }

    #[test]
    fn unexpected_event_emits_record_effect() {
        let session = SessionState::empty(SessionId::nil(), "x".into());
        let state = ConversationState::Idle { session };
        let event = Event::ApiStreamDelta {
            request_id: RequestId::nil(),
            text: "x".into(),
        };
        let (_, effects) = reduce(state, event);
        assert!(effects
            .iter()
            .any(|e| matches!(e, Effect::RecordUnexpectedEvent { .. })));
    }
}
