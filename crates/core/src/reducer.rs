//! Pure-function state machine reducer.
//!
//! Contract: `reduce(state, event) -> (new_state, effects)` is a pure function
//! over `&self`-free inputs. IDs, timestamps, randomness, and I/O are NOT
//! generated here — they arrive in input events or are emitted as effects.

use crate::events::Event;
use crate::prompt::assemble_request;
use crate::state_machine::ConversationState;
use crate::types::utf16_json::Utf16JsonProjection;
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
        // Idle + UserMessage → AwaitingApiResponse (append history, emit send).
        (
            ConversationState::Idle { mut session },
            Event::UserMessage {
                message_id,
                request_id,
                content,
            },
        ) => {
            let user_text = Utf16JsonProjection::root_string(
                content.clone(),
                content.encode_utf16().collect(),
            )
            .expect("a Rust string always has a valid UTF-16 projection");
            let request_body = match assemble_request(&session, &user_text) {
                Ok(body) => body,
                Err(_) => return reject_user_projection(session, "UserMessage"),
            };
            session
                .history
                .push(ConversationMessage::user(message_id, content));
            (
                ConversationState::AwaitingApiResponse {
                    session,
                    request_id,
                },
                vec![Effect::SendApiRequest {
                    request_id,
                    request_body,
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
            let user_text = match Utf16JsonProjection::root_string(
                content.clone(),
                utf16_code_units.clone(),
            ) {
                Ok(projection) => projection,
                Err(_) => return reject_user_projection(session, "UserMessageJsUtf16"),
            };
            let request_body = match assemble_request(&session, &user_text) {
                Ok(body) => body,
                Err(_) => return reject_user_projection(session, "UserMessageJsUtf16"),
            };
            session.history.push(ConversationMessage::User { api_message_override: None,
                id: message_id,
                content: vec![ContentBlock::TextJsUtf16 {
                    text: content,
                    utf16_code_units,
                    citations: None,
                }],
                is_meta: false,
                is_compact_summary: false,
                is_visible_in_transcript_only: false,
            });
            (
                ConversationState::AwaitingApiResponse {
                    session,
                    request_id,
                },
                vec![Effect::SendApiRequest {
                    request_id,
                    request_body,
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
    fn idle_plus_user_message_yields_awaiting_api_with_send_effect() {
        let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
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
        let Effect::SendApiRequest { request_body, .. } = &effects[0] else {
            panic!("user message must produce a request");
        };
        assert!(request_body.strings.is_empty(), "valid Unicode stays plain");
        assert_eq!(request_body.value["messages"][0]["content"], "hi");
    }

    #[test]
    fn exact_user_message_survives_history_and_send_effect() {
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
        let ConversationMessage::User { content, .. } = &session.history[0] else {
            panic!("exact user message should remain in history");
        };
        assert!(matches!(
            content.as_slice(),
            [ContentBlock::TextJsUtf16 { utf16_code_units, .. }]
                if utf16_code_units == &[u16::from(b'x'), 0xD800]
        ));

        let Effect::SendApiRequest { request_body, .. } = &effects[0] else {
            panic!("exact user message should produce a request");
        };
        assert_eq!(
            request_body.string_units("/messages/0/content"),
            Some(vec![u16::from(b'x'), 0xD800])
        );
        assert!(request_body
            .to_json_string()
            .unwrap()
            .contains(r#""content":"x\ud800""#));

        // The typed sidecar must also survive the effect's transport serde.
        let expected_body_json = request_body.to_json_string().unwrap();
        let wire = serde_json::to_string(&effects[0]).unwrap();
        let decoded: Effect = serde_json::from_str(&wire).unwrap();
        let Effect::SendApiRequest { request_body, .. } = decoded else {
            panic!("send API effect");
        };
        assert_eq!(request_body.to_json_string().unwrap(), expected_body_json);
    }

    #[test]
    fn malformed_exact_user_projection_is_rejected_without_sending() {
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
