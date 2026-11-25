//! Shared fixtures for the service regression tests.

mod accounting_test;
mod classifier_test;
mod context_test;
mod count_test;
mod dispatch_test;
mod native_test;
mod rate_limit_test;
mod recovery_test;
mod request_test;
mod retry_test;
mod stream_test;

use super::*;
use lingxi_llm_client::protocol::PromptText;
use lingxi_llm_client::providers::anthropic::error_recognition::{
    foundry_unsupported_feature_names, oauth_token_revoked,
};

fn context_policy_fixture(input_tokens: u64) -> (ApiService, Arc<FakeTransport>) {
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "openai")
        .unwrap();
    provider.profile_name = "context-policy-test".into();
    provider.provider_id = crate::ProviderId::Custom {
        name: "context-policy-test".into(),
    };
    provider.base_url = "https://example.test".into();
    provider.wire_profile = None;
    provider.connection = Default::default();
    provider.auth = crate::AuthStrategy::None;
    provider.credential = crate::CredentialConfig::None;
    provider.models.truncate(1);
    let model = &mut provider.models[0];
    model.request_model = "m".into();
    model.display_model = "m".into();
    model.billing_model = "m".into();
    model.aliases.clear();
    model.metadata = lingxi_core::host::ModelMetadata {
        context_window_tokens: Some(1000000),
        context: Some(lingxi_core::host::ContextMetadata {
            application: Some(lingxi_core::host::ContextPolicy::Custom { input_tokens }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let client = crate::ModelRuntime::from_config(crate::ClientConfig {
        providers: vec![provider],
    })
    .unwrap();
    let transport = FakeTransport::always(ProviderResponse::json(
        200,
        serde_json::json!({"choices":[]}),
    ));
    let service = ApiService::new(
        Arc::new(client),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
    .with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    (service, transport)
}

async fn pooled_context_usage_survives_physical_account_selection(streaming: bool) {
    use crate::context_usage::{ContextUsageEstimator, ContextUsagePrefix};
    use lingxi_core::types::{ContentBlock, MessageId};
    let (seed, _) = context_policy_fixture(272_000);
    let mut template = seed.client().configuration().providers.remove(0);
    template.auth = crate::AuthStrategy::ApiKey;
    let runtime = crate::service::live_web_tests::pooled_runtime(
        template,
        Arc::new(crate::service::live_web_tests::ApiKeyCredentials),
    );
    let response_json = serde_json::json!({
        "id":"measured", "object":"response", "model":"m", "status":"completed",
        "output":[{"type":"message","id":"answer","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"ok","annotations":[]}]}],
        "usage":{"input_tokens":127_965,"output_tokens":2,"total_tokens":127_967}
    });
    let batched = FakeTransport::always(ProviderResponse::json(200, response_json.clone()));
    let streamed = FakeStreamTransport::sequence((0..2).map(|_| FakeStreamResp::Frames {
        status: 200, headers: Default::default(), frames: vec![
            serde_json::to_vec(&serde_json::json!({"type":"response.created","response":{"id":"measured","model":"m","output":[]}})).unwrap(),
            serde_json::to_vec(&serde_json::json!({"type":"response.completed","response":response_json})).unwrap(),
        ],
    }).collect());
    let transport: Arc<dyn Transport> = if streaming {
        streamed.clone()
    } else {
        batched.clone()
    };
    let service = ApiService::new(
        Arc::new(runtime),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "pooled-context",
        None,
        None,
    )
    .with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let initial = vec![ConversationMessage::user(MessageId::new(), "hello".into())];
    let make = |history, profile: &str, system: &str| {
        MessagesCreateRequest::new(
            "m",
            Some(profile),
            Some(crate::SystemPromptInput::custom_prompt(
                PromptText::from_string(system),
            )),
            history,
            Vec::new(),
        )
    };
    let first_request = make(initial.clone(), "context-policy-test", "stable system");
    let first = if streaming {
        service.messages_create_buffered_stream(first_request).await
    } else {
        service.messages_create(first_request).await
    }
    .unwrap();
    let prefix: ContextUsagePrefix = serde_json::from_str(
        first
            .usage
            .provider_metadata
            .payload_at("/lingxi_context_usage_prefix")
            .expect("physical response must retain its measured prompt identity")
            .as_json_source(),
    )
    .unwrap();
    assert_eq!(first.usage.context_tokens(), Some(127_967));
    // Exercise the serialized replay boundary, including opaque provider
    // blocks whose bytes are not proportional to measured occupancy.
    let mut replay = initial;
    replay.push(ConversationMessage::Assistant {
        id: MessageId::new(),
        content: vec![ContentBlock::ProviderContent {
            protocol: "open_ai_responses".into(),
            value: lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(
                &serde_json::json!({"type":"reasoning","encrypted_content":"x".repeat(900_000)}),
            )
            .unwrap(),
        }],
        stop_reason: Some("end_turn".into()),
        per_turn_effort: None,
        resumable: None,
    });
    replay.push(ConversationMessage::user(
        MessageId::new(),
        "continue".into(),
    ));
    let replay: Vec<ConversationMessage> =
        serde_json::from_slice(&serde_json::to_vec(&replay).unwrap()).unwrap();
    let prefix: ContextUsagePrefix =
        serde_json::from_slice(&serde_json::to_vec(&prefix).unwrap()).unwrap();
    let estimator = ContextUsageEstimator::with_prefix(move |_, _, history| {
        prefix
            .matches_history(history)
            .then(|| (127_969, prefix.clone()))
    });
    let second_request = make(replay.clone(), "context-policy-test", "stable system");
    let second = estimator
        .scope(async {
            if streaming {
                service
                    .messages_create_buffered_stream(second_request)
                    .await
            } else {
                service.messages_create(second_request).await
            }
        })
        .await
        .unwrap();
    assert_eq!(second.usage.context_tokens(), Some(127_967));
    for (profile, system) in [
        ("context-policy-test", "changed system"),
        ("account.b", "stable system"),
    ] {
        let request = make(replay.clone(), profile, system);
        let rejected = estimator
            .scope(async {
                if streaming {
                    service.messages_create_buffered_stream(request).await
                } else {
                    service.messages_create(request).await
                }
            })
            .await;
        assert!(
            matches!(rejected, Err(LlmError::InputBudgetExceeded { .. })),
            "changed physical prompt/account must not inherit measured occupancy: {rejected:?}"
        );
    }
    if streaming {
        assert_eq!(streamed.stream_call_count(), 2);
        for index in 0..2 {
            assert_eq!(
                streamed
                    .seen_headers(index)
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.as_str()),
                Some("Bearer synthetic-acct.a")
            );
        }
    } else {
        assert_eq!(batched.seen_count(), 2);
        for index in 0..2 {
            assert_eq!(
                batched
                    .seen_headers(index)
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                    .map(|(_, value)| value.as_str()),
                Some("Bearer synthetic-acct.a")
            );
        }
    }
}

fn two_context_beta_connections(transport: Arc<dyn Transport>, model: &str) -> ApiService {
    let seed = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://a.bedrock.example.test",
        "connection-a",
        model,
        transport.clone(),
    );
    let mut config = seed.client().configuration();
    let mut second = config.providers[0].clone();
    second.profile_name = "connection-b".into();
    second.base_url = "https://b.bedrock.example.test".into();
    config.providers.push(second);
    ApiService::new(
        Arc::new(ModelRuntime::from_config(config).unwrap()),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
}

fn context_policy_count_fixture(
    first: FakeResponse,
) -> (ApiService, Arc<FakeTransport>, LlmRequest) {
    let mut provider = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|p| p.profile_name == "anthropic")
        .unwrap();
    provider.profile_name = "context-count-test".into();
    provider.provider_id = crate::ProviderId::Custom {
        name: "context-count-test".into(),
    };
    provider.base_url = "https://example.test".into();
    provider.wire_profile = None;
    provider.connection = Default::default();
    provider.auth = crate::AuthStrategy::None;
    provider.credential = crate::CredentialConfig::None;
    provider.models.truncate(1);
    let model = &mut provider.models[0];
    model.request_model = "m".into();
    model.display_model = "m".into();
    model.billing_model = "m".into();
    model.aliases.clear();
    model.metadata = lingxi_core::host::ModelMetadata {
        context_window_tokens: Some(8192),
        max_output_tokens: Some(1024),
        context: Some(lingxi_core::host::ContextMetadata {
            shared_window: Some(true),
            application: Some(lingxi_core::host::ContextPolicy::Long),
            ..Default::default()
        }),
        ..Default::default()
    };
    let client = crate::ModelRuntime::from_config(crate::ClientConfig {
        providers: vec![provider],
    })
    .unwrap();
    let transport = FakeTransport::sequence(vec![
        first,
        FakeResponse::Ok(ProviderResponse::json(200, ok_response_json())),
    ]);
    let service = ApiService::new(
        Arc::new(client),
        transport.clone(),
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
    .with_thinking(crate::model::thinking::ThinkingConfig::Disabled);
    let mut request = LlmRequest::new("m").with_user_text("x".repeat(22000));
    request.profile = Some("context-count-test".into());
    request.input.max_tokens = Some(512);
    // This fixture tests endpoint availability with an explicit output
    // budget. Concurrent extra-body tests must not replace its input or
    // generation controls while the count operation awaits its response.
    request.execution.anthropic_extra_body = Some(
        crate::anthropic_extra_body::CapturedAnthropicExtraBody::new(
            None,
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main,
        )
        .unwrap(),
    );
    (service, transport, request)
}

// Ordinary structural assertions may use serde_json as a test projection;
// source and exact-byte assertions below stay on JsonPayload/sealed bytes.
fn request_body_value(body: &lingxi_llm_client::protocol::JsonPayload) -> serde_json::Value {
    serde_json::from_str(body.as_json_source()).expect("request body is valid JSON")
}

fn provider_payload(value: serde_json::Value) -> lingxi_llm_client::protocol::JsonPayload {
    lingxi_llm_client::protocol::JsonPayload::from_generated_value(&value)
        .expect("test provider response is valid JSON")
}

fn context_1m_refusal_response(message: &str) -> FakeResponse {
    FakeResponse::Ok(ProviderResponse::json(
        400,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": message
            }
        }),
    ))
}

fn request_header_has_beta(headers: &BTreeMap<String, String>, beta: &str) -> bool {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("anthropic-beta"))
        .is_some_and(|(_, value)| value.split(',').map(str::trim).any(|item| item == beta))
}

fn request_body_has_beta(body: &lingxi_llm_client::protocol::JsonPayload, beta: &str) -> bool {
    request_body_value(body)
        .get("anthropic_beta")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(beta)))
}

