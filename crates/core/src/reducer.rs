//! Pure-function state machine reducer.
//!
//! Contract: `reduce(state, event) -> (new_state, effects)` is a pure function
//! over `&self`-free inputs. IDs, timestamps, randomness, and I/O are NOT
//! generated here — they arrive in input events or are emitted as effects.

use crate::events::Event;
use crate::prompt::assemble_request;
use crate::state_machine::ConversationState;
use crate::types::{ConversationMessage, Effect};

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
            let request_body = assemble_request(&session, &content);
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

fn event_name(e: &Event) -> &'static str {
    match e {
        Event::UserMessage { .. } => "UserMessage",
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
    use crate::types::{Effect, MessageId, RequestId, SessionId};

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
        assert!(matches!(effects[0], Effect::SendApiRequest { .. }));
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
