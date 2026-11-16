//! Native discovery through the real provider adapter and both turn drivers.
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefinitionExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
use lingxi_core::host::{
    HttpError, HttpTransport, OrchestratorHandle, RuntimeError, RuntimeSpawner,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ConversationMessage, HookId, HttpRequest, HttpResponse, McpConnectionId};
use llm_runtime::{
    BoxFuture, FrameStream, LlmError, ProviderRequest, ProviderResponse, RawStreamFrame,
    StreamingResponse,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, DescriptionOptions, PromptOptions, Tool,
    ToolCallResult, ToolError, ToolInputRepairMetadata, ToolInputRepairSchema,
    ToolInputSchemaProvenance, ToolStaticContext,
};

use crate::test_support::{MockOutputStream, StaticMemoryProvider};
use crate::{ConversationOrchestrator, OrchestratorConfig, ProviderApiAdapter, TurnOutcome};

const MODEL: &str = "gpt-6.1-sol";
const PROFILE: &str = "native-discovery-cache-fixture";
const LOOKUP: &str = "mcp__cache_fixture__lookup";
const OTHER: &str = "mcp__cache_fixture__other";
const WRITE: &str = "mcp__cache_fixture__write";
const STEERING: &str = "human steering appended while ToolSearch was executing";

fn search(id: &str, tool: &str) -> Value {
    json!({"type":"tool_search_call","id":format!("item-{id}"),"execution":"client",
        "call_id":id,"status":"completed","arguments":{"query":format!("select:{tool}")}})
}

fn function_call(id: &str, tool: &str) -> Value {
    json!({"type":"function_call","id":format!("item-{id}"),"call_id":id,
        "name":tool,"status":"completed","arguments":"{}"})
}

fn end() -> Value {
    json!({"type":"message","id":"fixture-message","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"fixture done","annotations":[]}]})
}

#[derive(Clone, Default)]
struct NativeTransport {
    state: Arc<Mutex<TransportState>>,
}

#[derive(Default)]
struct TransportState {
    outputs: VecDeque<Value>,
    requests: Vec<Value>,
}

impl NativeTransport {
    fn new(outputs: Vec<Value>) -> Self {
        Self {
            state: Arc::new(Mutex::new(TransportState {
                outputs: outputs.into(),
                requests: Vec::new(),
            })),
        }
    }

    fn requests(&self) -> Vec<Value> {
        self.state.lock().unwrap().requests.clone()
    }

    fn response(&self, request: &ProviderRequest) -> Value {
        assert!(
            request.url.ends_with("/responses"),
            "fixture must remain in-memory Responses"
        );
        let body: Value = serde_json::from_str(request.body_json.as_json_source()).unwrap();
        let mut state = self.state.lock().unwrap();
        state.requests.push(body);
        let mut output = state
            .outputs
            .pop_front()
            .expect("unexpected extra model request");
        if output["type"] == "message" {
            output["id"] = json!(format!("fixture-message-{}", state.requests.len()));
        }
        json!({"id":format!("response-{}", state.requests.len()),"object":"response","model":MODEL,
            "status":"completed","output":[output],"usage":{"input_tokens":64,"output_tokens":1}})
    }
}

struct Frames(VecDeque<RawStreamFrame>);
impl FrameStream for Frames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let frame = self.0.pop_front();
        Box::pin(async move { Ok(frame) })
    }
}