fn classified_error<T>(result: &Result<T, LlmError>) -> Option<&LlmError> {
    result.as_ref().err().map(LlmError::classification_source)
}

fn core_projection(
    value: &serde_json::Value,
) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
    lingxi_core::types::utf16_json::Utf16JsonProjection::from_serde_value(value)
        .expect("generated test fixture is valid JSON")
}

fn source_tool_declaration(name: &str, description: &str, schema: &str) -> crate::ToolDeclaration {
    crate::convert::to_tool_declarations(vec![lingxi_core::host::SourceToolWireDefinition {
        name: name.to_owned(),
        description: lingxi_core::types::utf16_json::Utf16JsonProjection::from(description),
        input_schema: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(schema).unwrap(),
        strict: false,
        defer_loading: false,
    }])
    .unwrap()
    .pop()
    .unwrap()
}

fn prompt_source_elements(request: &LlmRequest) -> Vec<&PromptText> {
    use lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput;

    let system = request
        .execution
        .prompt_cache
        .as_ref()
        .and_then(|context| context.system.as_ref())
        .expect("the source system prompt is carried to SDK projection");
    match system {
        SystemPromptInput::SourceVector {
            elements,
            dynamic_context,
            ..
        } => {
            let mut result = elements.iter().collect::<Vec<_>>();
            if let Some(dynamic_context) = dynamic_context.as_ref().filter(|text| !text.is_empty())
            {
                result.push(dynamic_context);
            }
            result
        }
        SystemPromptInput::CustomPrompt { text } => vec![text],
        SystemPromptInput::NativeCustomPrompt {
            source_elements, ..
        } => source_elements.iter().collect(),
    }
}

use crate::model::retry::DEFAULT_MAX_RETRIES;
use crate::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, Credential, CredentialConfig,
    CredentialProvider, CredentialScope, LlmError, ModelProfile, PricingConfig, ProtocolFamily,
    ProviderId, ProviderProfile, ProviderRequest, ProviderResponse, StreamingResponse,
};
use std::collections::BTreeMap;
use std::sync::{atomic::AtomicUsize, Mutex};

// These SDK projection fixtures exercise Native's exact brand-member
// classifier. An arbitrary opaque header is an ordinary source member,
// so use the current Native identity while testing its block/cache shape.
const HEADER: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const SECTION_SEP: &str = "\n\n";

// ── FakeTransport ─────────────────────────────────────────────────────────

/// A fake Transport that returns a scripted sequence of responses (or errors).
struct FakeTransport {
    /// Pre-recorded responses returned in order; cycles to last entry.
    responses: Mutex<Vec<FakeResponse>>,
    /// All requests received, in order.
    seen: Mutex<Vec<ProviderRequest>>,
}

#[derive(Clone)]
#[allow(dead_code)]
enum FakeResponse {
    Ok(ProviderResponse),
    ResetScopeBeforeReply {
        scope: crate::thinking_scope::ThinkingRecoveryScope,
        response: ProviderResponse,
    },
    Err(LlmError),
}

impl FakeTransport {
    /// Return the same response on every call.
    fn always(resp: ProviderResponse) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(vec![FakeResponse::Ok(resp)]),
            seen: Mutex::new(vec![]),
        })
    }

    /// Return each response in sequence; the last is repeated forever.
    fn sequence(resps: Vec<FakeResponse>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(resps),
            seen: Mutex::new(vec![]),
        })
    }

    fn seen_count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn seen_headers(&self, idx: usize) -> BTreeMap<String, String> {
        self.seen.lock().unwrap()[idx].headers.clone()
    }

    fn seen_body(&self, idx: usize) -> lingxi_llm_client::protocol::JsonPayload {
        self.seen.lock().unwrap()[idx].body_json.clone()
    }

    /// Return the `"model"` field from the JSON body of the `idx`-th request.
    /// Useful for asserting chain-walk model sequences.
    fn seen_body_model(&self, idx: usize) -> Option<String> {
        request_body_value(&self.seen.lock().unwrap()[idx].body_json)
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }
}

#[derive(Debug)]
struct RotatingAnthropicOAuthProvider {
    token: Mutex<String>,
    refreshes: AtomicUsize,
}

impl RotatingAnthropicOAuthProvider {
    fn new() -> Self {
        Self {
            token: Mutex::new("rejected-token".into()),
            refreshes: AtomicUsize::new(0),
        }
    }
}

impl CredentialProvider for RotatingAnthropicOAuthProvider {
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            Ok(Credential::AnthropicOAuth {
                access_token: self.token.lock().unwrap().clone(),
                scopes: vec!["user:inference".into()],
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            let Credential::AnthropicOAuth {
                access_token,
                scopes,
            } = rejected
            else {
                return Ok(None);
            };
            if access_token != "rejected-token" {
                return Ok(None);
            }
            self.refreshes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.token.lock().unwrap() = "refreshed-token".into();
            Ok(Some(Credential::AnthropicOAuth {
                access_token: "refreshed-token".into(),
                scopes: scopes.clone(),
            }))
        })
    }
}

impl llm_runtime::test_support::FixtureTransport for FakeTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let mut seen = self.seen.lock().unwrap();
        seen.push(request.clone());
        let idx = (seen.len() - 1).min({
            let resps = self.responses.lock().unwrap();
            resps.len().saturating_sub(1)
        });
        let resp = {
            let resps = self.responses.lock().unwrap();
            resps[idx].clone()
        };
        Box::pin(async move {
            match resp {
                FakeResponse::Ok(r) => Ok(r),
                FakeResponse::ResetScopeBeforeReply { scope, response } => {
                    scope.reset_beta_rejections();
                    Ok(response)
                }
                FakeResponse::Err(e) => Err(e),
            }
        })
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

// ── Test helpers ──────────────────────────────────────────────────────────

#[derive(Default)]
struct AttemptProbe {
    events: Mutex<Vec<&'static str>>,
    observations: Mutex<Vec<(crate::ExecutionUsage, crate::ModelAttemptUsageCompleteness)>>,
    fail_settlement: bool,
    admission_delay: Duration,
}

struct ProbeLease {
    probe: Arc<AttemptProbe>,
    finished: bool,
}

impl Drop for ProbeLease {
    fn drop(&mut self) {
        if !self.finished {
            self.probe.events.lock().unwrap().push("drop-owned");
        }
    }
}

struct ProbeSettlement(Arc<AttemptProbe>);

#[async_trait::async_trait]
impl crate::ModelAttemptHooks for Arc<AttemptProbe> {
    async fn begin(
        &self,
        _: &lingxi_core::host::ModelAttemptContext,
        request: &LlmRequest,
        prepared: &crate::PreparedLlmCall,
    ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
        assert!(request.execution.model_attempt.is_some());
        if !self.admission_delay.is_zero() {
            tokio::time::sleep(self.admission_delay).await;
        }
        let _ = prepared;
        self.events.lock().unwrap().push("begin");
        Ok(Box::new(ProbeLease {
            probe: self.clone(),
            finished: false,
        }))
    }
}

impl crate::ModelAttemptLease for ProbeLease {
    fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        self.probe.events.lock().unwrap().push("dispatch");
        Ok(())
    }
    fn observe_usage(
        &mut self,
        usage: &crate::ExecutionUsage,
        completeness: crate::ModelAttemptUsageCompleteness,
    ) {
        self.probe
            .observations
            .lock()
            .unwrap()
            .push((usage.clone(), completeness));
    }
    fn mark_no_provider_response(&mut self) {
        self.probe
            .events
            .lock()
            .unwrap()
            .push("no-provider-response");
    }
    fn finish(mut self: Box<Self>) -> Box<dyn crate::ModelAttemptSettlement> {
        self.finished = true;
        self.probe.events.lock().unwrap().push("finish-owned");
        Box::new(ProbeSettlement(self.probe.clone()))
    }
}

#[async_trait::async_trait]
impl crate::ModelAttemptSettlement for ProbeSettlement {
    async fn wait(self: Box<Self>) -> Result<(), LlmError> {
        self.0.events.lock().unwrap().push("settled");
        if self.0.fail_settlement {
            return Err(LlmError::Transport {
                message: "fake persistence failed".into(),
            });
        }
        Ok(())
    }
}

