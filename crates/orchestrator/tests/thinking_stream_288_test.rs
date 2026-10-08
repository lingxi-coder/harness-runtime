//! Both the API adapter and physical SDK transport participate in body retries.
use async_trait::async_trait;
use llm_runtime::model::{thinking::ThinkingConfig, user_agent::UserAgentEnv};
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
struct Capture {
    requests: Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    http_failure: bool,
    error_type: &'static str,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let mut requests = self.requests.lock().unwrap();
        let index = requests.len();
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["stream"], true,
            "thinking-only retries must reopen streaming"
        );
        requests.push(request);
        drop(requests);
        if self.http_failure && index == 0 {
            return Ok(lingxi_llm_client::HttpResponse {
                status: 500,
                headers: vec![],
                body: br#"{"error":{"type":"api_error","message":"fixture"}}"#
                    .to_vec()
                    .into(),
            }
            .into());
        }
        let frames = [
            json!({"type":"message_start","message":{"id":format!("msg_{index}"),"model":"claude-sonnet-4-6","role":"assistant","content":[],"usage":{"input_tokens":100,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"completed reasoning"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fixture-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"error","error":{"type":self.error_type,"message":"fixture provider timeout"}}),
        ];
        let bytes = frames
            .iter()
            .map(|f| format!("data: {f}\n\n"))
            .collect::<String>()
            .into_bytes();
        Ok(lingxi_llm_client::HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: bytes.into(),
        }
        .into())
    }
}
#[tokio::test(start_paused = true)]
async fn current_thinking_retry_counters_reach_real_sdk_requests_and_share_http_budget() {
    for name in [
        branding::MAX_RETRIES_ENV,
        "CLAUDE_CODE_MAX_RETRIES",
        branding::RETRY_WATCHDOG_ENV,
        "CLAUDE_CODE_RETRY_WATCHDOG",
        "API_TIMEOUT_MS",
        "CLAUDE_CODE_EXTRA_BODY",
    ] {
        std::env::remove_var(name);
    }
    for (max_retries, http_failure, error_type, expected) in [
        (0, false, "timeout_error", 1),
        (1, false, "timeout_error", 2),
        (10, false, "timeout_error", 2),
        (2, false, "api_error", 3),
        (1, true, "timeout_error", 2),
        (2, true, "timeout_error", 3),
    ] {
        let cfg:ClientConfig=serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]}]})).unwrap();
        let capture = Arc::new(Capture {
            requests: Mutex::new(vec![]),
            http_failure,
            error_type,
        });
        let service = Arc::new(
            ApiService::new_with_routing(
                Arc::new(ModelRuntime::from_config(cfg).unwrap()),
                capture.clone(),
                SubscriberState::default(),
                UserAgentEnv::default(),
                "test",
                None,
                None,
                None,
                Default::default(),
                Some(max_retries),
                None,
            )
            .with_thinking(ThinkingConfig::Disabled),
        );
        let adapter = Arc::new(ProviderApiAdapter::new(service));
        let output = Arc::new(MockOutputStream::new());
        let orch =
            ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: "claude-sonnet-4-6".into(),
                    interactive_session: true,
                    ..Default::default()
                },
                adapter.clone(),
                adapter,
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output,
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            ));
        orch.run_turn_streaming("finish").await.unwrap();
        assert_eq!(
            capture.requests.lock().unwrap().len(),
            expected,
            "retries={max_retries}, http={http_failure}, type={error_type}"
        );
    }
}