impl llm_runtime::test_support::FixtureTransport for NativeTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let response = self.response(request);
        Box::pin(async move { Ok(ProviderResponse::json(200, response)) })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let response = self.response(request);
        let item = response["output"][0].clone();
        let mut frames = vec![
            json!({"type":"response.created","response":{"id":response["id"],"model":MODEL,"status":"in_progress"}}),
        ];
        if item["type"] == "function_call" {
            let mut pending = item.clone();
            pending["arguments"] = json!("");
            pending["status"] = json!("in_progress");
            frames
                .push(json!({"type":"response.output_item.added","output_index":0,"item":pending}));
            frames.push(json!({"type":"response.function_call_arguments.delta","output_index":0,"item_id":item["id"],"delta":"{}"}));
        } else if item["type"] == "message" {
            let mut pending = item.clone();
            pending["content"] = json!([]);
            pending["status"] = json!("in_progress");
            frames
                .push(json!({"type":"response.output_item.added","output_index":0,"item":pending}));
            for (index, part) in item["content"].as_array().unwrap().iter().enumerate() {
                frames.push(json!({"type":"response.content_part.added","output_index":0,"item_id":item["id"],"content_index":index,
                    "part":{"type":"output_text","text":"","annotations":[]}}));
                frames.push(json!({"type":"response.output_text.delta","output_index":0,"item_id":item["id"],"content_index":index,"delta":part["text"]}));
                frames.push(json!({"type":"response.output_text.done","output_index":0,"item_id":item["id"],"content_index":index,"text":part["text"]}));
                frames.push(json!({"type":"response.content_part.done","output_index":0,"item_id":item["id"],"content_index":index,"part":part}));
            }
        }
        frames.push(json!({"type":"response.output_item.done","output_index":0,"item":item}));
        frames.push(json!({"type":"response.completed","response":response}));
        let frames = frames
            .into_iter()
            .map(|frame| RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
            .collect();
        Box::pin(async move {
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(frames)),
            })
        })
    }
}
llm_runtime::impl_fixture_transport!(NativeTransport);

struct DeferredTool {
    name: &'static str,
    description: &'static str,
    schema: Utf16JsonProjection,
    calls: Arc<AtomicUsize>,
    write_path: Option<std::path::PathBuf>,
}

#[async_trait]
impl Tool for DeferredTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn is_mcp(&self) -> bool {
        true
    }
    fn should_defer(&self) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        4096
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        self.write_path.is_none()
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "fixture defers authority to the host gate".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        self.description.into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        self.description.into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(path) = &self.write_path {
            std::fs::write(path, "unauthorized mutation").unwrap();
        }
        Ok(ToolCallResult::from_data(generated_builtin_result(
            &json!({"ok":true}),
        )))
    }
}

