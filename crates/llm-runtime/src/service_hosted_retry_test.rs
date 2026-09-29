//! The host's single-dispatch SDK usage must not reintroduce unsafe replay.

use super::*;
use crate::{
    AuthStrategy, BoxFuture, Capabilities, ClientConfig, ConnectionSpec, CredentialConfig,
    FailoverTriggers, FrameStream, ModelProfile, PricingConfig, ProtocolFamily, ProviderId,
    ProviderProfile, ProviderRequest, ProviderResponse, RawStreamFrame, StreamingResponse,
};
use futures::StreamExt;
use lingxi_llm_client::protocol as wire;
use lingxi_llm_client::providers::anthropic::{
    native::{AnthropicHostedTool, AnthropicRequestOptions},
    toolsets::AnthropicClientToolset,
};
use serde_json::json;
use std::collections::BTreeMap;

const MODEL: &str = "claude-sonnet-4-6";

#[derive(Clone, Copy)]
enum Failure {
    Transport,
    FileUploadOutcomeUnknown,
    Status(u16),
    Body,
}

struct ProbeTransport {
    failure: Failure,
    seen: Mutex<Vec<ProviderRequest>>,
}

impl ProbeTransport {
    fn new(failure: Failure) -> Arc<Self> {
        Arc::new(Self {
            failure,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn first(&self, request: &ProviderRequest) -> bool {
        let mut seen = self.seen.lock().unwrap();
        seen.push(request.clone());
        seen.len() == 1
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn connection_error() -> LlmError {
    LlmError::Transport {
        message: "connection lost after request dispatch".into(),
    }
}

fn response(status: u16) -> ProviderResponse {
    ProviderResponse {
        status,
        headers: BTreeMap::new(),
        request_id: None,
        body_json: if status >= 400 {
            json!({"type":"error","error":{"type":if status == 529 {"overloaded_error"} else {"api_error"},"message":"temporarily unavailable"}})
        } else {
            json!({"id":"msg_retry","model":MODEL,"content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})
        },
    }
}

struct Frames(VecDeque<Result<RawStreamFrame, LlmError>>);

impl FrameStream for Frames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let frame = self.0.pop_front().transpose();
        Box::pin(async move { frame })
    }
}

impl llm_runtime::test_support::FixtureTransport for ProbeTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let first = self.first(request);
        let result = match (first, self.failure) {
            (true, Failure::Transport | Failure::Body) => Err(connection_error()),
            (true, Failure::FileUploadOutcomeUnknown) => Err(LlmError::FileUploadOutcomeUnknown {
                message: "provider may have accepted the upload".into(),
            }),
            (true, Failure::Status(status)) => Ok(response(status)),
            _ => Ok(response(200)),
        };
        Box::pin(async move { result })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let first = self.first(request);
        let result = match (first, self.failure) {
            (true, Failure::Transport) => Err(connection_error()),
            (true, Failure::FileUploadOutcomeUnknown) => Err(LlmError::FileUploadOutcomeUnknown {
                message: "provider may have accepted the upload".into(),
            }),
            (true, Failure::Body) => Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(VecDeque::from([Err(connection_error())]))),
            }),
            (true, Failure::Status(status)) => Ok(StreamingResponse {
                status,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(VecDeque::from([Ok(RawStreamFrame::new(
                    serde_json::to_vec(&response(status).body_json).unwrap(),
                ))]))),
            }),
            _ => Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(VecDeque::from([
                    Ok(RawStreamFrame::new(serde_json::to_vec(&json!({"type":"message_start","message":{"id":"msg_retry","model":MODEL,"content":[],"usage":{"input_tokens":1,"output_tokens":0}}})).unwrap())),
                    Ok(RawStreamFrame::new(br#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}"#.to_vec())),
                    Ok(RawStreamFrame::new(br#"{"type":"message_stop"}"#.to_vec())),
                ]))),
            }),
        };
        Box::pin(async move { result })
    }
}
llm_runtime::impl_fixture_transport!(ProbeTransport);

fn service(transport: Arc<dyn Transport>, with_connection_failover: bool) -> ApiService {
    service_on(
        transport,
        with_connection_failover,
        ProtocolFamily::AnthropicMessages,
        MODEL,
    )
}

