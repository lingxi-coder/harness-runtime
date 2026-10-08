//! Real prompt-hook streams use native auxiliary dispatch classification.
//! This isolated test binary owns its feature overrides, so concurrent unit
//! tests cannot observe a transient dispatch opt-in.

use async_trait::async_trait;
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::model::thinking::ThinkingConfig;
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{
    ApiService, AuthStrategy, Capabilities, ClientConfig, CredentialConfig, ModelProfile,
    ModelRuntime, PricingConfig, ProtocolFamily, ProviderId, ProviderProfile, SubscriberState,
    Transport,
};
use orchestrator::{
    HookPromptRequest, OrchestratorApiClient, OrchestratorApiRequest, ProviderApiAdapter,
    StreamingApiClient,
};
use std::sync::{Arc, Mutex};

struct DispatchFlags {
    previous_max_retries: Option<std::ffi::OsString>,
    previous_disable_betas: Option<std::ffi::OsString>,
    previous_extra_body: Option<std::ffi::OsString>,
}

impl DispatchFlags {
    fn v2s_only() -> Self {
        let previous_max_retries = std::env::var_os("LINGXI_MAX_RETRIES");
        let previous_disable_betas = std::env::var_os("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        let previous_extra_body = std::env::var_os("CLAUDE_CODE_EXTRA_BODY");
        std::env::set_var("LINGXI_MAX_RETRIES", "0");
        std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
        std::env::remove_var("CLAUDE_CODE_EXTRA_BODY");
        telemetry::test_set_flag("tengu_cedar_lattice", true);
        telemetry::test_set_flag("tengu_dreamy_frost", false);
        Self {
            previous_max_retries,
            previous_disable_betas,
            previous_extra_body,
        }
    }
}

impl Drop for DispatchFlags {
    fn drop(&mut self) {
        telemetry::test_clear_flag("tengu_cedar_lattice");
        telemetry::test_clear_flag("tengu_dreamy_frost");
        match self.previous_max_retries.take() {
            Some(value) => std::env::set_var("LINGXI_MAX_RETRIES", value),
            None => std::env::remove_var("LINGXI_MAX_RETRIES"),
        }
        for (name, previous) in [
            (
                "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
                self.previous_disable_betas.take(),
            ),
            ("CLAUDE_CODE_EXTRA_BODY", self.previous_extra_body.take()),
        ] {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

struct FailedWireCapture(Mutex<Vec<llm_runtime::services::sdk::HttpRequest>>);

#[async_trait]
impl Transport for FailedWireCapture {
    async fn send(
        &self,
        request: llm_runtime::services::sdk::HttpRequest,
    ) -> Result<
        llm_runtime::services::sdk::StreamResponse,
        llm_runtime::services::sdk::protocol::LlmError,
    > {
        self.0.lock().unwrap().push(request);
        Err(llm_runtime::services::sdk::protocol::LlmError::Transport {
            message: "captured connection failure".into(),
        })
    }
}

fn adapter(capture: Arc<FailedWireCapture>) -> ProviderApiAdapter {
    std::env::set_var("HOOK_DISPATCH_287_TEST_KEY", "fixture-key");
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: llm_runtime::Region::all(),
                provider_id: ProviderId::AnthropicFirstParty,
                profile_name: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                protocol: ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::ApiKey,
                credential: CredentialConfig::Env {
                    var: "HOOK_DISPATCH_287_TEST_KEY".into(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-6".into(),
                    request_model: "claude-sonnet-4-6".into(),
                    billing_model: "claude-sonnet-4".into(),
                    aliases: Vec::new(),
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
                        structured_output: true,
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
        .unwrap(),
    );
    let service = ApiService::new_with_routing(
        client,
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    )
    .with_thinking(ThinkingConfig::Disabled)
    .with_forced_tool_choice(llm_runtime::ToolChoice::Tool {
        name: "StructuredOutput".into(),
    });
    ProviderApiAdapter::new(Arc::new(service))
}

fn dispatch(request: &llm_runtime::services::sdk::HttpRequest) -> Option<&str> {
    request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-dispatch-id"))
        .map(|(_, value)| value.as_str())
}

#[tokio::test]
async fn real_hook_stream_has_no_connection_recovery_and_main_resends_v2p() {
    let _flags = DispatchFlags::v2s_only();
    let capture = Arc::new(FailedWireCapture(Mutex::new(Vec::new())));
    let adapter = adapter(capture.clone());
    let hook = HookPromptRequest::new(
        "claude-sonnet-4-6",
        Some("anthropic"),
        "evaluate hook",
        vec![ConversationMessage::user(
            MessageId::new(),
            "allow this action".into(),
        )],
    );
    let hook_error = adapter
        .messages_create(OrchestratorApiRequest::HookPrompt(hook))
        .await
        .unwrap_err();
    assert!(
        matches!(&hook_error, llm_runtime::LlmError::Transport { message } if message == "captured connection failure"),
        "hook must reach the actual failing HTTP transport: {hook_error:?}",
    );
    {
        let requests = capture.0.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "auxiliary hook must not acquire a budget-free header-strip retry"
        );
        assert_eq!(dispatch(&requests[0]), None);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["stream"], true);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(
            body["output_config"]["format"]["schema"]["required"],
            serde_json::json!(["ok", "reason"])
        );
        assert!(requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key")));
    }
    for disabled in ["1", "true", "yes", "on", " TRUE "] {
        std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", disabled);
        capture.0.lock().unwrap().clear();
        let hook = HookPromptRequest::new(
            "claude-sonnet-4-6",
            Some("anthropic"),
            "evaluate hook",
            vec![ConversationMessage::user(
                MessageId::new(),
                "allow this action".into(),
            )],
        );
        adapter
            .messages_create(OrchestratorApiRequest::HookPrompt(hook))
            .await
            .unwrap_err();
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(
            body.get("output_config").is_none(),
            "disable={disabled}: {body}"
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("thinking").is_none());
        assert_eq!(dispatch(&requests[0]), None);
    }
    for enabled in ["0", "false", "FALSE", "wat", ""] {
        std::env::set_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS", enabled);
        capture.0.lock().unwrap().clear();
        let hook = HookPromptRequest::new(
            "claude-sonnet-4-6",
            Some("anthropic"),
            "evaluate hook",
            vec![ConversationMessage::user(
                MessageId::new(),
                "allow this action".into(),
            )],
        );
        adapter
            .messages_create(OrchestratorApiRequest::HookPrompt(hook))
            .await
            .unwrap_err();
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body["output_config"]["format"]["type"], "json_schema",
            "enable={enabled}"
        );
    }
    std::env::remove_var("CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS");
    capture.0.lock().unwrap().clear();
    let system = llm_runtime::SystemPromptInput::source_vector(
        vec!["main".into()],
        None,
        None,
    );
    let main = StreamingApiClient::stream(
        &adapter, "claude-sonnet-4-6", Some("anthropic"), Some(&system),
        vec![ConversationMessage::user(MessageId::new(), "answer".into())],
        vec![serde_json::json!({"name":"StructuredOutput", "description":"schema", "input_schema":{"type":"object", "properties":{}}})],
        "sdk", false, None,
    ).await;
    assert!(main.is_err());
    let requests = capture.0.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "main still owns the current header-degradation retry"
    );
    assert_eq!(dispatch(&requests[0]), Some("v2s"));
    assert_eq!(dispatch(&requests[1]), Some("v2p"));
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["tool_choice"],
        serde_json::json!({"type":"tool", "name":"StructuredOutput"})
    );
}