struct IncompleteResponseTransport {
    status: u16,
}
#[async_trait::async_trait]
impl Transport for IncompleteResponseTransport {
    async fn send(
        &self,
        _: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures::StreamExt;
        let body = if self.status >= 400 {
            futures::stream::pending().boxed()
        } else {
            futures::stream::iter(vec![
                Ok(r#"{"id":"msg","content":[{"type":"text","text":"generated"}],"usage":"#.into()),
                Err(lingxi_llm_client::protocol::LlmError::StreamInterrupted {
                    message: "lost body after generation".into(),
                }),
            ])
            .boxed()
        };
        Ok(lingxi_llm_client::StreamResponse {
            status: self.status,
            headers: vec![],
            body,
        })
    }
}

struct StalledNonStreamTransport {
    body: bool,
}

struct ClassifierStreamTransport {
    phase: u8,
}

struct ClassifierRetryTransport {
    failures: Vec<u8>,
    sent: std::sync::atomic::AtomicUsize,
}

fn classifier_stream_chunks(text: &str, complete: bool) -> Vec<bytes::Bytes> {
    let mut events = vec![
        serde_json::json!({"type":"response.created","response":{"id":"r","model":"model","output":[]}}),
        serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"message","id":"m","role":"assistant","status":"in_progress","content":[]}}),
        serde_json::json!({"type":"response.content_part.added","item_id":"m","output_index":0,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}),
        serde_json::json!({"type":"response.output_text.delta","item_id":"m","output_index":0,"content_index":0,"delta":text}),
    ];
    if complete {
        events.push(serde_json::json!({"type":"response.completed","response":{"id":"r","model":"model","status":"completed","output":[{"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}));
    }
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n").into())
        .collect()
}

#[async_trait::async_trait]
impl Transport for ClassifierRetryTransport {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures::StreamExt;
        let index = self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Some(phase) = self.failures.get(index) {
            if matches!(*phase, 5 | 6) {
                return Ok(lingxi_llm_client::StreamResponse {
                    status: 503,
                    headers: vec![
                        ("content-type".into(), "application/json".into()),
                        ("x-should-retry".into(), "false".into()),
                    ],
                    body: if *phase == 6 {
                        futures::stream::pending().boxed()
                    } else {
                        futures::stream::once(async {
                            Ok(r#"{"error":{"message":"explicit retry decline","type":"api_error"}}"#.into())
                        })
                        .boxed()
                    },
                });
            }
            if *phase != 4 {
                return ClassifierStreamTransport { phase: *phase }
                    .send(request)
                    .await;
            }
            return Ok(lingxi_llm_client::StreamResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: futures::stream::iter(
                    classifier_stream_chunks("discard this partial", false)
                        .into_iter()
                        .map(Ok),
                )
                .chain(futures::stream::pending())
                .boxed(),
            });
        }
        Ok(lingxi_llm_client::StreamResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: futures::stream::iter(
                classifier_stream_chunks("<block>no", true)
                    .into_iter()
                    .map(Ok),
            )
            .boxed(),
        })
    }
}

#[async_trait::async_trait]
impl Transport for ClassifierStreamTransport {
    async fn send(
        &self,
        _: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures::StreamExt;
        if self.phase == 0 {
            return std::future::pending().await;
        }
        tokio::time::sleep(Duration::from_millis(3)).await;
        let body = if self.phase == 2 {
            futures::stream::unfold((), |()| async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((Ok(": heartbeat\n\n".into()), ()))
            })
            .boxed()
        } else {
            futures::stream::pending().boxed()
        };
        Ok(lingxi_llm_client::StreamResponse {
            status: if self.phase == 3 { 500 } else { 200 },
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body,
        })
    }
}
#[async_trait::async_trait]
impl Transport for StalledNonStreamTransport {
    async fn send(
        &self,
        _: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures::StreamExt;
        if !self.body {
            return std::future::pending().await;
        }
        Ok(lingxi_llm_client::StreamResponse {
            status: 200,
            headers: vec![],
            body: futures::stream::pending().boxed(),
        })
    }
}

fn registered_request() -> LlmRequest {
    let mut request = LlmRequest::new("claude-sonnet-4-20250514").with_user_text("fake request");
    request.input.max_tokens = Some(100);
    request.execution.model_attempt = Some(
        lingxi_core::host::ModelAttemptRun::new(Arc::new(()))
            .context(lingxi_core::host::ModelAttemptStage::Panel, Some(0))
            .unwrap(),
    );
    request
}

struct HttpOnlyProbeTransport {
    frames: Vec<Vec<u8>>,
    http_calls: std::sync::atomic::AtomicUsize,
    ws_calls: std::sync::atomic::AtomicUsize,
}

fn scope_quota(dispatches: u32) -> Arc<crate::ScopeOutputQuota> {
    Arc::new(crate::ScopeOutputQuota::new(
        std::num::NonZeroU32::new(1024).unwrap(),
        std::num::NonZeroU32::new(dispatches).unwrap(),
    ))
}

struct ScopeWaitingAdmission {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    probe: Arc<AttemptProbe>,
}

#[async_trait::async_trait]
impl crate::ModelAttemptHooks for Arc<ScopeWaitingAdmission> {
    async fn begin(
        &self,
        _: &lingxi_core::host::ModelAttemptContext,
        _: &LlmRequest,
        _: &crate::PreparedLlmCall,
    ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(Box::new(ProbeLease {
            probe: self.probe.clone(),
            finished: false,
        }))
    }
}

fn scope_wire_service(
    protocol: ProtocolFamily,
    extra: serde_json::Value,
    auth: AuthStrategy,
    transport: Arc<dyn Transport>,
) -> ApiService {
    let endpoint = if auth == AuthStrategy::ChatGptPlan {
        "https://api.openai.com/v1"
    } else {
        "https://provider.example/v1"
    };
    let provider_id = match protocol {
        ProtocolFamily::GeminiGenerateContent | ProtocolFamily::GeminiInteractions => {
            ProviderId::Gemini
        }
        _ => ProviderId::OpenAI,
    };
    let mut profile =
        native_capability_profile(provider_id, "p", protocol, endpoint, "model", Vec::new());
    profile.auth = auth;
    profile.credential = CredentialConfig::Static {
        id: "scope-test".into(),
    };
    profile.wire_profile = Some(serde_json::from_value(serde_json::json!({
        "provider_id": "openai", "profile_name": "p", "protocol": protocol,
        "base_url": endpoint, "auth": auth,
        "model_list": "none",
        "pricing": if auth == AuthStrategy::ChatGptPlan { serde_json::json!({"billingMode":"subscription"}) } else { serde_json::json!({}) },
        "regions": ["international"], "extra": { "body": extra },
        "models": [{ "display_model": "model", "request_model": "model", "billing_model": "model" }],
    })).unwrap());
    let credential = if auth == AuthStrategy::ChatGptOAuth {
        Credential::ChatGptOAuth {
            access_token: "scope-test".into(),
            account_id: Some("scope-account".into()),
            fedramp: false,
        }
    } else if auth == AuthStrategy::ChatGptPlan {
        Credential::BearerToken("scope-test".into())
    } else {
        Credential::ApiKey("scope-test".into())
    };
    ApiService::new(
        Arc::new(
            ModelRuntime::from_config(ClientConfig {
                providers: vec![profile],
            })
            .unwrap()
            .with_credential_provider(Arc::new(crate::StaticCredentialProvider::new(credential))),
        ),
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
    )
}

impl llm_runtime::test_support::FixtureTransport for HttpOnlyProbeTransport {
    fn execute<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async { panic!("registered stream must not become nonstream fallback") })
    }
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        assert_eq!(
            request.stream_transport,
            crate::ProviderStreamTransport::Http
        );
        self.http_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let frames = self.frames.clone();
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(ScriptedFrames::new(frames)),
            })
        })
    }
    fn open_responses_websocket_session<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<Box<dyn crate::ResponsesWebSocketTransportSession>, LlmError>> {
        self.ws_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let frames = self.frames.clone();
        Box::pin(async move {
            Ok(Box::new(ProbeWsSession { frames })
                as Box<dyn crate::ResponsesWebSocketTransportSession>)
        })
    }
}
llm_runtime::impl_fixture_transport!(HttpOnlyProbeTransport);

struct ProbeWsSession {
    frames: Vec<Vec<u8>>,
}

impl crate::ResponsesWebSocketTransportSession for ProbeWsSession {
    fn send<'a>(
        &'a mut self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let frames = self.frames.clone();
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 101,
                headers: BTreeMap::new(),
                frames: Box::new(ScriptedFrames::new(frames)),
            })
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async { Ok(()) })
    }
}

