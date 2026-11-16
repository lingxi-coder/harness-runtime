//! Final Responses-wire regressions for the Inline date notification lane.
//!
//! Announced routing is an explicit control: it already uses durable date
//! attachments and must not start emitting the legacy Inline reminder.

use super::*;
use crate::test_support::{noop_hook_executor, NoOpPermissionGate, StaticMemoryProvider};
use crate::{OrchestratorConfig, ProviderApiAdapter, TurnOutcome};
use async_trait::async_trait;
use lingxi_core::host::instructions::InstructionRendering;
use lingxi_core::host::{
    BudgetError, FileSystem, OutputStream, WorkflowOutputAccount, WorkflowOutputEventId,
    WorkflowOutputScope, WorkflowOutputScopes,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock, ConversationMessage, MessageId, SessionId, ToolUseId};
use llm_runtime::{
    BoxFuture, FrameStream, LlmError, ProviderRequest, ProviderResponse, RawStreamFrame,
    StreamingResponse,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    generated_builtin_schema, DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};

const MODEL: &str = "gpt-6.1-sol";
const PROFILE: &str = "date-cache-wire-fixture";
const DATE_TEXT: &str = "The date has changed. Today's date is now";
const STEERING: &str = "date-cache steering received during the running request";

#[derive(Default)]
struct ResponseGate {
    armed: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl ResponseGate {
    async fn pause_if_armed(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

#[derive(Clone, Default)]
struct WireTransport {
    requests: Arc<StdMutex<Vec<Value>>>,
    response_gate: Arc<ResponseGate>,
    search_next: Arc<AtomicBool>,
}

impl WireTransport {
    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }

    fn response(&self, request: &ProviderRequest) -> Value {
        assert!(request.url.ends_with("/responses"));
        let body: Value = serde_json::from_str(request.body_json.as_json_source()).unwrap();
        let mut requests = self.requests.lock().unwrap();
        requests.push(body);
        let index = requests.len();
        let output = if self.search_next.swap(false, Ordering::SeqCst) {
            assert_eq!(
                requests[index - 1]["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|tool| {
                        tool["type"] == "tool_search" && tool["execution"] == "client"
                    })
                    .count(),
                1,
                "a fixture search response requires advertised native client discovery"
            );
            json!({"type":"tool_search_call", "id":format!("date-search-item-{index}"), "execution":"client",
                "call_id":format!("date-search-call-{index}"), "status":"completed",
                "arguments":{"query":"select:missing_date_fixture_tool"}})
        } else {
            json!({"type":"message", "id":format!("date-message-{index}"),
                "role":"assistant", "status":"completed",
                "content":[{"type":"output_text", "text":"date fixture done", "annotations":[]}]})
        };
        json!({
            "id":format!("date-response-{index}"), "object":"response", "model":MODEL,
            "status":"completed",
            "output":[output],
            "usage":{"input_tokens":12000, "output_tokens":3}
        })
    }
}

struct Frames(VecDeque<RawStreamFrame>);

impl FrameStream for Frames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let frame = self.0.pop_front();
        Box::pin(async move { Ok(frame) })
    }
}

