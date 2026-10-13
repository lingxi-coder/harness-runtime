//! Captured-request tests for the Insights SideQuery producer.
//!
//! This module is referenced from `insights.rs` in the acceptance Runtime
//! packet. It exercises `InsightsSideQueryExecutor` through the real Session
//! backend and SDK `ProviderRequest`, without a network request.

use super::{
    InsightsModelSelector, InsightsNativeSideQueryContextProvider, InsightsSelectedModel,
    InsightsSideQueryExecutor, RuntimeInsightsNativeSideQueryContextProvider,
};
use async_trait::async_trait;
use lingxi_core::host::session_flags::effective_non_interactive_session;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::test_support::FixtureTransport;
use llm_runtime::{
    ApiService, BoxFuture, ClientConfig, LlmError, ModelRuntime, ProviderRequest, ProviderResponse,
    StreamingResponse,
};
use serde_json::{json, Value};
use sidequery::{
    NativeSideQueryPromptContext, ProviderSideQueryClient, QuerySource, SideQueryClient,
    SideQueryRequest,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::sync::{Arc, Mutex};

// Source oracle from Native bNn. LingXi intentionally maps only this identity
// block to its own product wording; the acceptance request must preserve the
// Native block's position between billing attribution and caller system text.
const NATIVE_RAW_INSIGHTS_AF: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
const BRAND_MAPPED_INSIGHTS_AF: &str = branding::NATIVE_AGENT_SDK_IDENTITY;
const VERTEX_INSIGHTS_HEADER: &str = orchestrator::prompt::locked_templates::HEADER;

struct FixedModelSelector;

#[async_trait]
impl InsightsModelSelector for FixedModelSelector {
    async fn selected_model(&self) -> Result<InsightsSelectedModel, Utf16JsonProjection> {
        Ok(InsightsSelectedModel {
            model: "claude-haiku-4-5".into(),
            profile: Some("insights-test".into()),
        })
    }
}

fn configured_native_prefix(
) -> lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix {
    lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix {
        compatibility_version: "2.1.295".into(),
        entrypoint: "insights-test".into(),
        host_identity: llm_runtime::PromptText::from_string("INSIGHTS_HOST_IDENTITY_SENTINEL"),
        attribution_enabled: true,
        assume_first_party_base_url: false,
    }
}

fn native_side_query_context_provider(
    api: Arc<ApiService>,
) -> Arc<dyn InsightsNativeSideQueryContextProvider> {
    Arc::new(RuntimeInsightsNativeSideQueryContextProvider {
        api,
        fallback_prefix: Some(configured_native_prefix()),
    })
}

#[derive(Debug, Clone)]
struct CapturedRequest {
    url: String,
    body_source: String,
    noninteractive: bool,
}

struct ScriptedTransport {
    requests: Mutex<Vec<CapturedRequest>>,
    responses: Mutex<VecDeque<ProviderResponse>>,
}

impl ScriptedTransport {
    fn new(responses: impl IntoIterator<Item = ProviderResponse>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into_iter().collect()),
        }
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl FixtureTransport for ScriptedTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        self.requests.lock().unwrap().push(CapturedRequest {
            url: request.url.clone(),
            body_source: request.body_json.as_json_source().to_owned(),
            noninteractive: effective_non_interactive_session(),
        });
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("fixture has a response for every physical request");
        Box::pin(async move { Ok(response) })
    }

    fn open_stream<'a>(
        &'a self,
        _: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async { panic!("Insights side queries use the non-streaming Session path") })
    }
}
llm_runtime::impl_fixture_transport!(ScriptedTransport);

#[derive(Clone, Copy, Debug)]
enum NativeAnthropicRoute {
    FirstParty,
    Bedrock,
    Vertex,
    Foundry,
}