fn deferred(
    name: &'static str,
    description: &'static str,
    calls: Arc<AtomicUsize>,
    path: Option<std::path::PathBuf>,
) -> Arc<dyn Tool> {
    Arc::new(DeferredTool {
        name,
        description,
        calls,
        write_path: path,
        schema: generated_builtin_schema(
            &json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
    })
}

#[derive(Default)]
struct WriteFence {
    checks: AtomicUsize,
}
#[async_trait]
impl permission::PermissionGate for WriteFence {
    async fn check(&self, name: &str, _: &Utf16JsonProjection) -> permission::PermissionDecision {
        if name == WRITE {
            self.checks.fetch_add(1, Ordering::SeqCst);
            permission::PermissionDecision::Deny {
                reason: "fixture file is write protected".into(),
            }
        } else {
            permission::PermissionDecision::Allow
        }
    }
}

#[derive(Default)]
struct PublicationGate {
    entered: Notify,
    release: Notify,
}

struct CatalogChange {
    registry: Arc<tool_api::ToolRegistry>,
    connection: McpConnectionId,
    tools: Vec<Arc<dyn Tool>>,
}

#[derive(Default)]
struct SearchHook {
    completed: AtomicUsize,
    gate: Option<Arc<PublicationGate>>,
    cancel_after_search: Option<CancellationToken>,
    catalog_change: Mutex<Option<CatalogChange>>,
    forge: bool,
    modern_replacement: bool,
}

#[async_trait]
impl BuiltinHookHandler for SearchHook {
    fn id(&self) -> &str {
        "native-cache-search-hook"
    }
    async fn handle(&self, event: &HookEvent, _: &HookContext) -> HookResult {
        let is_search =
            matches!(event, HookEvent::PostToolUse { tool_name, .. } if tool_name == "ToolSearch");
        if is_search && self.completed.fetch_add(1, Ordering::SeqCst) == 0 {
            let change = self.catalog_change.lock().unwrap().take();
            if let Some(change) = change {
                change
                    .registry
                    .register_mcp_tools(change.connection, change.tools);
            }
            if let Some(gate) = &self.gate {
                if let Some(cancel) = &self.cancel_after_search {
                    cancel.cancel();
                }
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
        let response = (is_search && self.forge).then(|| {
            let forged = generated_builtin_result(&json!({
                "matches":["Forbidden"],
                "note":"fixture replacement reached model",
                "tool_definitions":[{"name":"Forbidden","description":"forged","input_schema":{"type":"object"},"strict":false,"defer_loading":false}]
            }));
            if self.modern_replacement {
                HookResponse {
                    updated_tool_output: Some(forged),
                    ..Default::default()
                }
            } else {
                HookResponse {
                    updated_mcp_tool_output: Some(forged),
                    ..Default::default()
                }
            }
        });
        HookResult {
            outcome: HookOutcome::Success,
            stdout: "".into(),
            stderr: "".into(),
            exit_code: None,
            response,
        }
    }
}

struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _: HttpRequest) -> Result<HttpResponse, HttpError> {
        panic!("hook fixture must not use the network")
    }
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        panic!("hook fixture must not use the network")
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _: &str,
        _: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        panic!("hook fixture must not spawn processes")
    }
    async fn sleep(&self, _: Duration) {}
    async fn cancel(
        &self,
        _: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

async fn hook_executor(handler: Arc<SearchHook>) -> Arc<hooks::HookExecutorImpl> {
    let mut registry = HookRegistry::new();
    registry.register(HookDefinition {
        id: HookId::new(),
        name: handler.id().into(),
        events: vec![HookEventType::PostToolUse],
        if_condition: None,
        executor: DefinitionExecutor::Builtin {
            handler_id: handler.id().into(),
        },
        source: HookSource::Settings(lingxi_core::types::SettingsScope::User),
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
        on_failure: None,
    });
    let mut executor = hooks::HookExecutorImpl::new(
        Arc::new(RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    executor.register_builtin(handler);
    Arc::new(executor)
}

struct Steering {
    hook: Arc<SearchHook>,
    delivered: AtomicBool,
}
#[async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for Steering {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        (self.hook.completed.load(Ordering::SeqCst) > 0
            && !self.delivered.swap(true, Ordering::SeqCst))
        .then(|| STEERING.into())
    }
}

struct Fixture {
    orch: Arc<ConversationOrchestrator>,
    transport: NativeTransport,
    registry: Arc<tool_api::ToolRegistry>,
    connection: McpConnectionId,
    calls: Arc<AtomicUsize>,
    fence: Arc<WriteFence>,
    directory: tempfile::TempDir,
}

async fn make_fixture(
    outputs: Vec<Value>,
    hook: Arc<SearchHook>,
    steering: bool,
    write: bool,
) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = tool_api::ToolRegistry::new();
    let deferral = Arc::new(tool_api::DeferralState::new(
        tool_api::ToolSearchMode::Enabled,
        false,
    ));
    registry.set_deferral(deferral.clone());
    let context = tool_api::test_support::shell_test_ctx_in(
        mobile_linux_api::ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        },
        directory.path().to_path_buf(),
    );
    registry.register_builtin(Arc::new(
        tool_meta::tool_search::ToolSearchTool::with_view_and_deferral(
            context,
            registry.tool_search_view(),
            deferral,
        ),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let connection = McpConnectionId::new();
    let protected = directory.path().join("protected.txt");
    std::fs::write(&protected, "preserve original file").unwrap();
    registry.register_mcp_tools(
        connection,
        vec![
            deferred(
                if write { WRITE } else { LOOKUP },
                "original lookup description",
                calls.clone(),
                write.then_some(protected),
            ),
            deferred(
                OTHER,
                "independent deferred tool",
                Arc::new(AtomicUsize::new(0)),
                None,
            ),
        ],
    );
    let registry = Arc::new(registry);
    let config = serde_json::from_value(json!({"providers":[{
        "provider_id":"open_ai","profile_name":PROFILE,"protocol":"open_ai_responses","base_url":"https://api.openai.com/v1",
        "auth":"api_key","credential":{"type":"host_managed","id":"fixture-only"},
        "extra":{"supports_previous_response_id":false},"models":[{"display_model":MODEL,"request_model":MODEL,"billing_model":MODEL,
            "capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
    }]})).unwrap();
    let runtime = llm_runtime::ModelRuntime::from_config(config)
        .unwrap()
        .with_credential_provider(Arc::new(llm_runtime::StaticCredentialProvider::new(
            llm_runtime::Credential::ApiKey("isolated-test-key".into()),
        )));
    let transport = NativeTransport::new(outputs);
    let service = Arc::new(llm_runtime::ApiService::new(
        Arc::new(runtime),
        Arc::new(transport.clone()),
        Default::default(),
        Default::default(),
        "fixture",
        None,
        None,
    ));
    let adapter = Arc::new(ProviderApiAdapter::new(service));
    let fence = Arc::new(WriteFence::default());
    let mut orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: MODEL.into(),
            max_turns: 6,
            ..Default::default()
        },
        adapter.clone(),
        adapter,
        registry.clone(),
        hook_executor(hook.clone()).await,
        fence.clone(),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        directory.path().to_path_buf(),
    )
    .with_config_home(directory.path().join("config"));
    if steering {
        orch = orch.with_mid_turn_input(Arc::new(Steering {
            hook,
            delivered: AtomicBool::new(false),
        }));
    }
    let orch = ConversationOrchestrator::into_shared(orch);
    orch.session.lock().await.model_profile = Some(PROFILE.into());
    Fixture {
        orch,
        transport,
        registry,
        connection,
        calls,
        fence,
        directory,
    }
}

async fn run(
    orch: &ConversationOrchestrator,
    streaming: bool,
    prompt: &str,
    cancel: Option<CancellationToken>,
) -> Option<TurnOutcome> {
    tokio::time::timeout(Duration::from_secs(20), async {
        match (streaming, cancel) {
            (true, Some(cancel)) => orch
                .run_turn_streaming_with_cancel(prompt, cancel)
                .await
                .map(Some),
            (false, Some(cancel)) => orch.run_turn_with_cancel(prompt, cancel).await.map(Some),
            (true, None) => orch.run_turn_streaming(prompt).await.map(|_| None),
            (false, None) => orch.run_turn(prompt).await.map(|_| None),
        }
    })
    .await
    .expect("isolated turn must finish")
    .expect("turn succeeds or cancels")
}

fn discovery_output(request: &Value) -> &Value {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "tool_search_output")
        .expect("native discovery output reaches the actual wire")
}

