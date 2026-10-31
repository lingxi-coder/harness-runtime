//! End-to-end Native thinking retry and output-budget provenance regressions.

use super::*;
use crate::test_support::FixtureTransport;
use crate::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, CredentialConfig, LlmError, ModelProfile,
    PricingConfig, ProviderProfile, ProviderRequest, ProviderResponse, RawStreamFrame,
    StreamingResponse,
};
use futures::StreamExt;
use lingxi_llm_client::protocol::NativeThinkingTypeRejection;
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

struct StreamPlan {
    status: u16,
    headers: BTreeMap<String, String>,
    frames: Vec<Vec<u8>>,
}

struct NativeDispatchTransport {
    requests: Mutex<Vec<ProviderRequest>>,
    execute_responses: Mutex<VecDeque<ProviderResponse>>,
    stream_responses: Mutex<VecDeque<StreamPlan>>,
}

impl NativeDispatchTransport {
    fn new(
        execute_responses: Vec<ProviderResponse>,
        stream_responses: Vec<StreamPlan>,
    ) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            execute_responses: Mutex::new(execute_responses.into()),
            stream_responses: Mutex::new(stream_responses.into()),
        })
    }

    fn requests(&self) -> Vec<ProviderRequest> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

struct ScriptedFrames(VecDeque<Vec<u8>>);

impl crate::FrameStream for ScriptedFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let frame = self.0.pop_front().map(RawStreamFrame::new);
        Box::pin(async move { Ok(frame) })
    }
}

impl FixtureTransport for NativeDispatchTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let response = self
            .execute_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| LlmError::Transport {
                message: "no non-stream response was scripted".into(),
            });
        Box::pin(async move { response })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        let response = self
            .stream_responses
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front()
            .ok_or_else(|| LlmError::Transport {
                message: "no streaming response was scripted".into(),
            })
            .map(|plan| StreamingResponse {
                status: plan.status,
                headers: plan.headers,
                frames: Box::new(ScriptedFrames(plan.frames.into())),
            });
        Box::pin(async move { response })
    }
}

llm_runtime::impl_fixture_transport!(NativeDispatchTransport);

fn native_service(transport: Arc<NativeDispatchTransport>) -> ApiService {
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
        .expect("Native test client configuration"),
    );
    let mut service = ApiService::new(
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
    );
    service.settings_max_retries = Some(1);
    service
}

fn main_request(
    max_tokens: u32,
    thinking: crate::model::thinking_state::ThinkingState,
) -> LlmRequest {
    let mut request = LlmRequest::new("claude-opus-4-6").with_user_text("hello");
    request.stream = false;
    request.input.max_tokens = Some(max_tokens);
    request.execution.query_source = Some("sdk".into());
    request.execution.anthropic_request_kind =
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main;
    request.execution.native_thinking_state_snapshot = Some(thinking);
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

fn request_body(request: &ProviderRequest) -> serde_json::Value {
    serde_json::from_str(request.body_json.as_json_source()).expect("request JSON")
}

fn request_thinking_type(request: &ProviderRequest) -> Option<String> {
    request_body(request)
        .pointer("/thinking/type")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
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

fn native_type_rejection(kind: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("thinking.type: {kind} is not supported")
        }
    })
}

fn anthropic_success_frames() -> Vec<Vec<u8>> {
    vec![
        br#"{"type":"message_start","message":{"id":"msg_stream","model":"claude-opus-4-6","usage":{"input_tokens":1,"output_tokens":0}}}"#.to_vec(),
        br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
        br#"{"type":"message_stop"}"#.to_vec(),
    ]
}

fn retry_control(max_retries: u32) -> crate::model::retry::RetryControl {
    crate::model::retry::RetryControl {
        max_retries,
        primary_model: "claude-opus-4-6".into(),
        ..Default::default()
    }
}

fn scope_quota(dispatches: u32) -> Arc<crate::ScopeOutputQuota> {
    Arc::new(crate::ScopeOutputQuota::new(
        std::num::NonZeroU32::new(4096).unwrap(),
        std::num::NonZeroU32::new(dispatches).unwrap(),
    ))
}

fn scope_request() -> LlmRequest {
    main_request(
        u32::MAX,
        crate::model::thinking_state::ThinkingState::new(
            crate::model::thinking::ThinkingConfig::Adaptive,
        ),
    )
}