impl NativeAnthropicRoute {
    fn config(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::FirstParty => (
                "anthropic_first_party",
                "anthropic_messages",
                "https://api.anthropic.com",
                "claude-haiku-4-5",
            ),
            Self::Bedrock => (
                "bedrock_claude",
                "bedrock_claude",
                "https://bedrock-runtime.us-east-1.amazonaws.com",
                "anthropic.claude-haiku-4-5-v1:0",
            ),
            Self::Vertex => (
                "vertex_claude",
                "vertex_claude",
                "https://us-central1-aiplatform.googleapis.com/v1/projects/p/locations/us-central1",
                "claude-haiku-4-5",
            ),
            Self::Foundry => (
                "foundry_claude",
                "foundry_claude",
                "https://foundry.fixture.invalid/anthropic/",
                "claude-haiku-4-5",
            ),
        }
    }

    fn expected_identity(self) -> &'static str {
        match self {
            Self::Vertex => VERTEX_INSIGHTS_HEADER,
            Self::FirstParty | Self::Bedrock | Self::Foundry => BRAND_MAPPED_INSIGHTS_AF,
        }
    }

    fn expects_checksum_placeholder(self) -> bool {
        // `NativeSystemPrefix::billing_header` emits Native Fmo's `cch=00000`
        // only for canonical Anthropic first-party Messages and Vertex Claude.
        matches!(self, Self::FirstParty | Self::Vertex)
    }

    fn assert_wire_route(self, request: &CapturedRequest) {
        match self {
            Self::FirstParty => assert!(request.url.contains("api.anthropic.com/v1/messages")),
            Self::Bedrock => assert!(
                request
                    .url
                    .contains("bedrock-runtime.us-east-1.amazonaws.com/model/")
                    && request.url.ends_with("/invoke"),
                "Bedrock request must use its model path: {}",
                request.url
            ),
            Self::Vertex => assert!(
                request
                    .url
                    .contains("/publishers/anthropic/models/claude-haiku-4-5:rawPredict"),
                "Vertex request must use its publisher route: {}",
                request.url
            ),
            Self::Foundry => assert!(
                request
                    .url
                    .contains("foundry.fixture.invalid/anthropic/v1/messages"),
                "Foundry request must use its configured Anthropic route: {}",
                request.url
            ),
        }
    }
}

fn insights_model_runtime_for(route: NativeAnthropicRoute) -> ModelRuntime {
    let (provider_id, protocol, base_url, request_model) = route.config();
    let config: ClientConfig = serde_json::from_value(json!({"providers":[{
        "provider_id":provider_id,
        "profile_name":"insights-test",
        "protocol":protocol,
        "base_url":base_url,
        "auth":"none",
        "credential":{"type":"none"},
        "models":[{
            "display_model":"claude-haiku-4-5",
            "request_model":request_model,
            "billing_model":"claude-haiku-4-5",
            "capabilities":{
                "streaming":true,
                "tools":true,
                "vision":false,
                "documents":false,
                "reasoning":true,
                "structured_output":false
            }
        }]
    }]}))
    .expect("fixture provider config is valid");
    ModelRuntime::from_config(config).expect("fixture model runtime is valid")
}

fn service(
    transport: Arc<ScriptedTransport>,
    analytics: Option<Arc<telemetry::AnalyticsBus>>,
    max_retries: Option<u32>,
) -> Arc<ApiService> {
    service_for_route(
        transport,
        analytics,
        max_retries,
        NativeAnthropicRoute::FirstParty,
    )
}

fn service_for_route(
    transport: Arc<ScriptedTransport>,
    analytics: Option<Arc<telemetry::AnalyticsBus>>,
    max_retries: Option<u32>,
    route: NativeAnthropicRoute,
) -> Arc<ApiService> {
    Arc::new(
        ApiService::new_with_routing(
            Arc::new(insights_model_runtime_for(route)),
            transport,
            Default::default(),
            Default::default(),
            "insights-query-test",
            analytics,
            None,
            None,
            Default::default(),
            max_retries,
            Some(0),
        )
        .with_native_system_prefix(configured_native_prefix()),
    )
}

fn side_query_client(service: Arc<ApiService>) -> Arc<dyn SideQueryClient> {
    Arc::new(ProviderSideQueryClient::from_service(service))
}

fn source_string(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::plain(json!(value))
}

fn success_response(text: &str) -> ProviderResponse {
    success_response_for("claude-haiku-4-5", text)
}

fn success_response_for(model: &str, text: &str) -> ProviderResponse {
    ProviderResponse::json(
        200,
        json!({
            "id":"insights-test",
            "type":"message",
            "role":"assistant",
            "model":model,
            "content":[{"type":"text","text":text}],
            "stop_reason":"end_turn",
            "usage":{"input_tokens":1,"output_tokens":2}
        }),
    )
}

fn no_tool_schemas(body: &Value) {
    match body.get("tools") {
        None | Some(Value::Null) => {}
        Some(Value::Array(tools)) if tools.is_empty() => {}
        other => panic!("expected no tool schemas, got {other:?}"),
    }
    assert!(body.get("tool_choice").is_none());
}

