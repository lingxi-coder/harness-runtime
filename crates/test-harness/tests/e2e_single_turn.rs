//! End-to-end: drive the reducer through a complete single-turn conversation
//! against `MockHttpTransport`. This is the M1.1 acceptance test.

use lingxi_core::types::{
    ConversationMessage, Effect, HttpResponse, MessageId, RequestId, SessionId,
};
use lingxi_core::{reduce, ConversationState, Event, SessionState, Usage};
use llm_runtime::services::sdk;
use std::sync::Arc;
use test_harness::mocks::{MockHttpTransport, ScriptedResponse};

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

    let final_message = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![lingxi_core::types::ContentBlock::Text {
            text: "Hello!".into(), citations: None,
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

    struct SdkFixture(Arc<MockHttpTransport>);
    #[async_trait::async_trait]
    impl sdk::Transport for SdkFixture {
        async fn send(
            &self,
            request: sdk::HttpRequest,
        ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
            llm_runtime::test_support::send_http_fixture(self.0.as_ref(), request).await
        }
    }
    let profile:sdk::protocol::ProviderProfile=serde_json::from_value(serde_json::json!({"provider_id":"anthropic","profile_name":"test","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"api_key","models":[{"request_model":"claude-opus-4-6","display_model":"claude-opus-4-6","billing_model":"claude-opus-4-6"}]})).unwrap();
    let client =
        sdk::LlmClientBuilder::with_transport(Arc::new(SdkFixture(transport.clone())), &[profile])
            .with_region(sdk::protocol::Region::International)
            .build()
            .unwrap();
    let request=serde_json::from_value(serde_json::json!({"model":"claude-opus-4-6","max_tokens":1024,"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]})).unwrap();
    let response = client
        .prepare_on(
            "test",
            &request,
            &sdk::RequestOptions {
                credential: Some("sk-ant-test".to_string().into()),
                ..Default::default()
            },
            sdk::RequestMode::Complete,
        )
        .await
        .unwrap()
        .dispatch_once()
        .await
        .unwrap()
        .collect()
        .await
        .unwrap();
    assert_eq!(
        response.decode().unwrap().usage.usage.unwrap().input_tokens,
        3
    );
    transport.assert_drained();
}
