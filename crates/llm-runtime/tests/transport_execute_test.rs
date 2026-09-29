use lingxi_llm_client::protocol::{ContentBlock, StopReason};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use llm_runtime::client::ModelRuntime;
use llm_runtime::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig, LlmError, LlmRequest,
    ModelProfile, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile, ProviderRequest,
    ProviderResponse, StreamingResponse, Transport,
};

#[derive(Debug)]
struct FakeTransport {
    response: ProviderResponse,
    seen: Mutex<Option<ProviderRequest>>,
}

impl FakeTransport {
    fn returning(response: ProviderResponse) -> Self {
        Self {
            response,
            seen: Mutex::new(None),
        }
    }
}

impl llm_runtime::test_support::FixtureTransport for FakeTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        *self.seen.lock().expect("seen lock") = Some(request.clone());
        let response = self.response.clone();
        Box::pin(async move { Ok(response) })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "open_stream not scripted".to_string(),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(FakeTransport);

fn anthropic_client() -> ModelRuntime {
    std::env::set_var("LLM_CLIENT_TRANSPORT_TEST_KEY", "transport-key");
    ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            wire_profile: None,
            regions: lingxi_llm_client::protocol::Region::all(),
            provider_id: ProviderId::AnthropicFirstParty,
            profile_name: "anthropic".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            protocol: ProtocolFamily::AnthropicMessages,
            auth: AuthStrategy::ApiKey,
            credential: CredentialConfig::Env {
                var: "LLM_CLIENT_TRANSPORT_TEST_KEY".to_string(),
            },
            models: vec![ModelProfile {
                display_model: "Claude".to_string(),
                request_model: "claude-sonnet-4-20250514".to_string(),
                billing_model: "claude-sonnet-4".to_string(),
                aliases: vec!["claude".to_string()],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            pricing: PricingConfig::default(),
            signing: None,
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .expect("client")
}

#[tokio::test]
async fn execute_sends_authenticated_request_and_decodes_response() {
    let transport = Arc::new(FakeTransport::returning(ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_1",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"text","text":"hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 3}
        }),
    )));
    let client = anthropic_client();

    let response = client
        .execute(
            &LlmRequest::new("claude").with_user_text("hello"),
            transport.clone(),
        )
        .await
        .expect("response");

    assert!(
        matches!(response.message.content.as_slice(), [ContentBlock::Text { text, .. }] if text == "hi")
    );
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.complete().unwrap().input_tokens, 9);

    let seen = transport
        .seen
        .lock()
        .expect("seen lock")
        .clone()
        .expect("request sent");
    assert_eq!(seen.url, "https://api.anthropic.com/v1/messages");
    assert_eq!(
        seen.headers.get("x-api-key").map(String::as_str),
        Some("transport-key")
    );
    assert_eq!(seen.body_json["model"], "claude-sonnet-4-20250514");
}

#[tokio::test]
async fn execute_routes_provider_errors_through_taxonomy() {
    let mut error_response = ProviderResponse::json(
        429,
        serde_json::json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "slow down"}
        }),
    );
    error_response
        .headers
        .insert("retry-after".to_string(), "7".to_string());
    let transport = Arc::new(FakeTransport::returning(error_response));
    let client = anthropic_client();

    let error = client
        .execute(
            &LlmRequest::new("claude").with_user_text("hello"),
            transport.clone(),
        )
        .await
        .expect_err("must map to taxonomy");

    assert!(matches!(
        error,
        LlmError::RateLimited { retry_after: Some(after), .. } if after == Duration::from_secs(7)
    ));
}

#[tokio::test]
async fn execute_propagates_transport_failures() {
    #[derive(Debug)]
    struct FailingTransport;
    impl llm_runtime::test_support::FixtureTransport for FailingTransport {
        fn execute<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "connection refused".to_string(),
                })
            })
        }
        fn open_stream<'a>(
            &'a self,
            _request: &'a ProviderRequest,
        ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
            Box::pin(async move {
                Err(LlmError::Transport {
                    message: "connection refused".to_string(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(FailingTransport);

    let client = anthropic_client();

    let error = client
        .execute(
            &LlmRequest::new("claude").with_user_text("hello"),
            Arc::new(FailingTransport),
        )
        .await
        .expect_err("transport failure");

    assert!(matches!(error, LlmError::Transport { message } if message.contains("refused")));
}

#[tokio::test]
async fn provider_services_reuses_configured_model_routes_and_host_credentials() {
    use llm_runtime::services::sdk;

    let transport = Arc::new(FakeTransport::returning(ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_service",
            "model": "claude-sonnet-4-20250514",
            "content": [{"type":"text","text":"hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 9, "output_tokens": 3}
        }),
    )));
    let client = anthropic_client();
    let services = client
        .provider_services(sdk::protocol::Region::International, transport.clone())
        .expect("services");
    let request: sdk::protocol::ChatRequest = serde_json::from_value(serde_json::json!({
        "model": "claude",
        "messages": [{"role":"user", "content":[{"type":"text", "text":"hello"}]}]
    }))
    .expect("request");
    services
        .client()
        .chat()
        .complete(&request, &sdk::RequestOptions::default())
        .await
        .expect("service model response");
    let seen = transport.seen.lock().unwrap();
    let seen = seen.as_ref().expect("authenticated request");
    assert_eq!(
        seen.headers.get("x-api-key").map(String::as_str),
        Some("transport-key")
    );
    assert_eq!(seen.body_json["model"], "claude-sonnet-4-20250514");
}