fn body_text(body: &Value, pointer: &str) -> String {
    body.pointer(pointer)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn assert_billing_attribution(body: &Value, expect_cch: bool) -> String {
    let system = body_text(body, "/system");
    assert!(
        system.contains("x-anthropic-billing-header: cc_version=2.1.295."),
        "ApiService's Native attribution prefix should remain on the SideQuery route: {system:?}"
    );
    assert!(
        system.contains("cc_entrypoint=insights-test;"),
        "the configured source entrypoint should be preserved: {system:?}"
    );
    assert_eq!(
        system.matches(" cch=00000;").count(),
        if expect_cch { 1 } else { 0 },
        "Native checksum placeholder presence is route-specific: {system:?}"
    );
    system
}

fn assert_insights_system_order(
    body: &Value,
    expected_identity: &str,
    expect_cch: bool,
    caller_text: Option<&str>,
) {
    let system = assert_billing_attribution(body, expect_cch);
    let billing_index = system
        .find("x-anthropic-billing-header: cc_version=2.1.295.")
        .expect("billing attribution exists");
    let identity_index = system
        .find(expected_identity)
        .expect("route-specific Insights identity is present");
    assert!(
        billing_index < identity_index,
        "billing must precede the route-specific Insights identity: {system:?}"
    );
    assert_eq!(
        system.matches(expected_identity).count(),
        1,
        "the route-specific Insights identity is emitted exactly once"
    );
    assert_ne!(
        NATIVE_RAW_INSIGHTS_AF, BRAND_MAPPED_INSIGHTS_AF,
        "keep the Native raw oracle distinct from the approved brand mapping"
    );
    assert!(
        !system.contains("INSIGHTS_HOST_IDENTITY_SENTINEL"),
        "Insights must use its source-mapped per-query identity, not the main-request sentinel"
    );
    if let Some(caller_text) = caller_text {
        let caller_index = system
            .find(caller_text)
            .expect("caller-provided system block survives");
        assert!(
            identity_index < caller_index,
            "the Native identity must precede caller system text: {system:?}"
        );
    }
}

fn assert_ordinary_sidequery_attribution_only(body: &Value, expect_cch: bool) {
    let system = assert_billing_attribution(body, expect_cch);
    assert!(
        !system.contains(BRAND_MAPPED_INSIGHTS_AF),
        "ordinary SideQueries must not receive the Insights identity context"
    );
    assert!(
        !system.contains(VERTEX_INSIGHTS_HEADER),
        "ordinary SideQueries must not receive the Vertex Insights identity context"
    );
    assert!(
        !system.contains("INSIGHTS_HOST_IDENTITY_SENTINEL"),
        "ordinary SideQueries remain attribution-only rather than adding main-request identity"
    );
}

fn insights_request(
    system_prompt: Option<Utf16JsonProjection>,
    native_prompt_context: Option<NativeSideQueryPromptContext>,
) -> SideQueryRequest {
    SideQueryRequest {
        enable_prompt_caching: false,
        model_attempt: None,
        native_prompt_context,
        model: "claude-haiku-4-5".into(),
        profile: Some("insights-test".into()),
        system_prompt,
        typed_attachment_ids: HashSet::new(),
        task_deliveries: HashMap::new(),
        system_prompt_input: None,
        messages: vec![ConversationMessage::user(
            MessageId::new(),
            "Insights test query".into(),
        )],
        tools: Vec::new(),
        tool_choice: None,
        output_format: None,
        max_tokens: 8192,
        max_retries: 0,
        temperature: None,
        thinking: Some(llm_runtime::model::thinking::ThinkingConfig::Disabled),
        effort: None,
        stop_sequences: Vec::new(),
        query_source: QuerySource::Custom("insights".into()),
        // Native Insights sets hasAppendSystemPrompt=false; that selects its
        // noninteractive prefix variant and does not suppress the service's
        // Native attribution prefix.
        skip_system_prompt_prefix: false,
    }
}

#[tokio::test]
async fn insights_executor_emits_native_caps_source_and_branded_system_context() {
    let transport = Arc::new(ScriptedTransport::new(
        ["facet", "summary", "section"].map(success_response),
    ));
    let sink = Arc::new(telemetry::InMemorySink::new());
    let analytics = Arc::new(telemetry::AnalyticsBus::new());
    analytics.attach_sink(sink.clone()).await;
    let service = service(transport.clone(), Some(analytics), Some(0));
    let executor = InsightsSideQueryExecutor::new(
        side_query_client(service.clone()),
        Arc::new(FixedModelSelector),
        native_side_query_context_provider(service),
    );

    for (cap, expected_text) in [(4096, "facet"), (500, "summary"), (8192, "section")] {
        let response = executor
            .query(source_string("Insights fixture prompt"), cap)
            .await
            .expect("fixture side query succeeds");
        assert_eq!(
            response.string_units(""),
            Some(expected_text.encode_utf16().collect())
        );
    }

    let requests = transport.requests();
    assert_eq!(requests.len(), 3);
    for (request, cap) in requests.iter().zip([4096, 500, 8192]) {
        assert!(request.noninteractive, "scope reaches actual SDK transport");
        let body: Value =
            serde_json::from_str(&request.body_source).expect("captured SDK body is valid JSON");
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["max_tokens"], cap);
        assert_eq!(
            body["messages"],
            json!([{"role":"user","content":"Insights fixture prompt"}])
        );
        assert_insights_system_order(&body, BRAND_MAPPED_INSIGHTS_AF, true, None);
        no_tool_schemas(&body);
        for key in [
            "response_format",
            "output_format",
            "output_config",
            "json_schema",
            "agents",
            "mcpTools",
            "mcp_tools",
            "agentContext",
            "agent_context",
            "hasAppendSystemPrompt",
        ] {
            assert!(body.get(key).is_none(), "unexpected `{key}` in {body}");
        }
        if let Some(thinking) = body.get("thinking") {
            assert_eq!(
                thinking.get("type").and_then(Value::as_str),
                Some("disabled"),
                "Insights must not inherit adaptive/enabled thinking: {body}"
            );
        }
        assert_ne!(body.get("stream"), Some(&json!(true)));
    }

    let events = sink.events().await;
    let query_source_events: Vec<_> = events
        .iter()
        .filter(|event| event.name == "tengu_api_query_source")
        .collect();
    assert_eq!(query_source_events.len(), 3);
    for event in query_source_events {
        assert!(matches!(
            event.metadata.get("querySource"),
            Some(telemetry::AnalyticsValue::String(source)) if source == "insights"
        ));
    }
}