impl llm_runtime::test_support::FixtureTransport for WireTransport {
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        let response = self.response(request);
        Box::pin(async move {
            self.response_gate.pause_if_armed().await;
            Ok(ProviderResponse::json(200, response))
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        let response = self.response(request);
        Box::pin(async move {
            self.response_gate.pause_if_armed().await;
            let item = response["output"][0].clone();
            let part = item["content"][0].clone();
            let mut pending = item.clone();
            pending["status"] = json!("in_progress");
            pending["content"] = json!([]);
            let mut frames = vec![
                json!({"type":"response.created", "response":{"id":response["id"], "model":MODEL, "status":"in_progress"}}),
            ];
            if item["type"] == "message" {
                frames.extend([
                    json!({"type":"response.output_item.added", "output_index":0, "item":pending}),
                    json!({"type":"response.content_part.added", "output_index":0, "item_id":item["id"], "content_index":0,
                    "part":{"type":"output_text", "text":"", "annotations":[]}}),
                    json!({"type":"response.output_text.delta", "output_index":0, "item_id":item["id"], "content_index":0, "delta":part["text"]}),
                    json!({"type":"response.output_text.done", "output_index":0, "item_id":item["id"], "content_index":0, "text":part["text"]}),
                    json!({"type":"response.content_part.done", "output_index":0, "item_id":item["id"], "content_index":0, "part":part}),
                ]);
            }
            frames.extend([
                json!({"type":"response.output_item.done", "output_index":0, "item":item}),
                json!({"type":"response.completed", "response":response}),
            ]);
            let frames = frames
                .into_iter()
                .map(|frame| RawStreamFrame::new(serde_json::to_vec(&frame).unwrap()))
                .collect();
            Ok(StreamingResponse {
                status: 200,
                headers: BTreeMap::new(),
                frames: Box::new(Frames(frames)),
            })
        })
    }
}

llm_runtime::impl_fixture_transport!(WireTransport);

#[derive(Default)]
struct SteeringQueue {
    pending: StdMutex<Option<Utf16JsonProjection>>,
    takes: AtomicUsize,
    consumed: AtomicUsize,
}

/// A real deferred candidate keeps the host's ToolSearch route available. The
/// response deliberately searches for a different, missing name, so this tool
/// must never execute and the native discovery result is legitimately empty.
struct DeferredDateCandidate(Utf16JsonProjection);

#[async_trait]
impl Tool for DeferredDateCandidate {
    fn name(&self) -> &str {
        "date_fixture_deferred_candidate"
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.0
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

    fn should_defer(&self) -> bool {
        true
    }

    fn max_result_size_chars(&self) -> usize {
        4096
    }

    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        true
    }

    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        true
    }

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        panic!("the unselected deferred candidate must not reach permission admission")
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Unselected deferred date fixture candidate".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Unselected deferred date fixture candidate".into()
    }

    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        panic!("the unselected deferred candidate must never execute")
    }
}

#[async_trait]
impl crate::prompt::mid_turn_input::MidTurnInputSource for SteeringQueue {
    fn admits_at(&self, point: crate::prompt::mid_turn_input::MidTurnInputPoint) -> bool {
        // This fixture runs the cancelable Complete entry, which deliberately
        // does not drain LoopStart input. Both drivers admit SDK human input
        // after the native search has settled.
        point == crate::prompt::mid_turn_input::MidTurnInputPoint::AfterTools
    }

    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        self.takes.fetch_add(1, Ordering::SeqCst);
        let input = self.pending.lock().unwrap().take();
        if input.is_some() {
            self.consumed.fetch_add(1, Ordering::SeqCst);
        }
        input
    }
}

#[derive(Default)]
struct DateOutputSink {
    search_results: AtomicUsize,
    cancel_after_search_result: StdMutex<Option<CancellationToken>>,
    cancelled_results: AtomicUsize,
}

#[async_trait]
impl OutputStream for DateOutputSink {
    async fn emit_text(&self, _: &Utf16JsonProjection) {}

    async fn emit_tool_call(&self, _: &ToolUseId, _: &str, _: &Utf16JsonProjection) {}

    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        tool: &str,
        _: &Utf16JsonProjection,
        _: &Utf16JsonProjection,
        _: Option<&Utf16JsonProjection>,
    ) {
        if tool == "ToolSearch" {
            self.search_results.fetch_add(1, Ordering::SeqCst);
            if let Some(cancel) = self.cancel_after_search_result.lock().unwrap().take() {
                self.cancelled_results.fetch_add(1, Ordering::SeqCst);
                cancel.cancel();
            }
        }
    }

    async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
}

struct OutputAccount {
    session: SessionId,
    generation: MessageId,
}

impl WorkflowOutputAccount for OutputAccount {
    fn session_id(&self) -> SessionId {
        self.session
    }
    fn generation_id(&self) -> MessageId {
        self.generation
    }
    fn spent(&self) -> u64 {
        0
    }
    fn record_legacy(&self, _: WorkflowOutputEventId, _: u64) -> Result<(), BudgetError> {
        Ok(())
    }
}

#[derive(Default)]
struct CaptureScopes {
    current: StdMutex<Option<WorkflowOutputScope>>,
    cancel_on_capture: StdMutex<Option<CancellationToken>>,
    cancelled_captures: AtomicUsize,
    reject_with_cancel: StdMutex<Option<CancellationToken>>,
    rejected: AtomicBool,
}

#[async_trait]
impl WorkflowOutputScopes for CaptureScopes {
    async fn ensure_current(
        &self,
        session: SessionId,
        generation: MessageId,
        max: Option<u64>,
    ) -> Result<WorkflowOutputScope, BudgetError> {
        if let Some(scope) = self.current.lock().unwrap().clone() {
            return Ok(scope);
        }
        self.begin_turn(session, generation, max).await
    }