fn service_on(
    transport: Arc<dyn Transport>,
    with_connection_failover: bool,
    protocol: ProtocolFamily,
    model: &str,
) -> ApiService {
    let responses = protocol == ProtocolFamily::OpenAiResponses;
    let profile = |order| ProviderProfile {
        wire_profile: None,
        regions: wire::Region::all(),
        provider_id: if responses {
            ProviderId::OpenAI
        } else {
            ProviderId::AnthropicFirstParty
        },
        profile_name: format!("hosted-retry-{order}"),
        base_url: if responses {
            "https://api.openai.com/v1"
        } else {
            "https://api.anthropic.com"
        }
        .into(),
        protocol: protocol.clone(),
        auth: AuthStrategy::None,
        credential: CredentialConfig::None,
        models: vec![ModelProfile {
            display_model: model.into(),
            request_model: model.into(),
            billing_model: model.into(),
            aliases: Vec::new(),
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
        connection: ConnectionSpec {
            group: Some("hosted-retry".into()),
            connection_id: Some(format!("{order}")),
            order,
            failover: if with_connection_failover {
                FailoverTriggers::DEFAULT
            } else {
                FailoverTriggers::NONE
            },
            ..Default::default()
        },
    };
    let mut profiles = vec![profile(0)];
    if with_connection_failover {
        profiles.push(profile(1));
    }
    let client = Arc::new(
        ModelRuntime::from_config(ClientConfig {
            providers: profiles,
        })
        .unwrap(),
    );
    ApiService::new_with_routing(
        client,
        transport,
        SubscriberState::default(),
        UserAgentEnv::default(),
        "test",
        None,
        None,
        None,
        BTreeMap::new(),
        Some(2),
        Some(0),
    )
}

fn request(hosted: bool) -> LlmRequest {
    let mut request = LlmRequest::new(MODEL).with_user_text("Find the answer");
    request.input.max_tokens = Some(100);
    if hosted {
        request
            .input
            .hosted_tools
            .push(wire::HostedTool::WebSearch(Default::default()));
    }
    request
}

#[derive(Clone, Default)]
struct SettlementProbe(Arc<Mutex<Vec<&'static str>>>);

#[async_trait::async_trait]
impl crate::ModelAttemptHooks for SettlementProbe {
    async fn begin(
        &self,
        _: &platform_api::ModelAttemptContext,
        _: &LlmRequest,
        _: &crate::PreparedLlmCall,
    ) -> Result<Box<dyn crate::ModelAttemptLease>, LlmError> {
        self.0.lock().unwrap().push("begin");
        Ok(Box::new(self.clone()))
    }
}

impl crate::ModelAttemptLease for SettlementProbe {
    fn mark_dispatched(&mut self) -> Result<(), LlmError> {
        self.0.lock().unwrap().push("dispatched");
        Ok(())
    }

    fn observe_usage(
        &mut self,
        _: &crate::ExecutionUsage,
        _: crate::ModelAttemptUsageCompleteness,
    ) {
    }

    fn finish(self: Box<Self>) -> Box<dyn crate::ModelAttemptSettlement> {
        self.0.lock().unwrap().push("finish");
        self
    }
}

#[async_trait::async_trait]
impl crate::ModelAttemptSettlement for SettlementProbe {
    async fn wait(self: Box<Self>) -> Result<(), LlmError> {
        self.0.lock().unwrap().push("settled");
        Ok(())
    }
}

#[tokio::test]
async fn a_hosted_failure_settles_its_registered_attempt_before_returning() {
    for streaming in [false, true] {
        for failure in [Failure::Transport, Failure::Status(503), Failure::Body] {
            let transport = ProbeTransport::new(failure);
            let service = service(transport.clone(), true);
            let probe = SettlementProbe::default();
            service.set_model_attempt_hooks(Arc::new(probe.clone()));
            let mut request = request(true);
            request.execution.model_attempt = Some(
                platform_api::ModelAttemptRun::new(Arc::new(()))
                    .context(platform_api::ModelAttemptStage::Panel, Some(0))
                    .unwrap(),
            );
            if streaming {
                if let Ok(stream) = service.stream_request(request).await {
                    assert!(stream.collect::<Vec<_>>().await.iter().any(Result::is_err));
                }
            } else {
                assert!(service.execute_side_query_request(request).await.is_err());
            }
            assert_eq!(transport.count(), 1);
            assert_eq!(
                *probe.0.lock().unwrap(),
                ["begin", "dispatched", "finish", "settled"]
            );
        }
    }
}

#[tokio::test]
async fn hosted_nonstream_failures_never_retry_or_switch_connections() {
    for failure in [Failure::Transport, Failure::Status(503)] {
        for failover in [false, true] {
            let transport = ProbeTransport::new(failure);
            let result = service(transport.clone(), failover)
                .execute_side_query_request(request(true))
                .await;
            assert!(
                result.is_err(),
                "the uncertain first result must remain visible"
            );
            assert_eq!(
                transport.count(),
                1,
                "hosted work must only be dispatched once"
            );
        }
    }
}

#[tokio::test]
async fn hosted_stream_failures_never_retry_or_switch_connections() {
    for failure in [Failure::Transport, Failure::Status(503), Failure::Body] {
        for failover in [false, true] {
            let transport = ProbeTransport::new(failure);
            let result = service(transport.clone(), failover)
                .stream_request(request(true))
                .await;
            match result {
                Ok(stream) => assert!(stream.collect::<Vec<_>>().await.iter().any(Result::is_err)),
                Err(_) => {}
            }
            assert_eq!(
                transport.count(),
                1,
                "hosted stream must only be dispatched once"
            );
        }
    }
}

#[tokio::test]
async fn native_hosted_tool_failure_is_dispatched_once() {
    for streaming in [false, true] {
        for failure in [Failure::Transport, Failure::Status(503)] {
            let transport = ProbeTransport::new(failure);
            let service = service(transport.clone(), true);
            let mut request = request(false);
            request
                .input
                .hosted_tools
                .push(AnthropicHostedTool::WebFetch(Default::default()).into());
            if streaming {
                assert!(service.stream_request(request).await.is_err());
            } else {
                assert!(service.execute_side_query_request(request).await.is_err());
            }
            assert_eq!(transport.count(), 1);
            assert_eq!(
                transport.seen.lock().unwrap()[0].body_json["tools"][0]["name"],
                "web_fetch"
            );
        }
    }
}

fn client_toolset_options() -> wire::NativeExtension {
    wire::NativeExtension::from_typed(AnthropicRequestOptions {
        client_toolsets: vec![AnthropicClientToolset::Browser(Default::default())],
    })
    .unwrap()
}

#[tokio::test]
async fn native_client_toolsets_preserve_ordinary_retry() {
    const TOOLSET_MODEL: &str = "claude-sonnet-5";
    for streaming in [false, true] {
        let transport = ProbeTransport::new(Failure::Transport);
        let service = service_on(
            transport.clone(),
            false,
            ProtocolFamily::AnthropicMessages,
            TOOLSET_MODEL,
        );
        let mut request = LlmRequest::new(TOOLSET_MODEL).with_user_text("List open tabs");
        request.input.native_options.push(client_toolset_options());
        if streaming {
            let stream = service.stream_request(request).await.unwrap();
            assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
        } else {
            service.execute_side_query_request(request).await.unwrap();
        }
        assert_eq!(transport.count(), 2);
        assert_eq!(
            transport.seen.lock().unwrap()[0].body_json["tools"][0]["type"],
            "browser_toolset_20260801"
        );
    }
}

#[tokio::test]
async fn unknown_file_upload_outcome_is_never_retried_or_failed_over() {
    let error = LlmError::FileUploadOutcomeUnknown {
        message: "provider may have accepted the upload".into(),
    };
    assert_eq!(
        ApiService::error_kind(&error),
        "file_upload_outcome_unknown"
    );
    assert_eq!(ApiService::status_of(&error), None);
    for streaming in [false, true] {
        let transport = ProbeTransport::new(Failure::FileUploadOutcomeUnknown);
        let service = service(transport.clone(), true);
        if streaming {
            assert!(matches!(
                service.stream_request(request(false)).await,
                Err(LlmError::FileUploadOutcomeUnknown { .. })
            ));
        } else {
            assert!(matches!(
                service.execute_side_query_request(request(false)).await,
                Err(LlmError::FileUploadOutcomeUnknown { .. })
            ));
        }
        assert_eq!(transport.count(), 1);
    }
}

#[tokio::test]
async fn plain_chat_preserves_nonstream_and_stream_retry() {
    for streaming in [false, true] {
        for failover in [false, true] {
            let transport = ProbeTransport::new(Failure::Transport);
            let service = service(transport.clone(), failover);
            if streaming {
                let stream = service.stream_request(request(false)).await.unwrap();
                assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
            } else {
                service
                    .execute_side_query_request(request(false))
                    .await
                    .unwrap();
            }
            assert_eq!(transport.count(), 2);
        }
    }
}

#[tokio::test]
async fn hosted_overload_does_not_take_the_model_fallback_chain() {
    let transport = ProbeTransport::new(Failure::Status(529));
    let service = service(transport.clone(), false);
    let fallback = "fallback-must-not-be-prepared".to_owned();
    let result = service
        .drive_non_stream_seeded_with_chain(
            request(true),
            RetryControl {
                max_529_retries: 1,
                fallback_model: Some(fallback.clone()),
                allow_fallback: true,
                ..Default::default()
            },
            0,
            &[fallback],
            DispatchHeaderState::default(),
        )
        .await;
    assert!(matches!(result, Err(LlmError::Overloaded { .. })));
    assert_eq!(transport.count(), 1);
}

#[tokio::test]
async fn scoped_continuation_transport_failure_is_dispatched_once() {
    for streaming in [false, true] {
        let transport = ProbeTransport::new(Failure::Transport);
        let service = service_on(
            transport.clone(),
            true,
            ProtocolFamily::OpenAiResponses,
            "gpt-4.1",
        );
        let mut request = LlmRequest::new("gpt-4.1").with_user_text("Continue");
        request.execution.account_scope = Some("account".into());
        request.input.continuation = Some(wire::ContinuationRef {
            response_id: wire::ResponseId::new("resp_previous"),
            provider_id: wire::ProviderId::new("openai"),
            profile_name: "hosted-retry-0".into(),
            endpoint_fingerprint: lingxi_llm_client::files::provider_file_endpoint_fingerprint(
                "https://api.openai.com/v1",
            ),
            account_scope: "account".into(),
            request_model: "gpt-4.1".into(),
            workspace_id: None,
        });
        if streaming {
            assert!(service.stream_request(request).await.is_err());
        } else {
            assert!(service.execute_side_query_request(request).await.is_err());
        }
        assert_eq!(
            transport.count(),
            1,
            "continuation must reach its scoped route exactly once"
        );
        assert_eq!(
            transport.seen.lock().unwrap()[0].body_json["previous_response_id"],
            "resp_previous"
        );
    }
}

#[test]
fn native_execution_history_and_continuations_disable_replay() {
    let mut request = request(false);
    assert!(allows_automatic_replay(&request));
    for value in [
        json!({"type":"server_tool_use","name":"web_fetch"}),
        json!({"type":"mcp_tool_result"}),
        json!({"type":"bash_code_execution_tool_result"}),
        json!({"executableCode":{"code":"print(1)"}}),
        json!({"type":"tool_use","caller":{"type":"code_execution_20260521"}}),
        json!({"type":"tool_use","toolset_name":"browser"}),
    ] {
        request.input.messages[0].content = vec![wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::AnthropicMessages,
            value,
        }];
        assert!(!allows_automatic_replay(&request));
    }
    request.input.messages.clear();
    request.input.continuation = Some(wire::ContinuationRef {
        response_id: wire::ResponseId::new("resp_previous"),
        provider_id: wire::ProviderId::new("openai"),
        profile_name: "openai".into(),
        endpoint_fingerprint: "scope".into(),
        account_scope: "account".into(),
        request_model: "model".into(),
        workspace_id: None,
    });
    assert!(!allows_automatic_replay(&request));
}

#[test]
fn ordinary_native_reasoning_and_client_tool_metadata_preserve_retry() {
    let mut request = request(false);
    for value in [
        json!({"type":"reasoning","encrypted_content":"opaque"}),
        json!({"type":"chat_reasoning","reasoning_content":"thought"}),
        json!({"type":"text","text":"signed text","thought_signature":"signed"}),
        json!({"type":"tool_use","caller":{"type":"direct"}}),
        json!({"type":"text","text":"cited text","citations":[{"type":"web_search_result_location","url":"https://example.com"}]}),
    ] {
        request.input.messages[0].content = vec![wire::ContentBlock::ProviderContent {
            protocol: wire::ProtocolFamily::OpenAiResponses,
            value,
        }];
        assert!(allows_automatic_replay(&request));
    }
}

#[test]
fn durable_replay_companions_are_classified_after_canonical_conversion() {
    for (family, block, replayable) in [
        (
            wire::ProtocolFamily::GeminiGenerateContent,
            json!({"type":"text","text":"signed","thought_signature":"signature"}),
            true,
        ),
        (
            wire::ProtocolFamily::AnthropicMessages,
            json!({"type":"tool_use","id":"call","name":"lookup","input":{},"caller":{"type":"direct"}}),
            true,
        ),
        (
            wire::ProtocolFamily::AnthropicMessages,
            json!({"type":"tool_use","id":"call","name":"lookup","input":{},"caller":{"type":"code_execution_20260120"}}),
            false,
        ),
        (
            wire::ProtocolFamily::AnthropicMessages,
            json!({"type":"tool_use","id":"call","name":"click","input":{},"toolset_name":"browser"}),
            false,
        ),
        (
            wire::ProtocolFamily::AnthropicMessages,
            json!({"type":"provider_content","protocol":"anthropic_messages","value":{"type":"text","text":"cited","citations":[{"type":"web_search_result_location","url":"https://example.com"}]}}),
            true,
        ),
        (
            wire::ProtocolFamily::OpenAiResponses,
            json!({"type":"provider_content","protocol":"open_ai_responses","value":{"type":"reasoning","encrypted_content":"opaque"}}),
            true,
        ),
        (
            wire::ProtocolFamily::AnthropicMessages,
            json!({"type":"provider_content","protocol":"anthropic_messages","value":{"type":"mcp_tool_result","tool_use_id":"mcp1","content":[]}}),
            false,
        ),
    ] {
        let decoded = serde_json::from_value(json!({
            "model":MODEL, "message":{"role":"assistant","content":[block]},
            "stop_reason":"end_turn", "usage":wire::UsageReport::default(),
        }))
        .unwrap();
        let history = crate::history_projection::project_response(
            decoded,
            ProviderResponse::json(200, json!({})),
            family,
        )
        .unwrap();
        let (input, _) = crate::convert::history_input(
            MODEL,
            &[crate::Message {
                role: "assistant".into(),
                content: history.content,
            }],
            &[],
            &[],
            family,
        )
        .unwrap();
        assert!(
            !input.messages.is_empty(),
            "classification must see replayed input"
        );
        let mut request = request(false);
        request.input = input;
        assert_eq!(allows_automatic_replay(&request), replayable, "{block}");
    }
}

#[test]
fn durable_observations_are_removed_before_sdk_execution_classification() {
    let mut request = request(false);
    let (input, _) = crate::convert::history_input(
        MODEL,
        &[crate::Message {
            role: "assistant".into(),
            content: vec![crate::ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: json!({"type":"lingxi_observation","kind":"native_metadata","payload":{"container":{"id":"previous-container"}}}),
            }],
        }],
        &[], &[], wire::ProtocolFamily::AnthropicMessages,
    ).unwrap();
    request.input = input;
    assert!(request.input.messages.is_empty());
    assert!(allows_automatic_replay(&request));
}

#[test]
fn canonical_message_extensions_and_previous_response_id_disable_replay() {
    let mut request = request(false);
    request.input.controls.responses.previous_response_id = Some("previous".into());
    assert!(!allows_automatic_replay(&request));
    request.input.controls.responses.previous_response_id = None;
    request.input.messages[0].native_options.push(
        wire::NativeExtension::new("future.message_state.v1", json!({"thread":"previous"}))
            .unwrap(),
    );
    assert!(!allows_automatic_replay(&request));
}

#[test]
fn native_options_only_allow_known_client_toolset_policy_to_replay() {
    use wire::NativeType;
    let mut request = request(false);
    request.input.native_options.push(client_toolset_options());
    assert!(allows_automatic_replay(&request));
    for (format, data) in [
        (
            "future.conversation_state.v1",
            json!({"thread_id":"thread_previous"}),
        ),
        (
            "openrouter.container_metadata.v1",
            json!({"id":"container_previous"}),
        ),
        (
            AnthropicRequestOptions::FORMAT,
            json!({"client_toolsets":[],"container":"container_previous"}),
        ),
        (
            AnthropicRequestOptions::FORMAT,
            json!({"client_toolsets":42}),
        ),
    ] {
        request.input.native_options = vec![wire::NativeExtension::new(format, data).unwrap()];
        assert!(
            !allows_automatic_replay(&request),
            "unrecognized native state cannot authorize replay"
        );
    }
}