#[tokio::test]
async fn insights_identity_uses_native_provider_route_mapping() {
    for route in [
        NativeAnthropicRoute::FirstParty,
        NativeAnthropicRoute::Bedrock,
        NativeAnthropicRoute::Vertex,
        NativeAnthropicRoute::Foundry,
    ] {
        let (_, _, _, response_model) = route.config();
        let transport = Arc::new(ScriptedTransport::new([success_response_for(
            response_model,
            "route mapped",
        )]));
        let api = service_for_route(transport.clone(), None, Some(0), route);
        let executor = InsightsSideQueryExecutor::new(
            side_query_client(api.clone()),
            Arc::new(FixedModelSelector),
            native_side_query_context_provider(api),
        );

        let response = executor
            .query(source_string("provider identity route fixture"), 4096)
            .await
            .expect("supported Anthropic Messages route succeeds");
        assert_eq!(
            response.string_units(""),
            Some("route mapped".encode_utf16().collect())
        );

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].noninteractive, "scope reaches SDK transport");
        route.assert_wire_route(&requests[0]);
        let body: Value = serde_json::from_str(&requests[0].body_source)
            .expect("captured provider request body is valid JSON");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(
            body["messages"],
            json!([{"role":"user","content":"provider identity route fixture"}])
        );
        assert_insights_system_order(
            &body,
            route.expected_identity(),
            route.expects_checksum_placeholder(),
            None,
        );
        no_tool_schemas(&body);
        if let Some(thinking) = body.get("thinking") {
            assert_eq!(
                thinking.get("type").and_then(Value::as_str),
                Some("disabled")
            );
        }
    }
}

struct RetryEnvironment(Option<OsString>);

impl RetryEnvironment {
    fn clear_override() -> Self {
        let previous = std::env::var_os(branding::MAX_RETRIES_ENV);
        std::env::remove_var(branding::MAX_RETRIES_ENV);
        Self(previous)
    }
}