    async fn begin_turn(
        &self,
        session: SessionId,
        generation: MessageId,
        _: Option<u64>,
    ) -> Result<WorkflowOutputScope, BudgetError> {
        let scope = WorkflowOutputScope::new(Arc::new(OutputAccount {
            session,
            generation,
        }));
        *self.current.lock().unwrap() = Some(scope.clone());
        Ok(scope)
    }

    fn capture(&self, session: SessionId) -> Result<WorkflowOutputScope, BudgetError> {
        // Both drivers call this after preparing context and before provider
        // admission. Unlike the rejection seam below, return a valid scope so
        // the ordinary Code cancellation gate must stop physical dispatch.
        if let Some(cancel) = self.cancel_on_capture.lock().unwrap().take() {
            self.cancelled_captures.fetch_add(1, Ordering::SeqCst);
            cancel.cancel();
        }
        if let Some(cancel) = self.reject_with_cancel.lock().unwrap().take() {
            self.rejected.store(true, Ordering::SeqCst);
            cancel.cancel();
            return Err(BudgetError::Internal(
                "date fixture capture rejected".into(),
            ));
        }
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|scope| scope.session_id() == session)
            .cloned()
            .ok_or_else(|| BudgetError::Internal("date fixture has no output scope".into()))
    }
}

fn durable_writer(
    fs: Arc<dyn FileSystem>,
    transcript: &Path,
    session_id: SessionId,
    cwd: &Path,
    home: &Path,
) -> Arc<session::JsonlWriter> {
    let authority = home.join("writer-authority");
    std::fs::create_dir_all(&authority).unwrap();
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(authority).unwrap());
    let writer =
        session::JsonlWriter::new(transcript.to_path_buf(), fs).with_durable_lock(durable.clone());
    let source = transcript.exists().then(|| {
        session::jsonl::writer::TranscriptFileAuthority::capture(
            transcript.parent().unwrap().to_path_buf(),
            transcript.file_name().unwrap().into(),
        )
        .unwrap()
        .0
    });
    writer
        .activate_session_target_with_durable_lock_and_authority(
            session_id,
            transcript.to_path_buf(),
            cwd.to_path_buf(),
            durable,
            source,
        )
        .unwrap();
    Arc::new(writer)
}

struct Fixture {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    transcript: PathBuf,
    fs: Arc<dyn FileSystem>,
    session_id: SessionId,
    config: OrchestratorConfig,
    adapter: Arc<ProviderApiAdapter>,
    tools: Arc<tool_api::ToolRegistry>,
    output: Arc<DateOutputSink>,
    capture_scopes: Arc<CaptureScopes>,
    steering: Arc<SteeringQueue>,
    transport: WireTransport,
    orch: Arc<ConversationOrchestrator>,
}

impl Fixture {
    async fn new(rendering: InstructionRendering) -> Self {
        Self::new_with_running_search(rendering, false).await
    }

    async fn new_with_running_search(
        rendering: InstructionRendering,
        running_search: bool,
    ) -> Self {
        Self::new_with_turn_limit(rendering, running_search, 6).await
    }