#[tokio::test]
async fn scope_quota_counts_retry_dispatches_and_clamps_explicit_output() {
    let transport = NativeDispatchTransport::new(
        vec![
            ProviderResponse::json(
                529,
                serde_json::json!({
                    "type": "error", "error": {"type": "overloaded_error", "message": "busy"}
                }),
            ),
            native_success(),
        ],
        Vec::new(),
    );
    let quota = scope_quota(1);
    let mut service =
        native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    service.settings_backoff_ms = Some(0);
    let error = service
        .drive_non_stream(
            scope_request(),
            retry_control(3),
            DispatchHeaderState::default(),
        )
        .await
        .expect_err("retry must not bypass scope quota");
    assert!(matches!(error, LlmError::ApiAttemptsExhausted));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(request_body(&requests[0])["max_tokens"], 4096);
    assert_eq!(quota.remaining_dispatches(), 0);
}

#[tokio::test]
async fn scope_quota_is_shared_by_main_auxiliary_and_forked_services() {
    let transport =
        NativeDispatchTransport::new(vec![native_success(), native_success()], Vec::new());
    let quota = scope_quota(2);
    let service = native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    service
        .drive_non_stream(
            scope_request(),
            retry_control(0),
            DispatchHeaderState::default(),
        )
        .await
        .unwrap();
    service
        .fork_independent()
        .execute_side_query_request(scope_request())
        .await
        .unwrap();
    let error = service
        .fork_app_invocation()
        .execute_classifier_request(scope_request(), 3)
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::ApiAttemptsExhausted));
    assert_eq!(quota.remaining_dispatches(), 0);
    assert_eq!(transport.requests().len(), 2);
    for request in transport.requests() {
        assert_eq!(request_body(&request)["max_tokens"], 4096);
    }
}