impl Drop for RetryEnvironment {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var(branding::MAX_RETRIES_ENV, value),
            None => std::env::remove_var(branding::MAX_RETRIES_ENV),
        }
    }
}

#[tokio::test]
async fn insights_retry_uses_service_budget_not_sidequery_max_retries() {
    let _retry_environment = RetryEnvironment::clear_override();
    let transport = Arc::new(ScriptedTransport::new([
        ProviderResponse::json(
            500,
            json!({"type":"error","error":{"type":"api_error","message":"retry fixture"}}),
        ),
        success_response("recovered"),
    ]));
    let service = service(transport.clone(), None, Some(1));
    let executor = InsightsSideQueryExecutor::new(
        side_query_client(service.clone()),
        Arc::new(FixedModelSelector),
        native_side_query_context_provider(service.clone()),
    );

    let response = executor
        .query(source_string("retry fixture prompt"), 4096)
        .await
        .expect("500 is retried by ApiService and the second response succeeds");
    assert_eq!(
        response.string_units(""),
        Some("recovered".encode_utf16().collect())
    );
    assert_eq!(service.last_retry_count(), 1);
    let requests = transport.requests();
    assert_eq!(
        requests.len(),
        2,
        "one service-owned retry means two attempts"
    );
    assert!(requests.iter().all(|request| request.noninteractive));
    assert_eq!(requests[0].body_source, requests[1].body_source);
    let body: Value = serde_json::from_str(&requests[0].body_source).unwrap();
    assert_eq!(body["max_tokens"], 4096);
    assert_insights_system_order(&body, BRAND_MAPPED_INSIGHTS_AF, true, None);
}

#[tokio::test]
async fn insights_context_orders_identity_before_caller_and_plain_sidequery_stays_attribution_only()
{
    for route in [
        NativeAnthropicRoute::FirstParty,
        NativeAnthropicRoute::Bedrock,
        NativeAnthropicRoute::Vertex,
        NativeAnthropicRoute::Foundry,
    ] {
        let transport = Arc::new(ScriptedTransport::new([
            success_response("ordinary sidequery"),
            success_response("with prefix"),
            success_response("with caller system"),
        ]));
        let service = service_for_route(transport.clone(), None, Some(0), route);
        let provider = native_side_query_context_provider(service.clone());
        let native_context = provider
            .context_for_request(
                "claude-haiku-4-5",
                Some("insights-test"),
                &"Insights test query".encode_utf16().collect::<Vec<_>>(),
            )
            .await
            .expect("route facts resolve")
            .expect("supported Anthropic Messages route receives Insights context");
        let client = side_query_client(service);

        let mut ordinary = insights_request(None, None);
        ordinary.query_source = QuerySource::Custom("classifier".into());
        client
            .query(ordinary)
            .await
            .expect("ordinary SideQuery succeeds");
        client
            .query(insights_request(None, Some(native_context.clone())))
            .await
            .expect("Insights request without caller system succeeds");
        client
            .query(insights_request(
                Some(source_string("EXPLICIT_INSIGHTS_SYSTEM_SENTINEL")),
                Some(native_context),
            ))
            .await
            .expect("request with explicit system succeeds");

        let requests = transport.requests();
        assert_eq!(requests.len(), 3);
        route.assert_wire_route(&requests[1]);
        route.assert_wire_route(&requests[2]);
        let ordinary: Value = serde_json::from_str(&requests[0].body_source).unwrap();
        let native_only: Value = serde_json::from_str(&requests[1].body_source).unwrap();
        let explicit_system: Value = serde_json::from_str(&requests[2].body_source).unwrap();
        assert_ordinary_sidequery_attribution_only(&ordinary, route.expects_checksum_placeholder());
        assert_insights_system_order(
            &native_only,
            route.expected_identity(),
            route.expects_checksum_placeholder(),
            None,
        );
        assert_insights_system_order(
            &explicit_system,
            route.expected_identity(),
            route.expects_checksum_placeholder(),
            Some("EXPLICIT_INSIGHTS_SYSTEM_SENTINEL"),
        );
        assert!(!body_text(&native_only, "/system").contains("EXPLICIT_INSIGHTS_SYSTEM_SENTINEL"));
    }

    let host_context_json = serde_json::to_value(insights_request(None, None))
        .expect("SideQuery request is serializable");
    assert!(
        host_context_json.get("native_prompt_context").is_none(),
        "trusted prompt context is host-only and never serialized into the side-query DTO"
    );
}