    async fn new_with_turn_limit(
        rendering: InstructionRendering,
        running_search: bool,
        max_turns: u32,
    ) -> Self {
        let root = tempfile::tempdir().unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let cwd = canonical.join("repo");
        let home = canonical.join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(canonical));
        let session_id = SessionId::new();
        let transcript = session::jsonl::path::session_path(
            &home,
            &cwd.to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let providers = serde_json::from_value(json!({"providers":[{
            "provider_id":"open_ai", "profile_name":PROFILE,
            "protocol":"open_ai_responses", "base_url":"https://api.openai.com/v1",
            "auth":"api_key", "credential":{"type":"host_managed", "id":"date-test-only"},
            "extra":{"supports_previous_response_id":false},
            "models":[{"display_model":MODEL, "request_model":MODEL, "billing_model":MODEL,
                "capabilities":{"streaming":true, "tools":true, "vision":false,
                    "documents":false, "reasoning":false, "structured_output":false}}]
        }]}))
        .unwrap();
        let runtime = llm_runtime::ModelRuntime::from_config(providers)
            .unwrap()
            .with_credential_provider(Arc::new(llm_runtime::StaticCredentialProvider::new(
                llm_runtime::Credential::ApiKey("isolated-date-fixture-key".into()),
            )));
        let transport = WireTransport::default();
        let service = Arc::new(llm_runtime::ApiService::new(
            Arc::new(runtime),
            Arc::new(transport.clone()),
            Default::default(),
            Default::default(),
            "date-cache-test",
            None,
            None,
        ));
        let adapter = Arc::new(ProviderApiAdapter::new(service));
        let output = Arc::new(DateOutputSink::default());
        let capture_scopes = Arc::new(CaptureScopes::default());
        let steering = Arc::new(SteeringQueue::default());
        let config = OrchestratorConfig {
            model: MODEL.into(),
            system_prompt_override: Some("Stable date cache fixture system.".into()),
            context_rendering: rendering,
            max_turns,
            ..Default::default()
        };
        let mut tools = tool_api::ToolRegistry::new();
        if running_search {
            let deferral = Arc::new(tool_api::DeferralState::new(
                tool_api::ToolSearchMode::Enabled,
                false,
            ));
            tools.set_deferral(deferral.clone());
            let context = tool_api::test_support::shell_test_ctx_in(
                mobile_linux_api::ProcessOutput {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: 0,
                    timed_out: false,
                },
                cwd.clone(),
            );
            tools.register_builtin(Arc::new(
                tool_meta::tool_search::ToolSearchTool::with_view_and_deferral(
                    context,
                    tools.tool_search_view(),
                    deferral,
                ),
            ));
            tools.register_builtin(Arc::new(DeferredDateCandidate(generated_builtin_schema(
                &json!({"type":"object", "properties":{}, "additionalProperties":false}),
            ))));
        }
        let tools = Arc::new(tools);
        let orch = ConversationOrchestrator::new_with_streaming(
            config.clone(),
            adapter.clone(),
            adapter.clone(),
            tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_session_id(session_id)
        .with_config_home(home.clone())
        .with_mid_turn_input(steering.clone())
        .with_workflow_output_scopes(capture_scopes.clone())
        .with_jsonl_writer(durable_writer(
            fs.clone(),
            &transcript,
            session_id,
            &cwd,
            &home,
        ));
        let orch = ConversationOrchestrator::into_shared(orch);
        orch.session.lock().await.model_profile = Some(PROFILE.into());
        orch.initialize_conversation_mode().await.unwrap();
        // Reuse the existing rollover seam: no wall-clock sleep or global
        // environment override, and no change to production date behavior.
        {
            let mut date = orch.prompt_runtime.date_change.lock().unwrap();
            date.session_id = Some(session_id);
            date.session_date = "2000-01-01".into();
            date.delivered_date = None;
        }
        orch.append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            format!(
                "date-cache long historical prefix: {}",
                "previous work ".repeat(4096)
            ),
        ))
        .await;
        orch.append_external_history_message(ConversationMessage::Assistant {
            per_turn_effort: None,
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: "historical answer before rollover".into(),
                citations: None,
            }],
            stop_reason: Some("end_turn".into()),
            resumable: None,
        })
        .await;
        Self {
            _root: root,
            cwd,
            home,
            transcript,
            fs,
            session_id,
            config,
            adapter,
            tools,
            output,
            capture_scopes,
            steering,
            transport,
            orch,
        }
    }

    async fn run(
        &self,
        streaming: bool,
        prompt: &str,
        cancel: Option<CancellationToken>,
    ) -> TurnOutcome {
        self.attempt(streaming, prompt, cancel)
            .await
            .expect("date fixture turn succeeds")
    }

    async fn attempt(
        &self,
        streaming: bool,
        prompt: &str,
        cancel: Option<CancellationToken>,
    ) -> Result<TurnOutcome, crate::OrchestratorError> {
        let cancel = cancel.unwrap_or_default();
        tokio::time::timeout(Duration::from_secs(15), async {
            if streaming {
                self.orch
                    .run_turn_streaming_with_cancel(prompt, cancel)
                    .await
            } else {
                self.orch.run_turn_with_cancel(prompt, cancel).await
            }
        })
        .await
        .expect("date fixture turn settles")
    }

    async fn run_with_steering_during_request(
        &self,
        streaming: bool,
        prompt: &str,
        cancel: Option<CancellationToken>,
    ) -> TurnOutcome {
        let gate = self.transport.response_gate.clone();
        gate.armed.store(true, Ordering::SeqCst);
        let mut running = Box::pin(self.run(streaming, prompt, cancel));
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::select! {
                () = gate.entered.notified() => {},
                result = &mut running => panic!("turn ended before its response gate: {result:?}"),
            }
        })
        .await
        .expect("initial physical request reaches the transport");
        assert!(self.steering.pending.lock().unwrap().is_none());
        *self.steering.pending.lock().unwrap() = Some(STEERING.into());
        gate.release.notify_one();
        running.await
    }

    async fn cold_resume(&mut self) {
        let mut cold = ConversationOrchestrator::with_resume(
            self.config.clone(),
            self.session_id.as_uuid(),
            self.home.clone(),
            self.cwd.to_string_lossy().into_owned(),
            self.fs.clone(),
            self.adapter.clone(),
            self.tools.clone(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            self.output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            self.cwd.clone(),
            Some(durable_writer(
                self.fs.clone(),
                &self.transcript,
                self.session_id,
                &self.cwd,
                &self.home,
            )),
        )
        .await
        .expect("cold resume the actual durable transcript")
        .with_config_home(self.home.clone())
        .with_mid_turn_input(self.steering.clone())
        .with_workflow_output_scopes(self.capture_scopes.clone());
        cold.streaming_api = self.adapter.clone();
        self.orch = ConversationOrchestrator::into_shared(cold);
        self.orch.session.lock().await.model_profile = Some(PROFILE.into());
    }

    fn diagnostics(&self) -> String {
        let request_shapes = self
            .transport
            .requests()
            .iter()
            .map(|request| {
                (
                    count_input_items(request, "tool_search_call"),
                    count_input_items(request, "tool_search_output"),
                    count_input_items(request, "function_call"),
                    count_input_items(request, "message"),
                )
            })
            .collect::<Vec<_>>();
        format!(
            "wire input counts(search,search_result,function,message)={request_shapes:?}; queue takes={}, consumed={}, pending={}; search result frames={}",
            self.steering.takes.load(Ordering::SeqCst),
            self.steering.consumed.load(Ordering::SeqCst),
            self.steering.pending.lock().unwrap().is_some(),
            self.output.search_results.load(Ordering::SeqCst),
        )
    }
}