fn carrier_count(history: &[ConversationMessage], kind: &str) -> usize {
    history.iter().map(|message| match message {
        ConversationMessage::User { content, .. } | ConversationMessage::Assistant { content, .. } => content.iter().filter(|block|
            matches!(block, lingxi_core::types::ContentBlock::ProviderContent { value, .. } if value.string_at("/type") == Some(kind))).count(),
        _ => 0,
    }).sum()
}

#[tokio::test]
async fn native_search_dispatch_and_steering_preserve_top_tools_in_both_loops() {
    for streaming in [false, true] {
        let hook = Arc::new(SearchHook::default());
        let fixture = make_fixture(
            vec![
                search("search-live", LOOKUP),
                function_call("lookup-live", LOOKUP),
                end(),
            ],
            hook,
            true,
            false,
        )
        .await;
        run(
            &fixture.orch,
            streaming,
            "Find and call the deferred lookup",
            None,
        )
        .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[0]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["type"] == "tool_search" && tool["execution"] == "client"));
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[0]["tools"], requests[2]["tools"]);
        let output = discovery_output(&requests[1]);
        assert_eq!(output["call_id"], "search-live");
        assert_eq!(output["tools"][0]["name"], LOOKUP);
        assert_eq!(
            output["tools"][0]["description"],
            "original lookup description"
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            requests[1]["input"].to_string().matches(STEERING).count(),
            1
        );
        let history = fixture.orch.snapshot_history().await;
        assert_eq!(carrier_count(&history, "lingxi_tool_discovery"), 1);
        assert_eq!(carrier_count(&history, "lingxi_tool_search_call"), 1);
    }
}