struct HookRetirementProbe {
    service: std::sync::Weak<ApiService>,
    unlocked_drop: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl crate::ModelAttemptHooks for HookRetirementProbe {
    async fn begin(
        &self,
        _: &lingxi_core::host::ModelAttemptContext,
        _: &LlmRequest,
        _: &crate::PreparedLlmCall,
    ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
        panic!("retired hooks must never receive a request")
    }
}

impl Drop for HookRetirementProbe {
    fn drop(&mut self) {
        let unlocked = self
            .service
            .upgrade()
            .is_some_and(|service| service.model_attempt_hooks.try_write().is_ok());
        self.unlocked_drop
            .store(unlocked, std::sync::atomic::Ordering::SeqCst);
    }
}

fn ok_response_json() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_test",
        "model": "claude-sonnet-4-20250514",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

fn make_adapter_with_subscriber(
    transport: Arc<dyn Transport>,
    subscriber: SubscriberState,
) -> ApiService {
    std::env::set_var("ADAPTER_TEST_KEY", "test-key");
    let client = Arc::new(
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
                    var: "ADAPTER_TEST_KEY".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-20250514".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec!["claude".to_string()],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
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
        .expect("client"),
    );
    ApiService::new(
        client,
        transport,
        subscriber,
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
    .with_fast_policy_source(Arc::new(|| {
        Ok(crate::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        })
    }))
}

/// `max_retries` bounds the retry ladder; `None` keeps the production
/// default, which walks ten rungs before giving up.
fn make_adapter_with_retries(
    transport: Arc<dyn Transport>,
    max_retries: Option<u32>,
) -> ApiService {
    std::env::set_var("ADAPTER_TEST_KEY", "test-key");
    let client = Arc::new(
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
                    var: "ADAPTER_TEST_KEY".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-20250514".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec!["claude".to_string()],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
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
        .expect("client"),
    );
    ApiService::new_with_routing(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
        None,
        std::collections::BTreeMap::new(),
        max_retries,
        None,
    )
    .with_fast_policy_source(Arc::new(|| {
        Ok(crate::model::fast_admission::Policy {
            cached_org_enabled: true,
            ..Default::default()
        })
    }))
}

fn make_adapter(transport: Arc<dyn Transport>) -> ApiService {
    make_adapter_with_retries(transport, None)
}

fn make_request_adapter(transport: Arc<dyn Transport>) -> ApiService {
    let mut service = make_adapter(transport);
    let mut config = service.client.configuration();
    let anthropic = &mut config.providers[0];
    let fixture_model = anthropic.models[0].clone();
    let row = |model: &str| ModelProfile {
        display_model: model.into(),
        request_model: model.into(),
        billing_model: model.into(),
        aliases: Vec::new(),
        ..fixture_model.clone()
    };
    anthropic.models.extend(
        [
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-4-8-20260115",
            "claude-opus-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
            "claude-haiku-4-5",
        ]
        .into_iter()
        .map(row),
    );
    let profile_template = anthropic.clone();
    for (id, name, protocol, base, model) in [
        (
            ProviderId::OpenAI,
            "openai",
            ProtocolFamily::OpenAiResponses,
            "https://api.openai.com/v1",
            "gpt-5",
        ),
        (
            ProviderId::Gemini,
            "gemini",
            ProtocolFamily::GeminiGenerateContent,
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-2.5-pro",
        ),
        (
            ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            "deepseek",
            ProtocolFamily::OpenAiChat,
            "https://api.deepseek.com/v1",
            "deepseek-reasoner",
        ),
        (
            ProviderId::OpenAICompatible {
                name: "github-copilot".into(),
            },
            "github-copilot",
            ProtocolFamily::OpenAiChat,
            "https://api.githubcopilot.com",
            "gpt-5.2",
        ),
        (
            ProviderId::Custom {
                name: "custom".into(),
            },
            "custom",
            ProtocolFamily::OpenAiChat,
            "https://models.example.test",
            "custom-long-output-model",
        ),
    ] {
        config.providers.push(ProviderProfile {
            provider_id: id,
            profile_name: name.into(),
            protocol,
            base_url: base.into(),
            auth: AuthStrategy::None,
            credential: CredentialConfig::None,
            models: vec![row(model)],
            ..profile_template.clone()
        });
    }
    service.client = Arc::new(ModelRuntime::from_config(config).unwrap());
    service
}

fn make_adapter_for_protocol(
    protocol: ProtocolFamily,
    provider_id: ProviderId,
    base_url: &str,
) -> ApiService {
    make_adapter_for_protocol_with_transport(
        protocol,
        provider_id,
        base_url,
        "p",
        "model",
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
    )
}

fn make_adapter_for_protocol_with_transport(
    protocol: ProtocolFamily,
    provider_id: ProviderId,
    base_url: &str,
    profile_name: &str,
    model: &str,
    transport: Arc<dyn Transport>,
) -> ApiService {
    make_adapter_for_protocol_with_websocket(
        protocol,
        provider_id,
        base_url,
        profile_name,
        model,
        transport,
        false,
    )
}

fn make_adapter_for_protocol_with_websocket(
    protocol: ProtocolFamily,
    provider_id: ProviderId,
    base_url: &str,
    profile_name: &str,
    model: &str,
    transport: Arc<dyn Transport>,
    supports_websockets: bool,
) -> ApiService {
    let azure = if matches!(protocol, ProtocolFamily::AzureOpenAi) {
        Some(crate::AzureConfig {
            api_version: "2024-02-01".to_string(),
        })
    } else {
        None
    };
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: crate::Region::all(),
                provider_id,
                profile_name: profile_name.to_string(),
                base_url: base_url.to_string(),
                protocol,
                auth: AuthStrategy::None,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: model.to_string(),
                    request_model: model.to_string(),
                    billing_model: model.to_string(),
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
                azure,
                supports_websockets,
                supports_websocket_compression: false,
                websocket_connect_timeout_ms: None,
                vision_delegate: None,
                connection: Default::default(),
            }],
        })
        .expect("client"),
    );
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
}

async fn headers_after_inject_for_protocol(
    protocol: ProtocolFamily,
    provider_id: ProviderId,
    base_url: &str,
) -> BTreeMap<String, String> {
    let adapter = make_adapter_for_protocol(protocol, provider_id, base_url);
    let mut request = LlmRequest::new("model").with_user_text("hi");
    request.input.max_tokens = Some(4096);
    let mut prepared = adapter.client.prepare(&request).await.expect("prepare");
    adapter
        .inject_headers(&mut prepared, DispatchHeaderState::default())
        .unwrap();
    prepared.provider_request.headers
}

// ── Prompt-cache breakpoints (CACHE.1) ──────────────────────────────────

// Serializes the two prompt-cache tests: one mutates DISABLE_PROMPT_CACHING
// (process-global), so the default-on assertion in the other must not run
// concurrently. Lock poison is benign here — recover the guard.
static CACHE_ENV_LOCK: Mutex<()> = Mutex::new(());

struct CacheEnvRestore(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl CacheEnvRestore {
    fn clear(names: &[&'static str]) -> Self {
        let saved = names
            .iter()
            .map(|name| (*name, std::env::var_os(name)))
            .collect();
        for name in names {
            std::env::remove_var(name);
        }
        Self(saved)
    }
}

impl Drop for CacheEnvRestore {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            if let Some(value) = value {
                std::env::set_var(name, value);
            } else {
                std::env::remove_var(name);
            }
        }
    }
}

fn native_capability_profile(
    provider_id: ProviderId,
    profile_name: &str,
    protocol: ProtocolFamily,
    base_url: &str,
    model: &str,
    aliases: Vec<String>,
) -> ProviderProfile {
    ProviderProfile {
        wire_profile: None,
        regions: lingxi_llm_client::protocol::Region::all(),
        provider_id,
        profile_name: profile_name.to_owned(),
        base_url: base_url.to_owned(),
        protocol,
        auth: AuthStrategy::ApiKey,
        credential: CredentialConfig::Env {
            var: "ADAPTER_TEST_KEY".to_owned(),
        },
        models: vec![ModelProfile {
            display_model: model.to_owned(),
            request_model: model.to_owned(),
            billing_model: model.to_owned(),
            aliases,
            description: None,
            metadata: Default::default(),
            capabilities: Capabilities {
                streaming: true,
                tools: true,
                reasoning: true,
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
    }
}

fn native_capability_adapter(profiles: Vec<ProviderProfile>) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: profiles,
        })
        .expect("client"),
    );
    let transport = FakeTransport::always(ProviderResponse::json(200, ok_response_json()));
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_owned()),
            entrypoint: Some("cli".to_owned()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
}

fn text_user_msg(s: &str) -> ConversationMessage {
    ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::Text {
            text: s.to_string(),
            citations: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }
}

// ── 1P cache-EDITING (cache_edits / cache_reference, RESIDUAL 4) ───────────

/// A multi-message conversation: an assistant tool_use, a user tool_result,
/// an assistant text, then a trailing user text. Only the trailing message
/// carries the cache_control marker, so the tool_result (in an earlier user
/// message) is strictly within the cached prefix.
fn tool_result_conversation() -> Vec<ConversationMessage> {
    use lingxi_core::types::{ContentBlock as PB, ConversationMessage as CM, MessageId, ToolUseId};
    let tool_id = ToolUseId::new();
    vec![
        CM::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![PB::ToolUse {
                id: tool_id.clone(),
                name: "Read".to_string(),
                input: core_projection(&serde_json::json!({"path": "/x"})),
                provider_id: Some("toolu_abc".to_string()),
            }],
            stop_reason: None,
            resumable: None,
        },
        CM::User {
            api_message_override: None,
            interrupted_by_shutdown: None,
            refused_message_id: None,
            id: MessageId::new(),
            content: vec![PB::ToolResult {
                tool_use_id: tool_id,
                content: "file body".to_string().into(),
                is_error: Some(false),
                provider_tool_use_id: Some("toolu_abc".to_string()),
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
            turn_companion: None,
            usage_limit_note: None,
            origin: None,
            turn_origin: None,
        },
        CM::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![PB::Text {
                text: "ok".to_string(),
                citations: None,
            }],
            stop_reason: None,
            resumable: None,
        },
        text_user_msg("continue"),
    ]
}

// ── build_request profile threading (Unit B Task 5) ──────────────────────