fn count_text(request: &Value, needle: &str) -> usize {
    serde_json::to_string(&request["input"])
        .unwrap()
        .matches(needle)
        .count()
}

fn count_input_items(request: &Value, item_type: &str) -> usize {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == item_type)
        .count()
}

fn assert_input_prefix(before: &Value, after: &Value) {
    assert!(
        before["instructions"] == after["instructions"],
        "static instructions changed"
    );
    assert!(before["tools"] == after["tools"], "top-level tools changed");
    let before = before["input"].as_array().unwrap();
    let after = after["input"].as_array().unwrap();
    assert!(
        after.len() >= before.len(),
        "input shortened: {} -> {}",
        before.len(),
        after.len()
    );
    for (index, (old, new)) in before.iter().zip(after).enumerate() {
        assert!(
            old == new,
            "historical input changed at item {index}: {}",
            first_difference(old, new, String::new())
        );
    }
}

fn first_difference(before: &Value, after: &Value, path: String) -> String {
    match (before, after) {
        (Value::Object(old), Value::Object(new)) => {
            for key in old.keys().chain(new.keys()) {
                if old.get(key) != new.get(key) {
                    let next = format!("{path}/{key}");
                    return match (old.get(key), new.get(key)) {
                        (Some(old), Some(new)) => first_difference(old, new, next),
                        _ => format!("{next}: field presence differs"),
                    };
                }
            }
        }
        (Value::Array(old), Value::Array(new)) => {
            for (index, (old, new)) in old.iter().zip(new).enumerate() {
                if old != new {
                    return first_difference(old, new, format!("{path}/{index}"));
                }
            }
            return format!("{path}: array length {} -> {}", old.len(), new.len());
        }
        (Value::String(old), Value::String(new)) => {
            let common = old
                .chars()
                .zip(new.chars())
                .take_while(|(a, b)| a == b)
                .count();
            return format!(
                "{path}: string chars {} -> {}, common prefix {common}",
                old.chars().count(),
                new.chars().count()
            );
        }
        _ => return format!("{path}: scalar value or kind differs"),
    }
    format!("{path}: no differing field")
}