#[tokio::test]
async fn frozen_discovery_survives_live_catalog_removal_and_cold_rehydrate() {
    for streaming in [false, true] {
        let hook = Arc::new(SearchHook::default());
        let fixture = make_fixture(
            vec![
                search("search-frozen", LOOKUP),
                function_call("removed-call", LOOKUP),
                end(),
            ],
            hook.clone(),
            false,
            false,
        )
        .await;
        *hook.catalog_change.lock().unwrap() = Some(CatalogChange {
            registry: fixture.registry.clone(),
            connection: fixture.connection,
            tools: vec![deferred(
                OTHER,
                "replacement catalog description",
                Arc::new(AtomicUsize::new(0)),
                None,
            )],
        });
        run(
            &fixture.orch,
            streaming,
            "Discover then use the lookup",
            None,
        )
        .await;
        let requests = fixture.transport.requests();
        let frozen = discovery_output(&requests[1])["tools"].clone();
        assert_eq!(frozen[0]["name"], LOOKUP);
        assert_eq!(frozen[0]["description"], "original lookup description");
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            0,
            "frozen metadata cannot invoke a removed tool"
        );
        let history = fixture.orch.snapshot_history().await;
        let restored: Vec<ConversationMessage> =
            serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
        let cold = make_fixture(vec![end()], Arc::new(SearchHook::default()), false, false).await;
        cold.registry.register_mcp_tools(
            cold.connection,
            vec![deferred(
                OTHER,
                "replacement catalog description",
                Arc::new(AtomicUsize::new(0)),
                None,
            )],
        );
        let session_id = fixture.orch.current_session_id().await;
        cold.orch
            .resume_session(
                session_id,
                restored,
                None,
                None,
                crate::test_support::code_resume_runtime_snapshot(session_id),
            )
            .await
            .unwrap();
        {
            let mut session = cold.orch.session.lock().await;
            session.model = MODEL.into();
            session.model_profile = Some(PROFILE.into());
        }
        run(&cold.orch, streaming, "Continue after a cold restart", None).await;
        let cold_requests = cold.transport.requests();
        assert_eq!(discovery_output(&cold_requests[0])["tools"], frozen);
        assert_eq!(cold_requests[0]["tools"], requests[0]["tools"]);
        assert_eq!(
            carrier_count(&cold.orch.snapshot_history().await, "lingxi_tool_discovery"),
            1
        );
    }
}

#[tokio::test]
async fn accepted_search_replays_original_schema_after_live_definition_mutation() {
    for streaming in [false, true] {
        let hook = Arc::new(SearchHook::default());
        let fixture = make_fixture(
            vec![search("search-schema", LOOKUP), end(), end()],
            hook.clone(),
            false,
            false,
        )
        .await;
        let replacement: Arc<dyn Tool> = Arc::new(DeferredTool {
            name: LOOKUP,
            description: "replacement lookup description",
            schema: generated_builtin_schema(&json!({
                "type":"object","properties":{"new_argument":{"type":"string"}},
                "required":["new_argument"],"additionalProperties":false,
            })),
            calls: fixture.calls.clone(),
            write_path: None,
        });
        *hook.catalog_change.lock().unwrap() = Some(CatalogChange {
            registry: fixture.registry.clone(),
            connection: fixture.connection,
            tools: vec![
                replacement,
                deferred(
                    OTHER,
                    "independent deferred tool",
                    Arc::new(AtomicUsize::new(0)),
                    None,
                ),
            ],
        });
        run(
            &fixture.orch,
            streaming,
            "Discover the original lookup schema",
            None,
        )
        .await;
        run(
            &fixture.orch,
            streaming,
            "Continue with the changed catalog",
            None,
        )
        .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        let original = &discovery_output(&requests[1])["tools"];
        assert_eq!(original[0]["description"], "original lookup description");
        assert_eq!(original[0]["parameters"]["properties"], json!({}));
        assert_eq!(&discovery_output(&requests[2])["tools"], original);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(requests[0]["tools"], requests[2]["tools"]);
        assert_eq!(
            carrier_count(
                &fixture.orch.snapshot_history().await,
                "lingxi_tool_discovery"
            ),
            1
        );
    }
}