fn reasoning_route_fixture() -> ApiService {
    let base = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|provider| provider.profile_name == "anthropic")
        .unwrap();
    let providers = [
        (
            "office-sonnet",
            ProviderId::AnthropicFirstParty,
            ProtocolFamily::AnthropicMessages,
        ),
        (
            "company-key#1",
            ProviderId::OpenAI,
            ProtocolFamily::OpenAiResponses,
        ),
        (
            "google-work",
            ProviderId::Gemini,
            ProtocolFamily::GeminiGenerateContent,
        ),
        (
            "coding-west",
            ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            ProtocolFamily::OpenAiChat,
        ),
        (
            "openai",
            ProviderId::Custom {
                name: "unknown-lab".into(),
            },
            ProtocolFamily::OpenAiChat,
        ),
    ]
    .into_iter()
    .map(|(name, id, protocol)| {
        let mut provider = base.clone();
        provider.profile_name = name.into();
        provider.provider_id = id;
        provider.protocol = protocol;
        provider.base_url = "https://reasoning.example.test".into();
        provider.wire_profile = None;
        provider.connection = Default::default();
        provider.auth = AuthStrategy::None;
        provider.credential = CredentialConfig::None;
        provider.models.truncate(1);
        let model = &mut provider.models[0];
        model.request_model = "reasoning-model".into();
        model.display_model = "reasoning-model".into();
        model.billing_model = "reasoning-model".into();
        model.aliases.clear();
        model.metadata = Default::default();
        model.capabilities.reasoning = true;
        provider
    })
    .collect();
    ApiService::new(
        Arc::new(ModelRuntime::from_config(ClientConfig { providers }).unwrap()),
        FakeTransport::always(ProviderResponse::json(200, ok_response_json())),
        Default::default(),
        Default::default(),
        "test",
        None,
        None,
    )
}

// ── build_request thinking / temperature / max_tokens (DIV-1/3/4) ────────

// Serialize the env-touching thinking tests: they mutate process-global
// LINGXI_DISABLE_THINKING. A module-level mutex keeps them from racing
// each other (and is poison-tolerant).
static THINKING_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_thinking_env() {
    std::env::remove_var("LINGXI_DISABLE_THINKING");
    std::env::remove_var("LINGXI_DISABLE_ADAPTIVE_THINKING");
}

// ── CLAUDE_CODE_EXTRA_BODY merge (claude-code B0t; parity 2.1.207) ────────

/// Exercise ApiService's logical capture and physical dispatch. A fixed
/// terminal response lets every wire family expose its request unchanged.
async fn request_after_service_dispatch(
    adapter: &ApiService,
    request: &LlmRequest,
) -> ProviderRequest {
    let transport = FakeTransport::always(ProviderResponse::json(
        400,
        serde_json::json!({"error":{"type":"invalid_request_error","message":"fixture captured request"}}),
    ));
    let mut service = adapter.fork_independent();
    service.transport = transport.clone();
    let error = service
        .execute_non_stream_request(
            request.clone(),
            NonStreamingRequestClass::Main,
            Default::default(),
        )
        .await
        .expect_err("fixture returns a terminal HTTP 400");
    assert_eq!(
        transport.seen_count(),
        1,
        "fixture must dispatch once: {error:?}"
    );
    let sent = transport.seen.lock().unwrap()[0].clone();
    sent
}

async fn body_after_inject(
    adapter: &ApiService,
    request: &LlmRequest,
) -> lingxi_llm_client::protocol::JsonPayload {
    request_after_service_dispatch(adapter, request)
        .await
        .body_json
}

// ── effective_subscriber (batch-5 Task 3: live SharedSubscription) ───────

fn shared_slot(
    snap: Option<lingxi_core::host::subscription::SubscriptionSnapshot>,
) -> lingxi_core::host::subscription::SharedSubscription {
    Arc::new(std::sync::RwLock::new(snap))
}

// ── Previously-ignored tests (un-ignored, ported to FakeTransport) ────────

// ── New plan-named tests ──────────────────────────────────────────────────

// ── (cc 2.1.219) `anthropic-dispatch-id: v2s` opt-in ────────────────────

/// Guards the process-global `tengu_cedar_lattice` override (and
/// `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL`).
///
/// `telemetry::test_set_flag` writes a PROCESS-GLOBAL map, so serializing
/// only the tests that flip it is not enough: while the flip is live, every
/// *concurrently running* test in this binary reads the flipped value too.
/// A leaked opt-in makes first-party attempts carry `anthropic-dispatch-id`,
/// and [`ApiService::note_dispatch_header_failure`] then inserts one
/// extra, budget-free "strip and retry" attempt on the first 5xx — which
/// silently shifts every attempt-count and fallback-position assertion in
/// the drive-loop tests (they consumed one more canned response than they
/// expected and never reached the fallback entry).
///
/// So: a test that FLIPS the flag takes [`cedar_lattice_write`]; a test
/// whose expectations depend on the header being ABSENT takes
/// [`cedar_lattice_read`]. Readers exclude writers, which is the whole
/// invariant — concurrent readers of an unflipped flag are fine.
/// Turns `tengu_cedar_lattice` on for the duration of a test and clears it
/// on drop, so a panicking test cannot leak the opt-in into its neighbours.
struct CedarLatticeOn;

impl CedarLatticeOn {
    fn set() -> Self {
        ::telemetry::test_set_flag("tengu_cedar_lattice", true);
        Self
    }
}

impl Drop for CedarLatticeOn {
    fn drop(&mut self) {
        ::telemetry::test_clear_flag("tengu_cedar_lattice");
    }
}

struct DispatchTrafficTransport {
    requests: Mutex<Vec<ProviderRequest>>,
    fail_first: bool,
}

impl llm_runtime::test_support::FixtureTransport for DispatchTrafficTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request.clone());
        let fail = self.fail_first && requests.len() == 1;
        Box::pin(async move {
            if fail {
                Err(LlmError::ProviderInternal)
            } else {
                Ok(ProviderResponse::json(200, ok_response_json()))
            }
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request.clone());
        let fail = self.fail_first && requests.len() == 1;
        Box::pin(async move {
            if fail {
                Err(LlmError::ProviderInternal)
            } else {
                Ok(StreamingResponse {
                    status: 200,
                    headers: BTreeMap::new(),
                    frames: Box::new(ScriptedFrames::new(
                        ScriptedStreamTransport::anthropic_success().frames.clone(),
                    )),
                })
            }
        })
    }
}
llm_runtime::impl_fixture_transport!(DispatchTrafficTransport);

struct DispatchRecoveryFrames(VecDeque<Result<crate::RawStreamFrame, LlmError>>);

impl crate::FrameStream for DispatchRecoveryFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
        let result = self.0.pop_front().transpose();
        Box::pin(async move { result })
    }
}

#[derive(Clone, Copy)]
enum DispatchFault {
    Http(u16, bool),
    Connection,
    Timeout,
    Body(bool),
}

struct DispatchRecoveryTransport {
    requests: Mutex<Vec<ProviderRequest>>,
    fault: DispatchFault,
    failures: usize,
}

impl DispatchRecoveryTransport {
    fn record(&self, request: &ProviderRequest) -> bool {
        let mut requests = self.requests.lock().unwrap();
        requests.push(request.clone());
        requests.len() <= self.failures
    }

    fn response(&self, failed: bool) -> Result<ProviderResponse, LlmError> {
        if !failed {
            return Ok(ProviderResponse::json(200, ok_response_json()));
        }
        match self.fault {
            DispatchFault::Http(status, declined) => Ok(ProviderResponse {
                status,
                headers: BTreeMap::from([
                    ("request-id".into(), "dispatch-request".into()),
                    (
                        "x-should-retry".into(),
                        if declined { "false" } else { "true" }.into(),
                    ),
                ]),
                request_id: Some("dispatch-request".into()),
                body_json: provider_payload(
                    serde_json::json!({"type":"error","error":{"type":"api_error","message":"service unavailable"}}),
                ),
            }),
            DispatchFault::Timeout => Err(LlmError::TransportTimeout {
                message: "HTTP request timed out".into(),
            }),
            DispatchFault::Connection | DispatchFault::Body(_) => Err(LlmError::Transport {
                message: "connection reset".into(),
            }),
        }
    }

    fn headers(&self) -> Vec<Option<String>> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.headers.get(DISPATCH_ID_HEADER).cloned())
            .collect()
    }
}

impl llm_runtime::test_support::FixtureTransport for DispatchRecoveryTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let result = self.response(self.record(request));
        Box::pin(async move { result })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let failed = self.record(request);
        let result = if failed && matches!(self.fault, DispatchFault::Body(_)) {
            let mut frames = Vec::new();
            if matches!(self.fault, DispatchFault::Body(true)) {
                frames.push(Ok(crate::RawStreamFrame::new(serde_json::to_vec(&serde_json::json!({"type":"message_start","message":{"id":"msg_dispatch","model":"claude-sonnet-4-20250514","content":[],"usage":{"input_tokens":1,"output_tokens":0}}})).unwrap())));
            }
            frames.push(Err(LlmError::Transport {
                message: "connection reset".into(),
            }));
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(DispatchRecoveryFrames(frames.into())),
            })
        } else {
            self.response(failed).map(|response| {
                let frames = if response.status >= 400 {
                    vec![Ok(crate::RawStreamFrame::new(
                        serde_json::to_vec(&response.body_json).unwrap(),
                    ))]
                } else {
                    ScriptedStreamTransport::anthropic_success()
                        .frames
                        .iter()
                        .cloned()
                        .map(|bytes| Ok(crate::RawStreamFrame::new(bytes)))
                        .collect()
                };
                StreamingResponse {
                    status: response.status,
                    headers: response.headers,
                    frames: Box::new(DispatchRecoveryFrames(frames.into())),
                }
            })
        };
        Box::pin(async move { result })
    }
}
llm_runtime::impl_fixture_transport!(DispatchRecoveryTransport);