#[tokio::test]
async fn rollover_keeps_the_final_sdk_wire_prefix_in_both_drivers() {
    for streaming in [false, true] {
        for rendering in [
            InstructionRendering::Inline,
            InstructionRendering::Announced,
        ] {
            let fixture = Fixture::new(rendering).await;
            fixture.run(streaming, "first after rollover", None).await;
            fixture.run(streaming, "second after rollover", None).await;
            let requests = fixture.transport.requests();
            assert_eq!(requests.len(), 2);
            let expected = usize::from(rendering == InstructionRendering::Inline);
            assert_eq!(count_text(&requests[0], DATE_TEXT), expected);
            assert_eq!(
                count_text(&requests[1], DATE_TEXT),
                expected,
                "once admitted, the date notification remains in historical context"
            );
            assert_input_prefix(&requests[0], &requests[1]);
        }
    }
}

#[tokio::test]
async fn cancellation_before_dispatch_keeps_the_date_for_continue_in_both_drivers() {
    for streaming in [false, true] {
        let fixture = Fixture::new(InstructionRendering::Inline).await;
        let cancel = CancellationToken::new();
        *fixture.capture_scopes.cancel_on_capture.lock().unwrap() = Some(cancel.clone());
        let outcome = fixture
            .run(streaming, "cancel before dispatch", Some(cancel.clone()))
            .await;
        assert!(
            matches!(outcome, TurnOutcome::Cancelled),
            "streaming={streaming}, outcome={outcome:?}, cancel={}",
            cancel.is_cancelled()
        );
        assert_eq!(
            fixture
                .capture_scopes
                .cancelled_captures
                .load(Ordering::SeqCst),
            1,
            "the shared pre-dispatch cancellation seam actually ran"
        );
        assert!(
            fixture.transport.requests().is_empty(),
            "cancellation occurs before the final transport dispatch"
        );
        fixture
            .run(streaming, "continue after cancellation", None)
            .await;
        fixture
            .run(streaming, "continue without duplication", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            count_text(&requests[0], DATE_TEXT),
            1,
            "the cancelled assembly cannot consume the date notification"
        );
        assert_eq!(count_text(&requests[1], DATE_TEXT), 1);
        assert_input_prefix(&requests[0], &requests[1]);
    }
}