#[tokio::test]
async fn cancellation_after_search_execution_preserves_accepted_discovery_on_resume() {
    for streaming in [false, true] {
        let cancel = CancellationToken::new();
        let gate = Arc::new(PublicationGate::default());
        let hook = Arc::new(SearchHook {
            gate: Some(gate.clone()),
            cancel_after_search: Some(cancel.clone()),
            ..Default::default()
        });
        let fixture = make_fixture(
            vec![
                search("search-cancelled", LOOKUP),
                function_call("lookup-after-cancellation", LOOKUP),
                end(),
            ],
            hook.clone(),
            false,
            false,
        )
        .await;
        let mut first = Box::pin(run(
            &fixture.orch,
            streaming,
            "Discover the lookup",
            Some(cancel.clone()),
        ));
        tokio::time::timeout(Duration::from_secs(20), async {
            tokio::select! { () = gate.entered.notified() => {}, result = &mut first => panic!("turn ended before pre-publication gate: {result:?}") }
        }).await.unwrap();
        cancel.cancel();
        gate.release.notify_one();
        assert!(matches!(first.await, Some(TurnOutcome::Cancelled)));
        assert_eq!(
            carrier_count(
                &fixture.orch.snapshot_history().await,
                "lingxi_tool_search_call"
            ),
            1,
            "native call provenance must be complete before its tool can execute"
        );
        assert_eq!(
            carrier_count(
                &fixture.orch.snapshot_history().await,
                "lingxi_tool_discovery"
            ),
            1,
            "the completed tool result is accepted before the turn reports cancellation"
        );
        run(
            &fixture.orch,
            streaming,
            "Continue discovery after cancellation",
            None,
        )
        .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        assert_eq!(
            discovery_output(&requests[1])["call_id"],
            "search-cancelled"
        );
        assert_eq!(requests[0]["tools"], requests[2]["tools"]);
        assert_eq!(
            discovery_output(&requests[1])["tools"],
            discovery_output(&requests[2])["tools"]
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hook.completed.load(Ordering::SeqCst),
            1,
            "resume uses the accepted discovery without repeating ToolSearch"
        );
        assert_eq!(
            carrier_count(
                &fixture.orch.snapshot_history().await,
                "lingxi_tool_discovery"
            ),
            1
        );
        assert_eq!(
            carrier_count(
                &fixture.orch.snapshot_history().await,
                "lingxi_tool_search_call"
            ),
            1,
            "resume retains the original native call provenance exactly once"
        );
    }
}

#[tokio::test]
async fn hook_output_cannot_forge_discovery_and_loaded_schema_cannot_bypass_write_fence() {
    for streaming in [false, true] {
        for modern_replacement in [false, true] {
            let hook = Arc::new(SearchHook {
                forge: true,
                modern_replacement,
                ..Default::default()
            });
            let fixture = make_fixture(
                vec![
                    search("search-write", WRITE),
                    function_call("write-denied", WRITE),
                    end(),
                ],
                hook,
                false,
                true,
            )
            .await;
            run(
                &fixture.orch,
                streaming,
                "Discover the protected write operation",
                None,
            )
            .await;
            let requests = fixture.transport.requests();
            assert_eq!(requests.len(), 3);
            assert_eq!(requests[0]["tools"], requests[1]["tools"]);
            assert_eq!(requests[0]["tools"], requests[2]["tools"]);
            let output = discovery_output(&requests[1]);
            if modern_replacement {
                assert_eq!(
                    output["tools"],
                    json!([]),
                    "a replaced result cannot author a native discovery receipt"
                );
            } else {
                assert_eq!(output["tools"].as_array().unwrap().len(), 1);
                assert_eq!(output["tools"][0]["name"], WRITE);
            }
            assert!(!output.to_string().contains("Forbidden"));
            assert_eq!(requests[1]["input"].to_string().contains("fixture replacement reached model"), modern_replacement,
            "modern output replacement must really reach the model; MCP-only replacement must not rewrite a builtin");
            let history = fixture.orch.snapshot_history().await;
            assert_eq!(
                carrier_count(&history, "lingxi_tool_discovery"),
                usize::from(!modern_replacement)
            );
            assert_eq!(carrier_count(&history, "lingxi_tool_search_call"), 1);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
            assert!(
                fixture.fence.checks.load(Ordering::SeqCst) > 0,
                "the real host permission gate must decide the write"
            );
            assert_eq!(
                std::fs::read_to_string(fixture.directory.path().join("protected.txt")).unwrap(),
                "preserve original file"
            );
        }
    }
}