async fn dispatch_result(
    service: &ApiService,
    source: &str,
    streaming: bool,
    side_query: bool,
) -> Result<(), LlmError> {
    use futures::StreamExt;
    let model = "claude-sonnet-4-20250514";
    let mut request = LlmRequest::new(model).with_user_text("hi");
    request.input.max_tokens = Some(100);
    request.execution.query_source = Some(source.into());
    if streaming {
        let mut stream = service.stream_request(request).await?;
        while let Some(event) = stream.next().await {
            event?;
        }
    } else if side_query {
        service.execute_side_query_request(request).await?;
    } else {
        let mut request = crate::MessagesCreateRequest::new(
            model,
            None,
            None,
            vec![ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "hi".into(),
            )],
            Vec::new(),
        );
        request.opts.max_output_tokens = Some(100);
        request.opts.query_source = Some(source.into());
        service.messages_create(request).await?;
    }
    Ok(())
}

struct DreamyFrostGuard;
impl DreamyFrostGuard {
    fn set(enabled: bool) -> Self {
        ::telemetry::test_set_flag("tengu_dreamy_frost", enabled);
        Self
    }
}
impl Drop for DreamyFrostGuard {
    fn drop(&mut self) {
        ::telemetry::test_clear_flag("tengu_dreamy_frost");
    }
}

async fn run_dispatch_traffic(service: &ApiService, source: Option<&str>, streaming: bool) {
    use futures::StreamExt;

    let model = "claude-sonnet-4-20250514";
    if streaming {
        let mut request = LlmRequest::new(model).with_user_text("hi");
        request.input.max_tokens = Some(100);
        request.execution.query_source = source.map(str::to_owned);
        let mut stream = service.stream_request(request).await.unwrap();
        while let Some(event) = stream.next().await {
            event.unwrap();
        }
    } else {
        let mut request = crate::MessagesCreateRequest::new(
            model,
            None,
            None,
            vec![ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                "hi".into(),
            )],
            Vec::new(),
        );
        request.opts.max_output_tokens = Some(100);
        request.opts.query_source = source.map(str::to_owned);
        service.messages_create(request).await.unwrap();
    }
}

/// A `FrameStream` whose body drops with a connection error before any
/// event — the oracle's `Bs!==null && !oc`.
struct ConnErrOnFirstFrame;

impl crate::FrameStream for ConnErrOnFirstFrame {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
        Box::pin(async {
            Err(LlmError::Transport {
                message: "ECONNRESET".to_string(),
            })
        })
    }
}

/// Connect succeeds on both attempts; the FIRST stream body drops before
/// the first event, the second is a clean anthropic stream. Records the
/// `anthropic-dispatch-id` value seen per attempt.
struct BodyPhaseDropThenOk {
    attempts: Mutex<Vec<Option<String>>>,
}

impl llm_runtime::test_support::FixtureTransport for BodyPhaseDropThenOk {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async {
            Err(LlmError::Transport {
                message: "execute not used".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let attempt = {
            let mut seen = self
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            seen.push(request.headers.get("anthropic-dispatch-id").cloned());
            seen.len()
        };
        Box::pin(async move {
            let frames: Box<dyn crate::FrameStream> = if attempt == 1 {
                Box::new(ConnErrOnFirstFrame)
            } else {
                Box::new(ScriptedFrames::new(vec![
                    br#"{"type":"message_start","message":{"id":"msg_t","model":"claude-sonnet-4-20250514","usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#.to_vec(),
                    br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
                    br#"{"type":"message_stop"}"#.to_vec(),
                ]))
            };
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames,
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(BodyPhaseDropThenOk);

struct RegisteredRequestSession(lingxi_core::types::SessionId);
#[async_trait::async_trait]
impl crate::ModelAttemptHooks for RegisteredRequestSession {
    fn request_session_id(
        &self,
        _: &lingxi_core::host::ModelAttemptContext,
    ) -> Option<lingxi_core::types::SessionId> {
        Some(self.0)
    }
    async fn begin(
        &self,
        _: &lingxi_core::host::ModelAttemptContext,
        _: &LlmRequest,
        _: &crate::PreparedLlmCall,
    ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
        unreachable!("identity lookup does not dispatch")
    }
}

// ---- MULTIMODAL.6: per-request media cap (stripExcessMediaItems) ----

fn img(n: usize) -> lingxi_core::types::ContentBlock {
    lingxi_core::types::ContentBlock::Image {
        source: lingxi_core::types::ImageSource::Base64 {
            media_type: "image/png".to_string(),
            data: format!("img{n}"),
        },
    }
}

fn user_with_imgs(range: std::ops::Range<usize>) -> ConversationMessage {
    let mut content = vec![ContentBlock::Text {
        text: "hi".to_string(),
        citations: None,
    }];
    content.extend(range.map(img));
    ConversationMessage::User {
        api_message_override: None,
        interrupted_by_shutdown: None,
        refused_message_id: None,
        id: lingxi_core::types::MessageId::new(),
        content,
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
        turn_companion: None,
        usage_limit_note: None,
        origin: None,
        turn_origin: None,
    }
}

fn image_data_in_order(msgs: &[ConversationMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in msgs {
        if let ConversationMessage::User { content, .. }
        | ConversationMessage::Assistant { content, .. } = m
        {
            for b in content {
                if let ContentBlock::Image {
                    source: lingxi_core::types::ImageSource::Base64 { data, .. },
                } = b
                {
                    out.push(data.clone());
                }
            }
        }
    }
    out
}

// ── Fix 2: error_kind label-parity test ──────────────────────────────────

// ── Task 8: subscriber 429 gate end-to-end through the adapter ───────────

// ── M12: rate-limit warning flicker (2.1.196 monotonic guard) ────────────

fn unified_headers(status: &str) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert(
        "anthropic-ratelimit-unified-representative-claim".to_string(),
        "seven_day".to_string(),
    );
    h.insert(
        "anthropic-ratelimit-unified-status".to_string(),
        status.to_string(),
    );
    h
}

fn near_limit_headers(
    utilization: &str,
    reset_secs: u64,
    overage_status: Option<&str>,
) -> BTreeMap<String, String> {
    let mut h = BTreeMap::new();
    h.insert(
        "anthropic-ratelimit-unified-representative-claim".to_string(),
        "five_hour".to_string(),
    );
    h.insert(
        "anthropic-ratelimit-unified-status".to_string(),
        "allowed".to_string(),
    );
    h.insert(
        "anthropic-ratelimit-unified-5h-utilization".to_string(),
        utilization.to_string(),
    );
    h.insert(
        "anthropic-ratelimit-unified-5h-reset".to_string(),
        reset_secs.to_string(),
    );
    if let Some(overage_status) = overage_status {
        h.insert(
            "anthropic-ratelimit-unified-overage-status".to_string(),
            overage_status.to_string(),
        );
    }
    h
}

fn reset_in(seconds: u64) -> u64 {
    u64::try_from(ApiService::now_ms() / 1000)
        .unwrap_or(u64::MAX)
        .saturating_add(seconds)
}

// ── B6-T1: terminal-only 429 state promotion (pending slot) ──────────────
//
// claude-code updates the limits/raw module state ONLY in the terminal
// catch handler `extractQuotaStatusFromError` (claudeAiLimits.ts:487),
// never on a retried attempt that later recovers. The Rust seam stages
// each 429 attempt's snapshot in a `pending_429` slot and promotes it into
// the live caches only when the retry loop declares the error TERMINAL —
// and discards it on drive-entry and on any subsequent success.

fn assistant_with_thinking() -> lingxi_core::types::ConversationMessage {
    lingxi_core::types::ConversationMessage::Assistant {
        per_turn_effort: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![
            lingxi_core::types::ContentBlock::Thinking {
                thinking: "secret".into(),
                signature: Some("sig".into()),
            },
            lingxi_core::types::ContentBlock::Text {
                text: "hello".into(),
                citations: None,
            },
        ],
        stop_reason: Some("end_turn".into()),
        resumable: None,
    }
}

fn prefix_lock_error_body() -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "This message is not created in this conversation"
        }
    })
}

fn prefix_lock_error_response() -> ProviderResponse {
    ProviderResponse {
        status: 400,
        headers: BTreeMap::from([(
            "anthropic-thinking-prefix-mismatch".into(),
            "block=messages.0.content.0;kind=signature".into(),
        )]),
        body_json: provider_payload(prefix_lock_error_body()),
        request_id: None,
    }
}

// ── 3c-T1: streaming 429 + retry-after header drives correct delay ────────

struct RawBodyFrames(Option<Vec<u8>>);
impl crate::FrameStream for RawBodyFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
        let frame = self.0.take().map(crate::RawStreamFrame::new);
        Box::pin(async move { Ok(frame) })
    }
}