#[tokio::test]
async fn rejected_output_capture_before_dispatch_keeps_date_for_continue_in_both_drivers() {
    for streaming in [false, true] {
        let fixture = Fixture::new(InstructionRendering::Inline).await;
        let cancel = CancellationToken::new();
        *fixture.capture_scopes.reject_with_cancel.lock().unwrap() = Some(cancel.clone());
        let result = fixture
            .attempt(
                streaming,
                "cancel at output admission",
                Some(cancel.clone()),
            )
            .await;
        assert!(fixture.capture_scopes.rejected.load(Ordering::SeqCst));
        assert!(cancel.is_cancelled());
        match result {
            Ok(TurnOutcome::Cancelled) => {}
            Err(error) => assert!(
                error.to_string().contains("date fixture capture rejected"),
                "unexpected rejection: {error}"
            ),
            other => panic!("rejected capture cannot complete a model turn: {other:?}"),
        }
        assert!(
            fixture.transport.requests().is_empty(),
            "output admission rejected before physical dispatch"
        );
        fixture
            .run(streaming, "continue after capture cancellation", None)
            .await;
        fixture
            .run(streaming, "continue with the same date identity", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(count_text(&requests[0], DATE_TEXT), 1);
        assert_eq!(count_text(&requests[1], DATE_TEXT), 1);
        assert_input_prefix(&requests[0], &requests[1]);
    }
}

#[tokio::test]
async fn first_cold_rollover_recovers_initial_date_without_a_prompt_snapshot_in_both_drivers() {
    for streaming in [false, true] {
        let mut fixture = Fixture::new(InstructionRendering::Inline).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        fixture
            .orch
            .prepare_inline_date_context(Some(&cancel))
            .await
            .unwrap();
        let source = std::fs::read_to_string(&fixture.transcript).unwrap();
        assert!(source.contains("inline_date_state"));
        assert!(!source.contains("prompt_snapshot"));
        assert!(
            !source.contains(DATE_TEXT),
            "the old day's checkpoint is host-only"
        );
        fixture.cold_resume().await;
        fixture
            .run(streaming, "first cold request after the date changed", None)
            .await;
        fixture
            .run(
                streaming,
                "continue without replacing initial context",
                None,
            )
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(count_text(&requests[0], "Today's date is 2000-01-01."), 1);
        assert_eq!(count_text(&requests[0], DATE_TEXT), 1);
        assert_eq!(count_text(&requests[1], DATE_TEXT), 1);
        assert_input_prefix(&requests[0], &requests[1]);
    }
}

#[tokio::test]
async fn retry_appends_only_the_current_date_then_deferred_tail_once() {
    let fixture = Fixture::new(InstructionRendering::Inline).await;
    let original = fixture.orch.session.lock().await.model_context_history();
    let date = fixture
        .orch
        .prepare_inline_date_context(None)
        .await
        .unwrap()
        .expect("rollover has a new accepted notification");
    let deferred = ConversationMessage::user_meta(
        MessageId::new(),
        "<system-reminder>current deferred tool delta</system-reminder>".into(),
    );
    let mut retry = original.clone();
    let mut reminders = Vec::new();
    let mut guards = Vec::new();
    let context = crate::conversation::PreparedContextAnnouncements::default();
    for _ in 0..2 {
        fixture
            .orch
            .reattach_outgoing_context(
                &mut retry,
                Some(&deferred),
                Some(&date),
                &mut reminders,
                &mut guards,
                &context,
                false,
            )
            .await;
        assert_eq!(&retry[..original.len()], original);
        assert_eq!(&retry[original.len()..], &[date.clone(), deferred.clone()]);
    }
}

#[tokio::test]
async fn running_user_append_preserves_the_long_date_prefix_in_both_drivers() {
    for streaming in [false, true] {
        let fixture = Fixture::new_with_running_search(InstructionRendering::Inline, true).await;
        fixture.transport.search_next.store(true, Ordering::SeqCst);
        fixture
            .run_with_steering_during_request(streaming, "begin running turn", None)
            .await;
        assert_eq!(count_text(&fixture.transport.requests()[0], DATE_TEXT), 1);
        assert_eq!(
            fixture.steering.consumed.load(Ordering::SeqCst),
            1,
            "streaming={streaming}: the running turn consumed the appended user input; {}",
            fixture.diagnostics()
        );
        let running_requests = fixture.transport.requests();
        assert_eq!(
            running_requests.len(),
            2,
            "native search and the appended input continue the same running turn"
        );
        let followup = &running_requests[1];
        for item_type in ["tool_search_call", "tool_search_output"] {
            assert_eq!(
                count_input_items(followup, item_type),
                1,
                "streaming={streaming}: settled native search {item_type} reached the SDK wire"
            );
        }
        assert_eq!(count_text(followup, STEERING), 1);
        assert_input_prefix(&running_requests[0], followup);
        fixture
            .run(streaming, "continue after steering", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 1);
        let last = requests.last().unwrap();
        assert_eq!(count_text(last, STEERING), 1);
        assert_eq!(count_text(last, "date-cache long historical prefix:"), 1);
        assert_eq!(count_text(last, DATE_TEXT), 1);
        assert_input_prefix(&requests[0], last);
    }
}

#[tokio::test]
async fn cancellation_after_tool_result_keeps_queued_input_for_the_next_turn_in_both_drivers() {
    for streaming in [false, true] {
        let fixture = Fixture::new_with_running_search(InstructionRendering::Inline, true).await;
        let cancel = CancellationToken::new();
        *fixture.output.cancel_after_search_result.lock().unwrap() = Some(cancel.clone());
        fixture.transport.search_next.store(true, Ordering::SeqCst);
        let outcome = fixture
            .run_with_steering_during_request(
                streaming,
                "cancel after the search result is emitted",
                Some(cancel.clone()),
            )
            .await;
        assert!(
            matches!(outcome, TurnOutcome::Cancelled),
            "streaming={streaming}, outcome={outcome:?}; {}",
            fixture.diagnostics()
        );
        assert!(cancel.is_cancelled());
        assert_eq!(
            fixture.output.search_results.load(Ordering::SeqCst),
            1,
            "streaming={streaming}; {}",
            fixture.diagnostics()
        );
        assert_eq!(fixture.output.cancelled_results.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.transport.requests().len(), 1);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 0);
        assert!(fixture.steering.pending.lock().unwrap().is_some());
        assert!(fixture
            .orch
            .snapshot_history()
            .await
            .iter()
            .all(|message| !ConversationOrchestrator::text_content(message)
                .is_some_and(|text| text.contains(STEERING))));

        fixture.transport.search_next.store(true, Ordering::SeqCst);
        fixture
            .run(streaming, "continue and finish the next real search", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(fixture.output.search_results.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 1);
        assert!(fixture.steering.pending.lock().unwrap().is_none());
        let followup = &requests[2];
        assert_eq!(count_input_items(followup, "tool_search_call"), 2);
        assert_eq!(count_input_items(followup, "tool_search_output"), 2);
        assert_eq!(count_text(&requests[1], STEERING), 0);
        assert_eq!(count_text(followup, STEERING), 1);
        assert_eq!(count_text(followup, DATE_TEXT), 1);
        assert_input_prefix(&requests[0], &requests[1]);
        assert_input_prefix(&requests[1], followup);

        fixture
            .run(streaming, "verify queued input stays unique", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 1);
        assert_eq!(count_text(&requests[3], STEERING), 1);
        assert_eq!(count_text(&requests[3], DATE_TEXT), 1);
        assert_input_prefix(&requests[2], &requests[3]);
    }
}

#[tokio::test]
async fn tool_step_at_turn_limit_keeps_queued_input_until_a_later_admitted_turn_in_both_drivers() {
    for streaming in [false, true] {
        let mut fixture = Fixture::new_with_turn_limit(InstructionRendering::Inline, true, 1).await;
        fixture.transport.search_next.store(true, Ordering::SeqCst);
        let outcome = fixture
            .run_with_steering_during_request(streaming, "settle a search at the turn limit", None)
            .await;
        assert!(matches!(outcome, TurnOutcome::MaxTurns));
        assert_eq!(
            fixture.output.search_results.load(Ordering::SeqCst),
            1,
            "streaming={streaming}; {}",
            fixture.diagnostics()
        );
        assert_eq!(fixture.transport.requests().len(), 1);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 0);
        assert!(fixture.steering.pending.lock().unwrap().is_some());

        // A host may raise its limit before the next query. Recover the actual
        // durable transcript with the same SDK queue and registry, exercising
        // both the admission boundary and cold date-prefix restoration.
        fixture.config.max_turns = 6;
        fixture.cold_resume().await;
        fixture.transport.search_next.store(true, Ordering::SeqCst);
        fixture
            .run(streaming, "continue with room for the queued input", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(fixture.output.search_results.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 1);
        assert!(fixture.steering.pending.lock().unwrap().is_none());
        assert_eq!(count_input_items(&requests[2], "tool_search_call"), 2);
        assert_eq!(count_input_items(&requests[2], "tool_search_output"), 2);
        assert_eq!(count_text(&requests[1], STEERING), 0);
        assert_eq!(count_text(&requests[2], STEERING), 1);
        assert_eq!(count_text(&requests[2], DATE_TEXT), 1);
        assert_input_prefix(&requests[0], &requests[1]);
        assert_input_prefix(&requests[1], &requests[2]);
        fixture
            .run(streaming, "verify recovered queue deduplication", None)
            .await;
        let requests = fixture.transport.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(fixture.steering.consumed.load(Ordering::SeqCst), 1);
        assert_eq!(count_text(&requests[3], STEERING), 1);
        assert_eq!(count_text(&requests[3], DATE_TEXT), 1);
        assert_input_prefix(&requests[2], &requests[3]);
    }
}

#[tokio::test]
async fn accepted_rollover_keeps_identical_prefix_after_hot_or_cold_recovery() {
    for streaming in [false, true] {
        for cold in [false, true] {
            for rendering in [
                InstructionRendering::Inline,
                InstructionRendering::Announced,
            ] {
                let mut fixture = Fixture::new(rendering).await;
                fixture
                    .run(streaming, "accepted before recovery", None)
                    .await;
                let first = fixture.transport.requests()[0].clone();
                if cold {
                    fixture.cold_resume().await;
                }
                fixture
                    .run(streaming, "continue after recovery", None)
                    .await;
                fixture
                    .run(streaming, "verify date deduplication", None)
                    .await;
                let requests = fixture.transport.requests();
                assert_eq!(requests.len(), 3);
                let expected = usize::from(rendering == InstructionRendering::Inline);
                assert_eq!(count_text(&requests[1], DATE_TEXT), expected);
                assert_eq!(count_text(&requests[2], DATE_TEXT), expected);
                assert_input_prefix(&first, &requests[1]);
                assert_input_prefix(&requests[1], &requests[2]);
            }
        }
    }
}
