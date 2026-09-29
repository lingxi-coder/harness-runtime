//! Regression for final host policy and the raw shared-executor boundary.
use futures::StreamExt;
use lingxi_llm_client as sdk;
use llm_runtime::*;
use std::sync::{Arc, Mutex};

struct RawOnly {
    requests: Mutex<Vec<sdk::HttpRequest>>,
}
impl llm_runtime::test_support::FixtureTransport for RawOnly {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        panic!("legacy collection must not run")
    }
    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        panic!("legacy stream splitting must not run")
    }
    fn send_raw(
        &self,
        request: sdk::HttpRequest,
    ) -> BoxFuture<'_, Result<sdk::StreamResponse, sdk::protocol::LlmError>> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async {
            let body=serde_json::to_vec(&serde_json::json!({"id":"r","model":"wire","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1}})).unwrap();
            Ok(sdk::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::once(async { Ok(body.into()) }).boxed(),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(RawOnly);
fn client() -> ModelRuntime {
    ModelRuntime::from_config(ClientConfig {
        providers: vec![ProviderProfile {
            profile_name: "signed".into(),
            provider_id: ProviderId::BedrockClaude,
            protocol: ProtocolFamily::BedrockClaude,
            base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".into(),
            auth: AuthStrategy::AwsSigV4,
            credential: CredentialConfig::Static { id: "aws".into() },
            signing: Some(SigningConfig {
                region: "us-east-1".into(),
                service: "bedrock".into(),
            }),
            models: vec![ModelProfile {
                display_model: "display".into(),
                request_model: "wire".into(),
                billing_model: "wire".into(),
                aliases: vec![],
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities {
                    streaming: true,
                    tools: true,
                    ..Default::default()
                },
            }],
            wire_profile: None,
            regions: Region::all(),
            pricing: Default::default(),
            azure: None,
            supports_websockets: false,
            supports_websocket_compression: false,
            websocket_connect_timeout_ms: None,
            vision_delegate: None,
            connection: Default::default(),
        }],
    })
    .unwrap()
    .with_credential_provider(Arc::new(StaticCredentialProvider::new(
        Credential::AwsSigV4 {
            access_key_id: "test-access".into(),
            secret_access_key: "test-secret".into(),
            session_token: Some("test-session".into()),
        },
    )))
}

#[tokio::test]
async fn shared_execution_sends_exact_tool_result_string_without_internal_sidecar() {
    let transport = Arc::new(RawOnly {
        requests: Mutex::new(vec![]),
    });
    let service = ApiService::new(
        Arc::new(client()),
        transport.clone(),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    );
    let mut request = LlmRequest::new("display");
    request.input.max_tokens = Some(100);
    assign_history(
        &mut request,
        &[Message {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_call_id: "call-1".into(),
                output: serde_json::Value::Array(::protocol::js_utf16::tool_result_sidecar(vec![
                    65, 0xd83d, 10,
                ])),
                is_error: false,
                cache_control: Some(CacheControl::Ephemeral),
                cache_reference: None,
            }],
        }],
    );
    service.execute_side_query_request(request).await.unwrap();
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let body = std::str::from_utf8(&requests[0].body).unwrap();
    assert!(body.contains(r#""content":"A\ud83d\n""#), "{body}");
    assert!(!body.contains("lingxi_tool_result_string_utf16"));
}
#[tokio::test]
async fn service_signs_the_final_policy_request_and_uses_only_shared_raw_execution() {
    let transport = Arc::new(RawOnly {
        requests: Mutex::new(vec![]),
    });
    let service = ApiService::new(
        Arc::new(client()),
        transport.clone(),
        SubscriberState::default(),
        model::user_agent::UserAgentEnv::default(),
        "test",
        None,
        None,
    );
    let mut request = LlmRequest::new("display").with_user_text("hello");
    request.input.max_tokens = Some(100);
    let response = service.execute_side_query_request(request).await.unwrap();
    assert_eq!(response.usage.billable_tokens.input, 2);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0];
    let headers: std::collections::BTreeMap<_, _> = sent.headers.iter().cloned().collect();
    assert!(headers.contains_key("x-request-id"));
    let mut unsigned = headers.clone();
    let actual = unsigned.remove("Authorization").unwrap();
    let date = unsigned.remove("x-amz-date").unwrap();
    unsigned.remove("x-amz-content-sha256");
    unsigned.remove("x-amz-security-token");
    let signed = sigv4::sign_request(
        &sent.method,
        &sent.url,
        &unsigned,
        &sent.body,
        "test-access",
        "test-secret",
        Some("test-session"),
        "us-east-1",
        "bedrock",
        &date,
    )
    .unwrap();
    assert_eq!(
        actual, signed.authorization,
        "signature must include final host headers and exact sent bytes"
    );
}

fn assign_history(request: &mut llm_runtime::LlmRequest, messages: &[llm_runtime::Message]) {
    let (input, exact_strings) = llm_runtime::convert::history_input(
        &request.input.model,
        messages,
        &[],
        &[],
        lingxi_llm_client::protocol::ProtocolFamily::AnthropicMessages,
    )
    .unwrap();
    request.input.messages = input.messages;
    request.input.prompt_cache = input.prompt_cache;
    request.execution.message_json_string_overrides = exact_strings;
}