fn bedrock_event_stream_frame(payload: &[u8]) -> Vec<u8> {
    let headers = [
        (":message-type", "event"),
        (":event-type", "chunk"),
        (":content-type", "application/json"),
    ];
    let mut encoded_headers = Vec::new();
    for (name, value) in headers {
        encoded_headers.push(name.len() as u8);
        encoded_headers.extend_from_slice(name.as_bytes());
        encoded_headers.push(7); // AWS event-stream string header.
        encoded_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        encoded_headers.extend_from_slice(value.as_bytes());
    }

    let total_len = 12 + encoded_headers.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total_len);
    frame.extend_from_slice(&(total_len as u32).to_be_bytes());
    frame.extend_from_slice(&(encoded_headers.len() as u32).to_be_bytes());
    frame.extend_from_slice(&crate::crc32(&frame).to_be_bytes());
    frame.extend_from_slice(&encoded_headers);
    frame.extend_from_slice(payload);
    let message_crc = crate::crc32(&frame);
    frame.extend_from_slice(&message_crc.to_be_bytes());
    frame
}

fn successful_bedrock_anthropic_frames(model: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;

    let events = vec![
        serde_json::to_vec(&serde_json::json!({
            "type": "message_start",
            "message": {
                "id": "msg_context_1m",
                "model": model,
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 0,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0
                }
            }
        }))
        .unwrap(),
        br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
        br#"{"type":"message_stop"}"#.to_vec(),
    ];
    events
        .into_iter()
        .map(|event| {
            let body = serde_json::to_vec(&serde_json::json!({
                "bytes": base64::engine::general_purpose::STANDARD.encode(event)
            }))
            .expect("event-stream chunk envelope");
            bedrock_event_stream_frame(&body)
        })
        .collect()
}

/// A Transport that sequences execute responses AND can return scripted
/// streaming (`open_stream`) responses.
struct FakeStreamTransport {
    /// Sequence of `open_stream` results.
    responses: Mutex<Vec<FakeStreamResp>>,
    call_count: Mutex<usize>,
    requests: Mutex<Vec<ProviderRequest>>,
}

#[allow(dead_code)]
enum FakeStreamResp {
    /// Streaming response with given status + headers + no frames.
    Status {
        status: u16,
        headers: BTreeMap<String, String>,
        body_json: Option<serde_json::Value>,
    },
    Frames {
        status: u16,
        headers: BTreeMap<String, String>,
        frames: Vec<Vec<u8>>,
    },
    /// Terminal transport error (e.g. connection failure).
    Err(LlmError),
}

impl FakeStreamTransport {
    fn sequence(stream_resps: Vec<FakeStreamResp>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(stream_resps),
            call_count: Mutex::new(0),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn stream_call_count(&self) -> usize {
        *self.call_count.lock().unwrap()
    }

    fn request_max_tokens(&self, index: usize) -> Option<u64> {
        request_body_value(&self.requests.lock().unwrap()[index].body_json)
            .get("max_tokens")
            .and_then(serde_json::Value::as_u64)
    }

    fn seen_headers(&self, index: usize) -> BTreeMap<String, String> {
        self.requests.lock().unwrap()[index].headers.clone()
    }

    fn seen_body(&self, index: usize) -> lingxi_llm_client::protocol::JsonPayload {
        self.requests.lock().unwrap()[index].body_json.clone()
    }
}

impl llm_runtime::test_support::FixtureTransport for FakeStreamTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "execute not scripted in FakeStreamTransport".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.requests.lock().unwrap().push(request.clone());
        let mut count = self.call_count.lock().unwrap();
        let idx = (*count).min(self.responses.lock().unwrap().len().saturating_sub(1));
        *count += 1;
        drop(count);
        let resp = {
            let resps = self.responses.lock().unwrap();
            match &resps[idx] {
                FakeStreamResp::Status {
                    status,
                    headers,
                    body_json,
                } => {
                    let body = body_json
                        .as_ref()
                        .map(|value| serde_json::to_vec(value).expect("scripted JSON body"));
                    Ok(StreamingResponse {
                        status: *status,
                        headers: headers.clone(),
                        frames: Box::new(RawBodyFrames(body)),
                    })
                }
                FakeStreamResp::Frames {
                    status,
                    headers,
                    frames,
                } => Ok(StreamingResponse {
                    status: *status,
                    headers: headers.clone(),
                    frames: Box::new(ScriptedFrames::new(frames.clone())),
                }),
                FakeStreamResp::Err(e) => Err(e.clone()),
            }
        };
        Box::pin(async move { resp })
    }
}
llm_runtime::impl_fixture_transport!(FakeStreamTransport);

/// A transport whose connect/header phase never resolves.
struct HangingOpenTransport {
    calls: Mutex<usize>,
}

impl HangingOpenTransport {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(0),
        })
    }

    fn call_count(&self) -> usize {
        *self.calls.lock().unwrap()
    }
}

impl llm_runtime::test_support::FixtureTransport for HangingOpenTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async {
            Err(LlmError::Transport {
                message: "execute not scripted".to_string(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        *self.calls.lock().unwrap() += 1;
        Box::pin(std::future::pending())
    }
}
llm_runtime::impl_fixture_transport!(HangingOpenTransport);

// ── M12: streaming idle watchdog (cc 2.1.196 default-on) ─────────────────

/// A frame stream that NEVER produces a frame nor completes — models a
/// hung connection so the idle watchdog must fire.
struct HangingFrames;
impl crate::FrameStream for HangingFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
        Box::pin(std::future::pending())
    }
}

/// Transport that opens a 200 stream whose frames hang forever.
struct HangingStreamTransport;
impl llm_runtime::test_support::FixtureTransport for HangingStreamTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "execute not scripted".to_string(),
            })
        })
    }
    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(HangingFrames),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(HangingStreamTransport);

// ── Task 1: routing.fallback/retry adapter tests ──────────────────────────

/// Build an adapter with routing overrides for per-model fallback and retry.
fn make_adapter_with_routing(
    transport: Arc<dyn Transport>,
    fallback_overrides: std::collections::BTreeMap<String, Vec<String>>,
    settings_max_retries: Option<u32>,
    settings_backoff_ms: Option<u64>,
) -> ApiService {
    #[allow(deprecated)]
    std::env::set_var("ROUTING_TEST_KEY", "test-key");
    let client = Arc::new(
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
                    var: "ROUTING_TEST_KEY".to_string(),
                },
                models: vec![
                    ModelProfile {
                        display_model: "claude-opus-4-6".to_string(),
                        request_model: "claude-opus-4-6".to_string(),
                        billing_model: "claude-opus-4-6".to_string(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
                            ..Default::default()
                        },
                    },
                    ModelProfile {
                        display_model: "claude-haiku-4-20250307".to_string(),
                        request_model: "claude-haiku-4-20250307".to_string(),
                        billing_model: "claude-haiku-4".to_string(),
                        aliases: vec![],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
                            ..Default::default()
                        },
                    },
                    ModelProfile {
                        display_model: "claude-sonnet-4-20250514".to_string(),
                        request_model: "claude-sonnet-4-20250514".to_string(),
                        billing_model: "claude-sonnet-4".to_string(),
                        aliases: vec!["claude".to_string()],
                        description: None,
                        metadata: Default::default(),
                        capabilities: Capabilities {
                            streaming: true,
                            tools: true,
                            reasoning: true,
                            ..Default::default()
                        },
                    },
                ],
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
        .expect("client"),
    );
    ApiService::new_with_routing(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
        None,
        fallback_overrides,
        settings_max_retries,
        settings_backoff_ms,
    )
}

