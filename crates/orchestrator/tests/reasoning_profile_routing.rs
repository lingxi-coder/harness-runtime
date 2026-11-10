//! Live controls must reach the selected profile's physical provider request.
use async_trait::async_trait;
use lingxi_core::host::{OrchestratorHandle, ReasoningSelection};
use llm_runtime::model::user_agent::UserAgentEnv;
use llm_runtime::{ApiService, ClientConfig, ModelRuntime, SubscriberState, Transport};
use orchestrator::test_support::{
    noop_hook_executor, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorApiRequest, OrchestratorConfig,
    ProviderApiAdapter,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Capture(Mutex<Vec<Value>>);

#[async_trait]
impl Transport for Capture {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        self.0
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap());
        // This fixture verifies request admission/encoding, without depending
        // on a provider-specific successful-response or stream decoder.
        Ok(lingxi_llm_client::HttpResponse {
            status: 400,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"error":{"type":"invalid_request_error","message":"request captured"}}"#
                .to_vec()
                .into(),
        }
        .into())
    }
}

struct Case {
    provider: Value,
    profile: &'static str,
    protocol: &'static str,
    base_url: &'static str,
    model: &'static str,
    selection: ReasoningSelection,
    pointer: &'static str,
    expected: Value,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            provider: json!("open_ai"),
            profile: "workspace-openai",
            protocol: "open_ai_responses",
            base_url: "https://api.openai.com/v1",
            model: "gpt-5",
            selection: ReasoningSelection::Level { id: "high".into() },
            pointer: "/reasoning/effort",
            expected: json!("high"),
        },
        Case {
            provider: json!("open_ai"),
            profile: "gemini",
            protocol: "open_ai_responses",
            base_url: "https://api.openai.com/v1",
            model: "gpt-5",
            selection: ReasoningSelection::Level { id: "high".into() },
            pointer: "/reasoning/effort",
            expected: json!("high"),
        },
        Case {
            provider: json!("gemini"),
            profile: "workspace-google",
            protocol: "gemini_generate_content",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            model: "gemini-2.5-flash",
            selection: ReasoningSelection::Disabled,
            pointer: "/generationConfig/thinkingConfig/thinkingBudget",
            expected: json!(0),
        },
        Case {
            provider: json!("gemini"),
            profile: "workspace-google",
            protocol: "gemini_generate_content",
            base_url: "https://generativelanguage.googleapis.com/v1beta",
            model: "gemini-2.5-flash",
            selection: ReasoningSelection::TokenBudget { tokens: 4096 },
            pointer: "/generationConfig/thinkingConfig/thinkingBudget",
            expected: json!(4096),
        },
        Case {
            provider: json!("anthropic_first_party"),
            profile: "workspace-anthropic",
            protocol: "anthropic_messages",
            base_url: "https://api.anthropic.com",
            model: "claude-opus-4-6",
            selection: ReasoningSelection::Level { id: "high".into() },
            pointer: "/output_config/effort",
            expected: json!("high"),
        },
        Case {
            provider: json!({"custom":{"name":"anthropic-proxy"}}),
            profile: "workspace-anthropic-proxy",
            protocol: "anthropic_messages",
            base_url: "https://gateway.example.test",
            model: "claude-opus-4-6",
            selection: ReasoningSelection::Disabled,
            pointer: "/thinking/type",
            expected: json!("disabled"),
        },
    ]
}

fn orchestrator(
    case: &Case,
    capture: Arc<Capture>,
) -> (Arc<ConversationOrchestrator>, Arc<ProviderApiAdapter>) {
    let mut config: ClientConfig = serde_json::from_value(json!({
        "providers": [{
            "provider_id": case.provider,
            "profile_name": case.profile,
            "base_url": case.base_url,
            "protocol": case.protocol,
            "auth": "none",
            "credential": {"type": "none"},
            "models": [{
                "display_model": case.model,
                "request_model": case.model,
                "billing_model": case.model,
                "capabilities": {
                    "streaming": true, "tools": true, "vision": false,
                    "documents": false, "reasoning": true, "structured_output": false
                }
            }]
        }]
    }))
    .unwrap();
    let provider = &mut config.providers[0];
    if let llm_runtime::ProviderId::Custom { name } = &provider.provider_id {
        // Custom endpoints need explicit SDK control facts in addition to the
        // host's broad reasoning capability. wire_profile is not a JSON field.
        provider.wire_profile = Some(
            serde_json::from_value(json!({
                "provider_id": name,
                "profile_name": case.profile,
                "base_url": case.base_url,
                "protocol": case.protocol,
                "auth": "none",
                "inference": {"reasoning": "anthropic"},
                "models": [{
                    "display_model": case.model,
                    "request_model": case.model,
                    "billing_model": case.model,
                    "info": {"features": {
                        "thinking": "supported",
                        "modes": ["enabled", "disabled", "adaptive"]
                    }}
                }]
            }))
            .unwrap(),
        );
    }
    let service = Arc::new(ApiService::new_with_routing(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        capture,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "reasoning-profile-regression",
        None,
        None,
        None,
        Default::default(),
        Some(0),
        None,
    ));
    let adapter = Arc::new(ProviderApiAdapter::new(service));
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: case.model.into(),
            system_prompt_override: Some("Answer briefly.".into()),
            bare: true,
            ..Default::default()
        },
        adapter.clone(),
        adapter.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    (orch, adapter)
}

#[tokio::test]
async fn selected_profile_reasoning_reaches_batched_and_streaming_requests() {
    for streaming in [false, true] {
        for case in cases() {
            let capture = Arc::new(Capture::default());
            let (orch, _) = orchestrator(&case, capture.clone());
            orch.seed_initial_model_profile(case.model, case.profile)
                .await;
            OrchestratorHandle::set_reasoning_selection(orch.as_ref(), case.selection.clone())
                .await
                .unwrap();
            assert_eq!(
                orch.current_reasoning_selection(),
                case.selection,
                "{} {} streaming={streaming}",
                case.profile,
                case.model,
            );

            let _captured_response = if streaming {
                orch.run_turn_streaming("Continue.").await
            } else {
                orch.run_turn("Continue.").await
            };
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), 1, "{} streaming={streaming}", case.profile);
            assert_eq!(
                requests[0].pointer(case.pointer),
                Some(&case.expected),
                "{} {} streaming={streaming}: {}",
                case.profile,
                case.model,
                requests[0],
            );
        }
    }
}

#[tokio::test]
async fn batched_request_effort_override_wins_over_the_session_selection() {
    let case = cases().remove(0);
    let capture = Arc::new(Capture::default());
    let (orch, adapter) = orchestrator(&case, capture.clone());
    orch.seed_initial_model_profile(case.model, case.profile)
        .await;
    OrchestratorHandle::set_reasoning_selection(orch.as_ref(), case.selection)
        .await
        .unwrap();
    let mut request = llm_runtime::MessagesCreateRequest::new(
        case.model,
        Some(case.profile),
        None,
        vec![lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "Continue.".into(),
        )],
        Vec::new(),
    );
    request.opts.effort = Some(json!("low"));
    let _captured_response = adapter
        .messages_create(OrchestratorApiRequest::Main(request))
        .await;
    let requests = capture.0.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].pointer("/reasoning/effort"),
        Some(&json!("low"))
    );
}
