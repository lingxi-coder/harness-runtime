//! End-to-end Native Main thinking latch and retry-budget tests.

use super::*;
use crate::test_support::FixtureTransport;
use crate::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig, LlmError, ModelProfile,
    PricingConfig, ProviderProfile, ProviderRequest, ProviderResponse, StreamingResponse,
};
use lingxi_llm_client::protocol::NativeThinkingTypeRejection;
use std::sync::{Arc, Mutex};

struct ScriptedNativeTransport {
    responses: Vec<ProviderResponse>,
    seen: Mutex<Vec<ProviderRequest>>,
}

impl ScriptedNativeTransport {
    fn new(responses: Vec<ProviderResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl FixtureTransport for ScriptedNativeTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let mut seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.push(request.clone());
        let index = (seen.len() - 1).min(self.responses.len().saturating_sub(1));
        let response = self.responses[index].clone();
        Box::pin(async move { Ok(response) })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async {
            Err(LlmError::UnsupportedCapability {
                capability: "streaming is not used by this test".into(),
            })
        })
    }
}

llm_runtime::impl_fixture_transport!(ScriptedNativeTransport);

fn native_error(thinking_type: &str) -> ProviderResponse {
    ProviderResponse::json(
        400,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": format!("thinking.type: {thinking_type} is not supported")
            }
        }),
    )
}

fn native_success() -> ProviderResponse {
    ProviderResponse::json(
        200,
        serde_json::json!({
            "id": "msg_test",
            "model": "claude-opus-4-6",
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 5, "output_tokens": 2}
        }),
    )
}

fn service(transport: Arc<ScriptedNativeTransport>) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: crate::Region::all(),
                provider_id: crate::ProviderId::AnthropicFirstParty,
                profile_name: "native".into(),
                base_url: "https://api.anthropic.com".into(),
                protocol: crate::ProtocolFamily::AnthropicMessages,
                auth: AuthStrategy::None,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: "claude-opus-4-6".into(),
                    request_model: "claude-opus-4-6".into(),
                    billing_model: "claude-opus-4-6".into(),
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
        .expect("test Native client configuration"),
    );
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".into()),
            entrypoint: Some("cli".into()),
            ..Default::default()
        },
        "test",
        None,
        None,
    )
}

fn main_request() -> LlmRequest {
    let mut request = LlmRequest::new("claude-opus-4-6").with_user_text("hello");
    request.stream = false;
    request.input.max_tokens = Some(4096);
    request.execution.query_source = Some("sdk".into());
    request.execution.anthropic_request_kind =
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main;
    request.execution.native_thinking_state_snapshot =
        Some(crate::model::thinking_state::ThinkingState::new(
            crate::model::thinking::ThinkingConfig::Adaptive,
        ));
    request
}

fn set_type_override(
    service: &ApiService,
    request: &LlmRequest,
    target: NativeThinkingTypeRejection,
) {
    let key = ApiService::native_thinking_type_key(
        request,
        &crate::ProviderId::AnthropicFirstParty,
        crate::ProtocolFamily::AnthropicMessages,
        "native",
    );
    service
        .native_thinking_type_overrides
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key, target);
}

fn request_thinking_type(request: &ProviderRequest) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(request.body_json.as_json_source()).ok()?;
    body.pointer("/thinking/type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn retry_control(max_retries: u32) -> crate::model::retry::RetryControl {
    crate::model::retry::RetryControl {
        max_retries,
        primary_model: "claude-opus-4-6".into(),
        ..Default::default()
    }
}

#[test]
fn generic_thinking_getter_resolves_overlay_and_rejects_unrepresentable_values() {
    let service = service(ScriptedNativeTransport::new(vec![native_success()]));
    assert_eq!(
        service.thinking(),
        Ok(crate::model::thinking::ThinkingConfig::Adaptive)
    );

    service.set_max_thinking_tokens_override(Some(
        crate::model::thinking_state::NativeIntegerNumber::new(0.0).unwrap(),
    ));
    assert_eq!(
        service.thinking(),
        Ok(crate::model::thinking::ThinkingConfig::Disabled)
    );

    service.set_max_thinking_tokens_override(Some(
        crate::model::thinking_state::NativeIntegerNumber::new(4_096.0).unwrap(),
    ));
    assert_eq!(
        service.thinking(),
        Ok(crate::model::thinking::ThinkingConfig::Enabled {
            budget_tokens: 4_096
        })
    );

    for invalid in [-1.0, f64::from(u32::MAX) + 1.0] {
        service.set_max_thinking_tokens_override(Some(
            crate::model::thinking_state::NativeIntegerNumber::new(invalid).unwrap(),
        ));
        assert!(matches!(
            service.thinking(),
            Err(LlmError::InvalidRequest { .. })
        ));
    }
}

