//! End-to-end: drive the reducer through a complete single-turn conversation
//! against `MockHttpTransport`. This is the M1.1 acceptance test.

use lingxi_core::{reduce, ConversationState, Event, SessionState, Usage};
use platform_api::HttpTransport;
use protocol::{ConversationMessage, Effect, HttpResponse, MessageId, RequestId, SessionId};
use std::sync::Arc;
use test_harness::mocks::{MockHttpTransport, ScriptedResponse};
use tool_api::anthropic_request::AnthropicRequestBuilder;

#[tokio::test]
async fn single_turn_conversation_against_mock_http() {
    // 1. Setup
    let session = SessionState::empty(SessionId::nil(), "claude-opus-4-6".into());
    let mut state = ConversationState::Idle { session };

    let request_id = RequestId::new();
    let message_id = MessageId::new();

    // 2. User says hi.
    let event = Event::UserMessage {
        message_id,
        request_id,
        content: "hi".into(),
    };
    let (next, effects) = reduce(state, event);
    state = next;

    // Assert: state advanced to AwaitingApiResponse, emitted SendApiRequest.
    match &state {
        ConversationState::AwaitingApiResponse { session, .. } => {
            assert_eq!(session.history.len(), 1);
        }
        other => panic!("unexpected state: {other:?}"),
    }
    let mut saw_send_request = false;
    for e in &effects {
        if let Effect::SendApiRequest {
            request_id: rid, ..
        } = e
        {
            assert_eq!(*rid, request_id);
            saw_send_request = true;
        }
    }
    assert!(saw_send_request);

    // 3. Simulate API stream events arriving.
    let (next, _) = reduce(state, Event::ApiStreamStart { request_id });
    state = next;
    assert!(matches!(state, ConversationState::StreamingResponse { .. }));

    let (next, effects) = reduce(
        state,
        Event::ApiStreamDelta {
            request_id,
            text: "Hello!".into(),
        },
    );
    state = next;
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::RenderStreamDelta { .. })));

    let final_message = ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "Hello!".into(),
        }],
        stop_reason: Some("end_turn".into()),
    };
    let (next, effects) = reduce(
        state,
        Event::ApiStreamEnd {
            request_id,
            final_message,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
        },
    );
    state = next;

    // Assert: back to Idle, assistant message appended, usage updated.
    match &state {
        ConversationState::Idle { session } => {
            assert_eq!(session.history.len(), 2);
            assert_eq!(session.usage.0.input_tokens, 10);
            assert_eq!(session.usage.0.output_tokens, 5);
        }
        other => panic!("unexpected state: {other:?}"),
    }
    assert!(effects
        .iter()
        .any(|e| matches!(e, Effect::RenderTokenUsageUpdate { .. })));
}

#[tokio::test]
async fn anthropic_provider_against_mock_http_does_one_roundtrip() {
    // This proves the AnthropicRequestBuilder + MockHttpTransport pipeline works.
    let transport = Arc::new(MockHttpTransport::new());
    transport.enqueue(ScriptedResponse::Sync(HttpResponse {
        status: 200,
        headers: vec![],
        body: r#"{"id":"msg_test","model":"claude-opus-4-6","content":[{"type":"text","text":"Hi"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"output_tokens":2}}"#.into(),
        body_bytes: Vec::new(),
    }));

    let builder = AnthropicRequestBuilder::new("sk-ant-test", None);
    let body = serde_json::json!({"model":"claude-opus-4-6","max_tokens":1024,"messages":[{"role":"user","content":"hi"}]});
    let req = builder.build_request(&body);
    let resp = transport
        .request(req)
        .await
        .expect("request should succeed");
    assert_eq!(resp.status, 200);
    transport.assert_drained();
}
