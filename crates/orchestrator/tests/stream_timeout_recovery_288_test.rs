//! Real stream/body/server errors carry elapsed-time authority into SDK retries.
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
    phases: Mutex<Vec<bool>>,
    delay_ms: u64,
    error_type: &'static str,
    start_event: bool,
    change_timeout: bool,
}
#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let streaming = body["stream"] == true;
        if !streaming {
            assert_eq!(
                body["stream"], false,
                "current fallback must force the SDK stream flag"
            );
        }
        let first = {
            let mut phases = self.phases.lock().unwrap();
            let first = phases.is_empty();
            phases.push(streaming);
            first
        };
        if !streaming {
            return std::future::pending().await;
        }
        if first && self.change_timeout {
            std::env::set_var("API_TIMEOUT_MS", "10");
        }
        tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
        let mut frames = vec![];
        if self.start_event {
            frames.push(json!({"type":"message_start","message":{"id":"msg_fixture","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}));
        }
        frames.push(json!({"type":"error","error":{"type":self.error_type,"message":"fixture stream failure"}}));
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
fn variable(name: &str, value: Option<&str>) {
    if let Some(value) = value {
        std::env::set_var(name, value)
    } else {
        std::env::remove_var(name)
    }
}
#[tokio::test(start_paused = true)]
async fn native_long_server_streams_reopen_before_bounded_nonstream_recovery() {
    variable(branding::MAX_RETRIES_ENV, None);
    variable(branding::REMOTE_ENV, None);
    variable(
        "CLAUDE_CODE_EXTRA_BODY",
        Some(r#"{"stream":true,"fixture_extra":true}"#),
    );
    let cases = [
        (100, "api_error", false, true, None, 10, false, false, 3, 3),
        (100, "api_error", true, true, None, 10, false, false, 3, 3),
        (0, "api_error", false, true, None, 10, false, false, 1, 10),
        (
            100,
            "timeout_error",
            false,
            true,
            None,
            10,
            false,
            false,
            1,
            3,
        ),
        (
            0,
            "timeout_error",
            false,
            true,
            None,
            10,
            false,
            false,
            1,
            10,
        ),
        (100, "api_error", false, false, None, 10, false, false, 3, 8),
        (
            100,
            "api_error",
            false,
            false,
            Some("0"),
            10,
            false,
            false,
            3,
            1,
        ),
        (100, "api_error", false, true, None, 10, true, false, 3, 0),
        (100, "api_error", false, true, None, 2, false, false, 3, 1),
        (100, "api_error", false, true, None, 1, false, false, 2, 1),
        (100, "api_error", false, true, None, 0, false, false, 1, 1),
        (20, "api_error", false, true, None, 10, false, true, 3, 3),
    ];
    for (
        delay_ms,
        error_type,
        start_event,
        persistent,
        explicit,
        max_retries,
        disabled,
        change_timeout,
        streams,
        nonstreams,
    ) in cases
    {
        variable("API_TIMEOUT_MS", Some("100"));
        variable(
            branding::RETRY_WATCHDOG_ENV,
            Some(if persistent { "1" } else { "0" }),
        );
        variable(branding::NONSTREAMING_TIMEOUT_RETRIES_ENV, explicit);
        variable(
            branding::DISABLE_NONSTREAMING_FALLBACK_ENV,
            Some(if disabled { "1" } else { "0" }),
        );
        let cfg:ClientConfig=serde_json::from_value(json!({"providers":[{"provider_id":"anthropic_first_party","profile_name":"direct","base_url":"https://api.anthropic.com","protocol":"anthropic_messages","auth":"none","credential":{"type":"none"},"models":[{"display_model":"claude-sonnet-4-6","request_model":"claude-sonnet-4-6","billing_model":"claude-sonnet-4-6","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":false}}]}]})).unwrap();
        let capture = Arc::new(Capture {
            phases: Mutex::new(vec![]),
            delay_ms,
            error_type,
            start_event,
            change_timeout,
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
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                std::env::temp_dir(),
            ));
        let _result = orch.run_turn_streaming("fixture").await;
        let phases = capture.phases.lock().unwrap().clone();
        assert_eq!(
            phases.iter().filter(|p| **p).count(),
            streams,
            "{phases:?},delay={delay_ms},kind={error_type},max={max_retries},disabled={disabled}"
        );
        assert_eq!(
            phases.iter().filter(|p| !**p).count(),
            nonstreams,
            "{phases:?},delay={delay_ms},kind={error_type},max={max_retries},disabled={disabled}"
        );
        assert!(phases[..streams].iter().all(|p| *p));
        assert!(phases[streams..].iter().all(|p| !*p));
    }
    for name in [
        "API_TIMEOUT_MS",
        branding::RETRY_WATCHDOG_ENV,
        branding::NONSTREAMING_TIMEOUT_RETRIES_ENV,
        branding::DISABLE_NONSTREAMING_FALLBACK_ENV,
        "CLAUDE_CODE_EXTRA_BODY",
    ] {
        variable(name, None);
    }
}