fn routing_ok_response_json() -> serde_json::Value {
    serde_json::json!({
        "id": "msg_routing",
        "model": "claude-sonnet-4-20250514",
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

// ── Task 5: fallback chain walk tests ────────────────────────────────────

// ── T3 Step 2: 2xx rate-limit header feed ────────────────────────────────

// ── count_tokens (inherent ApiService) ──────────────────────────────────────

// ── T3 Step 3: stream telemetry twins ────────────────────────────────────

/// A `FrameStream` that yields scripted raw SSE frames (encoded as
/// JSON byte sequences) then terminates. Used to drive the stream decoder
/// with a controlled event sequence so the `drive_stream` unfold
/// emits telemetry at the right points.
struct ScriptedFrames {
    frames: Vec<Vec<u8>>,
    idx: usize,
}

impl ScriptedFrames {
    fn new(frames: Vec<Vec<u8>>) -> Self {
        Self { frames, idx: 0 }
    }
}

impl crate::FrameStream for ScriptedFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<crate::RawStreamFrame>, LlmError>> {
        let result = if self.idx < self.frames.len() {
            let bytes = self.frames[self.idx].clone();
            self.idx += 1;
            Ok(Some(crate::RawStreamFrame::new(bytes)))
        } else {
            Ok(None)
        };
        Box::pin(async move { result })
    }
}

/// A transport that delivers a fixed successful stream ending in `message_stop`.
///
/// Builds valid Anthropic SSE frames (as raw JSON lines) so the `AnthropicMessages`
/// codec can decode them. The stream ends with `message_stop` which is the
/// terminal event → should trigger `emit_succeeded` once.
struct ScriptedStreamTransport {
    frames: Vec<Vec<u8>>,
    headers: BTreeMap<String, String>,
    status: u16,
}

impl ScriptedStreamTransport {
    /// Build a transport that delivers a minimal valid anthropic stream:
    /// `message_start` → `message_delta(stop_reason=end_turn)` → `message_stop`.
    fn anthropic_success() -> Arc<Self> {
        let frames = vec![
            br#"{"type":"message_start","message":{"id":"msg_t","model":"claude-sonnet-4-20250514","usage":{"input_tokens":1,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}"#.to_vec(),
            br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec(),
            br#"{"type":"message_stop"}"#.to_vec(),
        ];
        Arc::new(Self {
            frames,
            headers: BTreeMap::new(),
            status: 200,
        })
    }

    /// Build a transport that delivers a single malformed frame → decoder error.
    fn malformed_frame() -> Arc<Self> {
        let frames = vec![b"not-valid-json".to_vec()];
        Arc::new(Self {
            frames,
            headers: BTreeMap::new(),
            status: 200,
        })
    }
}

impl llm_runtime::test_support::FixtureTransport for ScriptedStreamTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            Err(LlmError::Transport {
                message: "execute not used in ScriptedStreamTransport".into(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let frames: Vec<Vec<u8>> = self.frames.clone();
        let headers = self.headers.clone();
        let status = self.status;
        Box::pin(async move {
            Ok(StreamingResponse {
                status,
                headers,
                frames: Box::new(ScriptedFrames::new(frames)),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(ScriptedStreamTransport);

/// Returns a scripted sequence of HTTP stream responses and records the
/// exact beta headers of each physical request.
struct NativeAfkStreamRetryTransport {
    responses: Mutex<std::collections::VecDeque<(u16, BTreeMap<String, String>, Vec<Vec<u8>>)>>,
    seen: Mutex<Vec<ProviderRequest>>,
}

impl llm_runtime::test_support::FixtureTransport for NativeAfkStreamRetryTransport {
    fn execute<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async {
            Err(LlmError::Transport {
                message: "execute is not used in Native AFK stream retry test".into(),
            })
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        self.seen.lock().unwrap().push(request.clone());
        let next = self.responses.lock().unwrap().pop_front();
        Box::pin(async move {
            let (status, headers, frames) = next.ok_or_else(|| LlmError::Transport {
                message: "Native AFK stream retry sequence exhausted".into(),
            })?;
            Ok(StreamingResponse {
                status,
                headers,
                frames: Box::new(ScriptedFrames::new(frames)),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(NativeAfkStreamRetryTransport);

/// Build a streaming adapter with an attached analytics bus.
async fn make_stream_adapter_with_bus(
    transport: Arc<dyn Transport>,
) -> (ApiService, Arc<::telemetry::InMemorySink>) {
    use ::telemetry::{AnalyticsBus, InMemorySink};

    #[allow(deprecated)]
    std::env::set_var("STREAM_TELEM_TEST_KEY", "test-key");
    let client = Arc::new(
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
                    var: "STREAM_TELEM_TEST_KEY".to_string(),
                },
                models: vec![ModelProfile {
                    display_model: "claude-sonnet-4-20250514".to_string(),
                    request_model: "claude-sonnet-4-20250514".to_string(),
                    billing_model: "claude-sonnet-4".to_string(),
                    aliases: vec![],
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
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
        .expect("client"),
    );

    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;

    let adapter = ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        Some(bus),
        None,
    );
    (adapter, sink)
}

// ── AWS auth refresh trigger (2.1.198 V_c/G_c/s_f + Ygf) ─────────────────

/// Counting stand-in for the `ZBd` driver — the drive loop only needs the
/// object-safe `AwsAuthRefresh` seam.
#[derive(Debug, Default)]
struct CountingAwsRefresh {
    calls: std::sync::atomic::AtomicU32,
}

impl crate::auth::external_aws::AwsAuthRefresh for CountingAwsRefresh {
    fn refresh(&self) -> BoxFuture<'_, bool> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { true })
    }
}

/// Bedrock-provider adapter with the AWS auth-refresh seam attached.
fn make_bedrock_adapter_with_aws(
    transport: Arc<dyn Transport>,
    aws: Arc<CountingAwsRefresh>,
) -> ApiService {
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: vec![ProviderProfile {
                wire_profile: None,
                regions: lingxi_llm_client::protocol::Region::all(),
                provider_id: ProviderId::BedrockClaude,
                profile_name: "bedrock".to_string(),
                base_url: "https://bedrock-runtime.us-east-1.amazonaws.com".to_string(),
                protocol: ProtocolFamily::BedrockClaude,
                // Auth None so prepare() succeeds without SigV4 material —
                // the refresh trigger keys off the RESPONSE error + the
                // route's provider_id, not the auth strategy.
                auth: AuthStrategy::None,
                credential: CredentialConfig::None,
                models: vec![ModelProfile {
                    display_model: "model".to_string(),
                    request_model: "model".to_string(),
                    billing_model: "model".to_string(),
                    aliases: Vec::new(),
                    description: None,
                    metadata: Default::default(),
                    capabilities: Capabilities {
                        streaming: true,
                        tools: true,
                        reasoning: true,
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
        .expect("client"),
    );
    ApiService::new(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv {
            user_type: Some("external".to_string()),
            entrypoint: Some("cli".to_string()),
            ..Default::default()
        },
        "0.0.0",
        None,
        None,
    )
    .with_aws_auth(aws)
}

/// 401 `authentication_error` body (decodes to `LlmError::Authentication`
/// through the Bedrock codec's Anthropic-shape error decode) — the
/// expired-STS terminal the 2.1.198 trigger classifies via `V_c`.
fn expired_sts_response() -> ProviderResponse {
    ProviderResponse::json(
        401,
        serde_json::json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "The security token included in the request is expired"
            }
        }),
    )
}

mod request_session_capture_tests {
    use super::*;

    fn service_with_metadata() -> ApiService {
        let mut metadata =
            lingxi_llm_client::providers::anthropic::request_policy::AnthropicClientMetadata::
                claude_code_2_1_293("darwin", "arm64");
        metadata.session_id = Some("stale-builder-session".into());
        make_adapter(FakeTransport::always(ProviderResponse::json(
            200,
            ok_response_json(),
        )))
        .with_anthropic_client_metadata(metadata)
    }

    #[tokio::test]
    async fn live_request_source_is_captured_per_request_and_none_does_not_fall_back() {
        let service = service_with_metadata();
        let source: RequestSessionIdSource =
            Arc::new(|| Box::pin(async { Some("live-session".to_owned()) }));
        service.set_request_session_id_source(source);

        let mut request = LlmRequest::new("claude-sonnet-4-5");
        service.capture_request_session_id(&mut request).await;
        assert_eq!(
            request.execution.request_session_id.as_deref(),
            Some("live-session")
        );

        let source: RequestSessionIdSource = Arc::new(|| Box::pin(async { None }));
        service.set_request_session_id_source(source);
        let mut request = LlmRequest::new("claude-sonnet-4-5");
        service.capture_request_session_id(&mut request).await;
        assert_eq!(request.execution.request_session_id, None);
    }

    #[tokio::test]
    async fn task_local_session_precedes_the_live_request_source() {
        let service = service_with_metadata();
        let source: RequestSessionIdSource =
            Arc::new(|| Box::pin(async { Some("wrong-live-session".to_owned()) }));
        service.set_request_session_id_source(source);
        let expected = lingxi_core::types::SessionId::new();

        let captured =
            lingxi_core::host::session_flags::scope_request_session_id(expected, async {
                let mut request = LlmRequest::new("claude-sonnet-4-5");
                service.capture_request_session_id(&mut request).await;
                request.execution.request_session_id
            })
            .await;

        assert_eq!(captured, Some(expected.as_uuid().to_string()));
    }
}

async fn run_bedrock_afk_stream_retry(
    model: &str,
    first_error: &str,
    second_error: &str,
) -> Arc<FakeStreamTransport> {
    use futures::StreamExt;
    use lingxi_llm_client::NativeAnthropicRequestContext;

    let _cache_env_lock = CACHE_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _thinking_env_lock = THINKING_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _restore = CacheEnvRestore::clear(&[
        "ANTHROPIC_BEDROCK_BASE_URL",
        "CLAUDE_CODE_SIMULATE_PROXY_USAGE",
        "CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS",
        "CLAUDE_CODE_EXTRA_BODY",
    ]);
    assert!(
        !lingxi_core::host::compliance_taints::is_tainted("hipaa"),
        "this fixture requires the initialized-empty compliance taint state"
    );
    let error_response = |message: &str| FakeStreamResp::Status {
        status: 400,
        headers: BTreeMap::new(),
        body_json: Some(serde_json::json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message}
        })),
    };
    let transport = FakeStreamTransport::sequence(vec![
        error_response(first_error),
        error_response(second_error),
        FakeStreamResp::Frames {
            status: 200,
            headers: BTreeMap::new(),
            frames: successful_bedrock_anthropic_frames(model),
        },
    ]);
    let service = make_adapter_for_protocol_with_transport(
        ProtocolFamily::BedrockClaude,
        ProviderId::BedrockClaude,
        "https://bedrock.example.test",
        "bedrock",
        model,
        transport.clone(),
    );
    let scope = crate::thinking_scope::ThinkingRecoveryScope::default();
    let mut request = LlmRequest::new(model).with_user_text("Hi");
    request.profile = Some("bedrock".into());
    request.execution.thinking_recovery_scope = Some(scope);
    request.execution.native_anthropic_request_context = Some(NativeAnthropicRequestContext {
        afk_mode: true,
        ..Default::default()
    });
    request.input.thinking = Some(lingxi_llm_client::protocol::ThinkingConfig {
        mode: Some(lingxi_llm_client::protocol::ThinkingMode::Adaptive),
        ..Default::default()
    });

    let mut stream = service
        .stream_request(request)
        .await
        .expect("the Native AFK/TBC request is admitted");
    while let Some(event) = stream.next().await {
        event.expect("the final retry stream decodes");
    }
    transport
}