#[test]
fn native_main_route_uses_lsn_before_generic_overlay_validation() {
    let service = service(ScriptedNativeTransport::new(vec![native_success()]));
    let mut state = crate::model::thinking_state::ThinkingState::default();
    state.set_max_thinking_tokens_override(Some(
        crate::model::thinking_state::NativeIntegerNumber::new(-9.0).unwrap(),
    ));

    assert_eq!(
        service.main_request_thinking_config(
            "claude-opus-4-6",
            Some("native"),
            crate::PromptCacheQuerySource::Named("sdk"),
            state,
        ),
        Ok(crate::model::thinking::ThinkingConfig::Adaptive)
    );
    assert!(matches!(
        service.main_request_thinking_config(
            "claude-opus-4-6",
            Some("native"),
            crate::PromptCacheQuerySource::Named("agent:child"),
            state,
        ),
        Err(LlmError::InvalidRequest { .. })
    ));
}

#[tokio::test]
async fn native_type_retry_is_one_shot_outside_zero_and_one_retry_budgets() {
    // The matched typed 400 gets one dedicated repair even when no ordinary
    // retries remain; it still updates the service-lifetime model latch.
    let zero_transport = ScriptedNativeTransport::new(vec![
        native_error("adaptive"),
        native_success(),
        native_success(),
    ]);
    let zero_service = service(zero_transport.clone());
    let initial = main_request();
    set_type_override(
        &zero_service,
        &initial,
        NativeThinkingTypeRejection::Adaptive,
    );
    zero_service
        .drive_non_stream(
            initial.clone(),
            retry_control(0),
            DispatchHeaderState::default(),
        )
        .await
        .expect("zero ordinary retries still allow the dedicated typed repair");
    let first_call_requests = zero_transport.requests();
    assert_eq!(first_call_requests.len(), 2);
    assert_eq!(
        request_thinking_type(&first_call_requests[0]).as_deref(),
        Some("adaptive")
    );
    assert_eq!(
        request_thinking_type(&first_call_requests[1]).as_deref(),
        Some("enabled")
    );

    zero_service
        .drive_non_stream(
            main_request(),
            retry_control(0),
            DispatchHeaderState::default(),
        )
        .await
        .expect("the next logical call uses the latched enabled type");
    let zero_requests = zero_transport.requests();
    assert_eq!(zero_requests.len(), 3);
    assert_eq!(
        request_thinking_type(&zero_requests[2]).as_deref(),
        Some("enabled")
    );

    // The dedicated repair is also independent of a one-retry ordinary
    // budget. A second typed error updates the latch, but the one-shot gate
    // prevents a third request in this logical call.
    let one_transport = ScriptedNativeTransport::new(vec![
        native_error("adaptive"),
        native_error("enabled"),
        native_success(),
    ]);
    let one_service = service(one_transport.clone());
    let initial = main_request();
    set_type_override(
        &one_service,
        &initial,
        NativeThinkingTypeRejection::Adaptive,
    );
    let error = one_service
        .drive_non_stream(initial, retry_control(1), DispatchHeaderState::default())
        .await
        .expect_err("the second 400 must surface after two actual attempts");
    assert_eq!(ApiService::status_of(&error), Some(400));
    let first_call_requests = one_transport.requests();
    assert_eq!(first_call_requests.len(), 2);
    assert_eq!(
        request_thinking_type(&first_call_requests[0]).as_deref(),
        Some("adaptive")
    );
    assert_eq!(
        request_thinking_type(&first_call_requests[1]).as_deref(),
        Some("enabled")
    );

    one_service
        .drive_non_stream(
            main_request(),
            retry_control(0),
            DispatchHeaderState::default(),
        )
        .await
        .expect("the repeated type error updated the latch to adaptive");
    let requests = one_transport.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        request_thinking_type(&requests[2]).as_deref(),
        Some("adaptive")
    );
}
