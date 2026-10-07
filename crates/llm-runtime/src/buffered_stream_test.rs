use super::*;
use crate::{ProviderRequest, ProviderResponse};
use futures::StreamExt;
use serde_json::json;

struct BufferedTransport {
    terminal: bool,
    seen: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
}

impl crate::test_support::FixtureTransport for BufferedTransport {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> crate::BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        unreachable!()
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> crate::BoxFuture<'a, Result<crate::StreamingResponse, LlmError>> {
        unreachable!()
    }
    fn send_raw(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> crate::BoxFuture<
        '_,
        Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>,
    > {
        self.seen.lock().unwrap().push(request);
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"response","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"Read","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"file\"}"}}),
            json!({"type":"content_block_stop","index":0}),
        ];
        if self.terminal {
            events.extend([json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})]);
        }
        let body = events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    event["type"].as_str().unwrap(),
                    event
                )
            })
            .collect::<String>();
        Box::pin(async move {
            Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::once(async move { Ok(body.into_bytes().into()) }).boxed(),
            })
        })
    }
}
crate::impl_fixture_transport!(BufferedTransport);

fn fixture(terminal: bool) -> (ApiService, Arc<BufferedTransport>, MessagesCreateRequest) {
    let config:crate::ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":"anthropic_first_party","profile_name":"test","protocol":"anthropic_messages","base_url":"https://api.anthropic.com","auth":"none","credential":{"type":"none"},
        "models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
    }]})).unwrap();
    let client = Arc::new(crate::ModelRuntime::from_config(config).unwrap());
    let transport = Arc::new(BufferedTransport {
        terminal,
        seen: Mutex::new(vec![]),
    });
    let service = ApiService::new_with_routing(
        client,
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        Some(0),
    )
    .with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let mut request = MessagesCreateRequest::new(
        "claude-sonnet-4-6",
        Some("test"),
        None,
        vec![ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "test".into(),
        )],
        vec![
            json!({"name":"Read","description":"Read","input_schema":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}}),
        ],
    );
    request.opts.max_output_tokens = Some(64);
    (service, transport, request)
}

#[tokio::test]
async fn buffered_stream_returns_a_complete_response_with_main_request_options() {
    let (service, transport, request) = fixture(true);
    let response = service
        .messages_create_buffered_stream(request)
        .await
        .unwrap();
    assert_eq!(response.stop_reason.as_deref(), Some("tool_use"));
    assert!(response.content.iter().any(|block| matches!(block, crate::ContentBlock::ToolCall{id,input,..} if id=="call-1" && input==&json!({"path":"file"}))));
    let seen = transport.seen.lock().unwrap();
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["max_tokens"], 64);
    assert_eq!(seen.len(), 1);
}

#[tokio::test]
async fn buffered_stream_does_not_return_completed_tool_blocks_without_a_terminal_response() {
    let (service, transport, request) = fixture(false);
    assert!(service
        .messages_create_buffered_stream(request)
        .await
        .is_err());
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn buffered_stream_preserves_request_admission() {
    let (service, transport, mut request) = fixture(true);
    request.opts.request_dispatch_admission = Some(crate::RequestDispatchAdmission::new(|| false));
    assert!(service
        .messages_create_buffered_stream(request)
        .await
        .is_err());
    assert!(transport.seen.lock().unwrap().is_empty());
}