#[tokio::test]
async fn scope_quota_failed_prepare_does_not_charge() {
    let transport = NativeDispatchTransport::new(Vec::new(), Vec::new());
    let quota = scope_quota(1);
    let service = native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    let mut request = scope_request();
    request.input.model = "missing-model".into();
    assert!(service.execute_side_query_request(request).await.is_err());
    assert_eq!(quota.remaining_dispatches(), 1);
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn scope_quota_preserves_the_separate_operation_budget_and_rolls_back() {
    let transport = NativeDispatchTransport::new(vec![native_success()], Vec::new());
    let quota = scope_quota(1);
    let service = native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    let mut request = scope_request();
    request.execution.api_attempts_left = Some(crate::ApiAttemptsLeft::new(0));
    assert!(matches!(
        service.execute_side_query_request(request).await,
        Err(LlmError::ApiAttemptsExhausted)
    ));
    assert_eq!(quota.remaining_dispatches(), 1);
    service
        .execute_side_query_request(scope_request())
        .await
        .unwrap();
    let operation = crate::ApiAttemptsLeft::new(3);
    let mut request = scope_request();
    request.execution.api_attempts_left = Some(operation.clone());
    assert!(matches!(
        service.execute_side_query_request(request).await,
        Err(LlmError::ApiAttemptsExhausted)
    ));
    assert_eq!(
        operation.remaining(),
        3,
        "failed scope reservation refunds operation reservation"
    );
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn scope_quota_revocation_at_late_admission_stops_transport_without_charge() {
    let transport = NativeDispatchTransport::new(Vec::new(), Vec::new());
    let quota = scope_quota(1);
    let service = native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    let mut request = scope_request();
    let revoke = quota.clone();
    request.execution.request_dispatch_admission =
        Some(crate::RequestDispatchAdmission::new(move || {
            revoke.revoke();
            true
        }));
    assert!(matches!(
        service.execute_side_query_request(request).await,
        Err(LlmError::RequestDispatchRejected {
            prior_dispatch: false
        })
    ));
    assert!(quota.is_revoked());
    assert_eq!(quota.remaining_dispatches(), 1);
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn scope_quota_stream_retry_cannot_reset_the_counter() {
    let transport = NativeDispatchTransport::new(
        Vec::new(),
        vec![
            StreamPlan {
                status: 400,
                headers: BTreeMap::new(),
                frames: vec![serde_json::to_vec(&native_type_rejection("adaptive")).unwrap()],
            },
            StreamPlan {
                status: 200,
                headers: BTreeMap::new(),
                frames: anthropic_success_frames(),
            },
        ],
    );
    let quota = scope_quota(1);
    let service = native_service(transport.clone()).with_scope_output_quota(Some(quota.clone()));
    let error = match service.stream_request(scope_request()).await {
        Err(error) => error,
        Ok(mut stream) => loop {
            match stream.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => {}
                None => panic!("expected exhausted retry"),
            }
        },
    };
    assert!(matches!(error, LlmError::ApiAttemptsExhausted));
    assert_eq!(quota.remaining_dispatches(), 0);
    assert_eq!(transport.requests().len(), 1);
    assert_eq!(request_body(&transport.requests()[0])["max_tokens"], 4096);
}

#[tokio::test]
async fn streaming_typed_thinking_400_retries_once_with_zero_ordinary_retries() {
    let transport = NativeDispatchTransport::new(
        Vec::new(),
        vec![
            StreamPlan {
                status: 400,
                headers: BTreeMap::new(),
                frames: vec![serde_json::to_vec(&native_type_rejection("adaptive")).unwrap()],
            },
            StreamPlan {
                status: 200,
                headers: BTreeMap::new(),
                frames: anthropic_success_frames(),
            },
        ],
    );
    let mut service = native_service(transport.clone());
    service.settings_max_retries = Some(0);
    let request = main_request(
        4096,
        crate::model::thinking_state::ThinkingState::new(
            crate::model::thinking::ThinkingConfig::Adaptive,
        ),
    );
    set_type_override(&service, &request, NativeThinkingTypeRejection::Adaptive);

    let mut stream = service
        .stream_request(request)
        .await
        .expect("the typed 400 should take the dedicated retry");
    while let Some(event) = stream.next().await {
        event.expect("the retried Anthropic stream should decode");
    }

    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        2,
        "one physical retry follows the typed 400"
    );
    assert_eq!(
        request_thinking_type(&requests[0]).as_deref(),
        Some("adaptive")
    );
    assert_eq!(
        request_thinking_type(&requests[1]).as_deref(),
        Some("enabled")
    );
}

#[tokio::test]
async fn streaming_dispatch_spends_one_operation_api_attempt() {
    let transport = NativeDispatchTransport::new(
        Vec::new(),
        vec![StreamPlan {
            status: 200,
            headers: BTreeMap::new(),
            frames: anthropic_success_frames(),
        }],
    );
    let service = native_service(transport.clone());
    let budget = crate::ApiAttemptsLeft::new(1);
    let mut request = main_request(
        4096,
        crate::model::thinking_state::ThinkingState::new(
            crate::model::thinking::ThinkingConfig::Adaptive,
        ),
    );
    request.stream = true;
    request.execution.api_attempts_left = Some(budget.clone());

    let mut stream = service.stream_request(request).await.expect("stream opens");
    while let Some(event) = stream.next().await {
        event.expect("successful stream event");
    }

    assert_eq!(transport.requests().len(), 1);
    assert_eq!(budget.remaining(), 0);
}

#[tokio::test]
async fn exhausted_stream_budget_stops_before_transport_with_typed_error() {
    let transport = NativeDispatchTransport::new(
        Vec::new(),
        vec![StreamPlan {
            status: 200,
            headers: BTreeMap::new(),
            frames: anthropic_success_frames(),
        }],
    );
    let mut service = native_service(transport.clone());
    service.settings_max_retries = Some(0);
    let budget = crate::ApiAttemptsLeft::new(0);
    let mut request = main_request(
        4096,
        crate::model::thinking_state::ThinkingState::new(
            crate::model::thinking::ThinkingConfig::Adaptive,
        ),
    );
    request.stream = true;
    request.execution.api_attempts_left = Some(budget.clone());

    let error = match service.stream_request(request).await {
        Err(error) => error,
        Ok(mut stream) => loop {
            match stream.next().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => {}
                None => panic!("exhausted stream budget ended without an error"),
            }
        },
    };

    assert!(matches!(error, LlmError::ApiAttemptsExhausted));
    assert_eq!(error.to_string(), "Not sent: no API attempts left");
    assert_eq!(ApiService::status_of(&error), None);
    assert_eq!(transport.requests().len(), 0);
    assert_eq!(budget.remaining(), 0);
}

#[tokio::test]
async fn native_manual_floor_carries_resolved_output_provenance_to_the_sdk() {
    let transport = NativeDispatchTransport::new(vec![native_success()], Vec::new());
    let service = native_service(transport.clone());
    let mut thinking = crate::model::thinking_state::ThinkingState::new(
        crate::model::thinking::ThinkingConfig::Adaptive,
    );
    thinking.set_max_thinking_tokens_override(Some(
        crate::model::thinking_state::NativeIntegerNumber::new(-7.0).unwrap(),
    ));
    let request = main_request(512, thinking);
    set_type_override(&service, &request, NativeThinkingTypeRejection::Enabled);

    service
        .drive_non_stream(request, retry_control(0), DispatchHeaderState::default())
        .await
        .expect("the SDK should accept the exact Lsn floor provenance");

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    let body = request_body(&requests[0]);
    assert_eq!(
        body.get("max_tokens").and_then(serde_json::Value::as_u64),
        Some(512)
    );
    assert_eq!(
        body.pointer("/thinking/budget_tokens")
            .and_then(serde_json::Value::as_u64),
        Some(1024),
        "the Native manual minimum remains 1024 even below the output cap"
    );
}
