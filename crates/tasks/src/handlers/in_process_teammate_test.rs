//! In-process teammate tests.
#![allow(clippy::unwrap_used)]

use super::*;
use futures::StreamExt;
use std::collections::HashMap as StdHashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex as StdMutex;

use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use lingxi_core::host::RuntimeSpawner;
use test_harness::mocks::MockRuntimeSpawner;
use tokio::sync::Mutex as TokioMutex;

// ---- In-memory FileSystem (mirrors the other handler tests) ------------

struct InMemoryFs {
    files: TokioMutex<StdHashMap<String, String>>,
}

#[test]
fn fallback_api_error_projects_once_and_tombstone_is_not_spooled_or_terminal() {
    let agent_id = lingxi_core::types::AgentId::new();
    let row = lingxi_core::host::ServerFallbackApiErrorRow::new(
        "refusal body",
        "2026-10-04T00:00:00.000Z".into(),
    );
    let query_message = serde_json::to_value(row.query_message()).unwrap();
    let api_error_event = SubagentEvent::ServerFallbackApiErrorRow {
        agent_id: agent_id.clone(),
        row: row.clone(),
        message_index: 7,
    };
    let ordinary_message_event = SubagentEvent::Message {
        agent_id,
        message: query_message,
        message_index: Some(7),
    };
    let api_line = event_line(&api_error_event);
    assert!(
        api_line.contains("refusal body"),
        "refusal text remains visible"
    );
    assert_eq!(
        api_line,
        event_line(&ordinary_message_event),
        "the dedicated row event projects visible assistant text exactly once"
    );
    assert_eq!(terminal_status(&api_error_event), None);
    assert!(!is_idle_event(&api_error_event));

    let tombstone = SubagentEvent::ServerFallbackTombstone {
        agent_id,
        message: lingxi_core::host::ServerFallbackTombstoneMessage {
            uuid: row.uuid.clone(),
            message_type: "assistant".into(),
            timestamp: "2026-10-04T00:00:00.000Z".into(),
            request_id: row.request_id.clone(),
            request_ref: None,
            provider_message_id: None,
            model: Some("test-model".into()),
            stop_reason: Some("refusal".into()),
            stop_details: row.message.stop_details.clone(),
            usage: Some(row.message.usage.clone()),
            content: row.message.content.clone(),
            is_api_error_message: Some(true),
            supersedes_uuids: None,
        },
        display_only: true,
    };
    assert!(event_line(&tombstone).is_empty());
    assert_eq!(terminal_status(&tombstone), None);
    assert!(!is_idle_event(&tombstone));
}

impl InMemoryFs {
    fn new() -> Self {
        Self {
            files: TokioMutex::new(StdHashMap::new()),
        }
    }
}
#[async_trait]
impl FileSystem for InMemoryFs {
    async fn read_file(
        &self,
        path: &str,
        _offset: Option<u64>,
        _limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let map = self.files.lock().await;
        let content = map.get(path).cloned().unwrap_or_default();
        let total_lines = content.lines().count() as u64;
        Ok(FileContent {
            content,
            truncated: false,
            total_lines,
        })
    }
    async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .await
            .insert(path.to_string(), body.to_string());
        Ok(())
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        Err(FsError::Io("not supported".into()))
    }
    async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
        let mut map = self.files.lock().await;
        map.entry(path.to_string()).or_default().push_str(body);
        Ok(())
    }
    async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
        Ok(())
    }
    async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
        Ok(std::time::SystemTime::UNIX_EPOCH)
    }
    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        let map = self.files.lock().await;
        Ok(map.get(path).map_or(0, |s| s.len() as u64))
    }
    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.files.lock().await.remove(path);
        Ok(())
    }
    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io("not supported".into()))
    }
    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        Ok(())
    }
}

// ---- Scripted SubagentApiClient ----------------------------------------

/// Hands back a pre-scripted queue of responses, one per `stream(request)`.
/// When exhausted it returns an `end_turn` text turn so each turn-set
/// terminates and the persistent runner parks for the next message. An
/// `Err` entry surfaces as an API error (driving the runner to `Failed`).
struct ScriptedApiClient {
    responses: StdMutex<VecDeque<Result<llm_runtime::HistoryResponse, String>>>,
    calls: AtomicUsize,
}

/// Holds the first provider request open so a test can prove inbound teammate
/// messages queue instead of cancelling/reissuing that in-flight request.
struct GatedApiClient {
    calls: AtomicUsize,
    first_started: tokio::sync::Semaphore,
    release_first: tokio::sync::Semaphore,
    histories: StdMutex<Vec<Vec<lingxi_core::types::ConversationMessage>>>,
}

impl GatedApiClient {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            first_started: tokio::sync::Semaphore::new(0),
            release_first: tokio::sync::Semaphore::new(0),
            histories: StdMutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl SubagentApiClient for GatedApiClient {
    async fn stream(
        &self,
        request: agent::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let messages = request.messages;
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.histories.lock().unwrap().push(messages);
            if call == 0 {
                self.first_started.add_permits(1);
                self.release_first.acquire().await.unwrap().forget();
            }
            Ok(text_response(if call == 0 { "first" } else { "second" }))
        };
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}
impl ScriptedApiClient {
    fn new(texts: Vec<&str>) -> Arc<Self> {
        let responses = texts
            .into_iter()
            .map(|t| Ok(text_response(t)))
            .collect::<VecDeque<_>>();
        Arc::new(Self {
            responses: StdMutex::new(responses),
            calls: AtomicUsize::new(0),
        })
    }
    /// Script a single API error so the first turn-set surfaces `Failed`.
    fn new_error(message: &str) -> Arc<Self> {
        let mut responses = VecDeque::new();
        responses.push_back(Err(message.to_string()));
        Arc::new(Self {
            responses: StdMutex::new(responses),
            calls: AtomicUsize::new(0),
        })
    }
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
#[async_trait]
impl SubagentApiClient for ScriptedApiClient {
    async fn stream(
        &self,
        _request: agent::api::SubagentApiRequest,
    ) -> Result<
        futures::stream::BoxStream<
            'static,
            Result<llm_runtime::HistoryEvent, llm_runtime::LlmError>,
        >,
        llm_runtime::LlmError,
    > {
        let response: Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> = {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.responses.lock().unwrap().pop_front();
            match next {
                Some(Ok(resp)) => Ok(resp),
                Some(Err(msg)) => Err(llm_runtime::LlmError::InvalidRequest { message: msg }),
                None => Ok(text_response("(idle)")),
            }
        };
        let events = llm_runtime::stream_accumulator::response_to_stream_events(response?);
        Ok(futures::stream::iter(events.into_iter().map(Ok)).boxed())
    }
}

fn text_response(text: &str) -> llm_runtime::HistoryResponse {
    llm_runtime::HistoryResponse {
        id: "mock".into(),
        model: "mock".into(),
        content: vec![llm_runtime::ContentBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        usage: llm_runtime::ExecutionUsage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

// ---- Recording status sink ---------------------------------------------

#[derive(Default)]
struct RecordingSink {
    idle_tasks: StdMutex<Vec<String>>,
    awaiting_plan: StdMutex<Vec<bool>>,
    requires_activation: AtomicBool,
    statuses: StdMutex<Vec<(String, TaskStatus)>>,
    /// Failure reasons received through the `set_failed` seam (cc 2.1.198:
    /// the failed idle notification's `failureReason` to the lead).
    failures: StdMutex<Vec<(String, String)>>,
    team_activity: StdMutex<Vec<(lingxi_core::types::AgentId, bool)>>,
    event_order: StdMutex<Vec<String>>,
}
#[async_trait]
impl TaskStatusSink for RecordingSink {
    async fn set_teammate_idle(&self, task_id: &str) {
        self.idle_tasks.lock().unwrap().push(task_id.to_owned());
    }
    async fn set_awaiting_plan_approval(&self, _: &str, awaiting: bool) {
        self.awaiting_plan.lock().unwrap().push(awaiting);
    }
    fn requires_explicit_activation(&self) -> bool {
        self.requires_activation.load(Ordering::SeqCst)
    }

    async fn set_status(&self, task_id: &str, status: TaskStatus) {
        self.event_order
            .lock()
            .unwrap()
            .push(format!("status:{status:?}"));
        self.statuses
            .lock()
            .unwrap()
            .push((task_id.to_string(), status));
    }
    async fn set_failed(&self, task_id: &str, error: &str) {
        self.event_order.lock().unwrap().push("failed".into());
        self.failures
            .lock()
            .unwrap()
            .push((task_id.to_string(), error.to_string()));
        self.set_status(task_id, TaskStatus::Failed).await;
    }
    async fn set_team_member_active(
        &self,
        agent_id: lingxi_core::types::AgentId,
        active: bool,
    ) -> Result<(), String> {
        self.team_activity.lock().unwrap().push((agent_id, active));
        self.event_order
            .lock()
            .unwrap()
            .push(format!("team-active:{active}"));
        Ok(())
    }
}
impl RecordingSink {
    fn require_activation(&self) {
        self.requires_activation.store(true, Ordering::SeqCst);
    }

    fn last_status(&self) -> Option<TaskStatus> {
        self.statuses.lock().unwrap().last().map(|(_, s)| *s)
    }
    fn failures(&self) -> Vec<(String, String)> {
        self.failures.lock().unwrap().clone()
    }
}

// ---- Recording TeammateIdle firer --------------------------------------

/// A [`hooks::TeammateIdleFirer`] that records every fire it receives, so a
/// test can assert the per-turn-set idle moment fired with the right payload.
#[derive(Default)]
struct RecordingIdleFirer {
    fires: StdMutex<Vec<hooks::TeammateIdleFire>>,
    outcomes: StdMutex<VecDeque<hooks::TeammateIdleOutcome>>,
}
#[async_trait]
impl hooks::TeammateIdleFirer for RecordingIdleFirer {
    async fn fire(&self, fire: hooks::TeammateIdleFire) -> hooks::TeammateIdleOutcome {
        self.fires.lock().unwrap().push(fire);
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_default()
    }
}
impl RecordingIdleFirer {
    fn fires(&self) -> Vec<hooks::TeammateIdleFire> {
        self.fires.lock().unwrap().clone()
    }
    fn push_outcome(&self, outcome: hooks::TeammateIdleOutcome) {
        self.outcomes.lock().unwrap().push_back(outcome);
    }
}

struct GatedIdleFirer {
    started: tokio::sync::Semaphore,
    release: tokio::sync::Semaphore,
}

impl GatedIdleFirer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        })
    }
}

#[async_trait]
impl hooks::TeammateIdleFirer for GatedIdleFirer {
    async fn fire(&self, _fire: hooks::TeammateIdleFire) -> hooks::TeammateIdleOutcome {
        self.started.add_permits(1);
        self.release.acquire().await.unwrap().forget();
        hooks::TeammateIdleOutcome {
            blocking_feedback: vec!["TeammateIdle hook feedback:\ncontinue".into()],
            ..Default::default()
        }
    }
}

// ---- Helpers ------------------------------------------------------------

fn make_handler(
    api: Arc<dyn SubagentApiClient>,
) -> (
    tempfile::TempDir,
    Arc<dyn FileSystem>,
    Arc<MockRuntimeSpawner>,
    InProcessTeammateHandler,
) {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let handler = InProcessTeammateHandler::new(pool, output, api);
    (dir, fs, runtime, handler)
}

/// Like [`make_handler`] but returns the attached [`RecordingSink`] so a
/// test can observe terminal status transitions.
fn make_handler_with_sink(
    api: Arc<ScriptedApiClient>,
) -> (
    tempfile::TempDir,
    Arc<dyn FileSystem>,
    Arc<MockRuntimeSpawner>,
    InProcessTeammateHandler,
    Arc<RecordingSink>,
) {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let sink = Arc::new(RecordingSink::default());
    let handler = InProcessTeammateHandler::new(pool, output, api).with_status_sink(sink.clone());
    (dir, fs, runtime, handler, sink)
}

/// Yield until the sink reports a terminal status, or the budget runs out.
async fn await_terminal(sink: &Arc<RecordingSink>) -> Option<TaskStatus> {
    for _ in 0..400 {
        if let Some(s) = sink.last_status() {
            if s.is_terminal() {
                return Some(s);
            }
        }
        tokio::task::yield_now().await;
    }
    sink.last_status()
}

fn ctx(fs: Arc<dyn FileSystem>, runtime: Arc<MockRuntimeSpawner>) -> TaskContext {
    TaskContext {
        fs,
        runtime: runtime as Arc<dyn RuntimeSpawner>,
    }
}

/// Yield until `pred` over the spool body holds, or the budget runs out.
async fn await_spool<F: Fn(&str) -> bool>(
    fs: &Arc<dyn FileSystem>,
    spool: &str,
    pred: F,
) -> String {
    for _ in 0..400 {
        let body = fs.read_file(spool, None, None).await.unwrap().content;
        if pred(&body) {
            return body;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    fs.read_file(spool, None, None).await.unwrap().content
}

/// Wait for the activated worker to publish its task-list claim.
async fn await_claim(
    store: &task_store::TodoStore,
    task_id: &str,
    owner: &str,
) -> task_store::TodoTask {
    for _ in 0..400 {
        if let Some(task) = store.get(task_id).await {
            if task.owner.as_deref() == Some(owner)
                && task.status == lingxi_core::TodoState::InProgress
            {
                return task;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("task {task_id} was not claimed by {owner}");
}

/// Wait for terminal cleanup to remove the handler's live control block.
async fn await_entry_removed(handler: &InProcessTeammateHandler, task_id: &str) {
    for _ in 0..400 {
        if !handler.entries.lock().await.contains_key(task_id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("teammate entry {task_id} was not removed");
}

// ---- Tests --------------------------------------------------------------

/// Minimal handler for the `build_context` model-resolution tests (no spawn).
fn model_test_handler(default_model: Option<&str>) -> InProcessTeammateHandler {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let output = Arc::new(TaskOutputManager::new(PathBuf::from(dir.path()), fs));
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let pool = Arc::new(StateMachinePool::new(runtime as Arc<dyn RuntimeSpawner>, 8));
    let handler = InProcessTeammateHandler::new(pool, output, ScriptedApiClient::new(vec!["ok"]))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.to_string(),
                    profile: profile.map(str::to_owned),
                    provider: Some(agent::ModelProviderKind::FirstParty),
                    ..Default::default()
                },
                family_defaults: agent::FamilyModelDefaults {
                    opus: Some("claude-opus-4-8".into()),
                    sonnet: Some("claude-sonnet-5".into()),
                    haiku: Some("claude-haiku-4-5".into()),
                    fable: Some("claude-fable-5-1".into()),
                },
                ..Default::default()
            })
        }));
    match default_model {
        Some(m) => handler.with_default_model(m),
        None => handler,
    }
}

struct RoutePromptTool;

#[async_trait]
impl tool_api::Tool for RoutePromptTool {
    fn name(&self) -> &str {
        "RouteFacts"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: OnceLock<serde_json::Value> = OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object"}))
    }
    fn is_enabled(&self, _: &tool_api::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _: &serde_json::Value) -> bool {
        true
    }
    async fn check_permissions(
        &self,
        _: &serde_json::Value,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &serde_json::Value, _: &tool_api::DescriptionOptions) -> String {
        "Route facts".into()
    }
    async fn prompt(&self, opts: &tool_api::PromptOptions) -> String {
        format!("model={:?}, profile={:?}", opts.model, opts.model_profile)
    }
    async fn call(
        &self,
        _: serde_json::Value,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        unreachable!("schema rendering does not invoke tools")
    }
}

#[tokio::test]
async fn teammate_tool_prompts_use_child_model_and_profile_for_duplicate_model_ids() {
    let mut registry = agent::ToolRegistry::new();
    registry.register_builtin(Arc::new(RoutePromptTool));
    let handler = model_test_handler(Some("parent-model"))
        .with_default_model_profile(Some("provider-a".into()))
        .with_tool_registry(Arc::new(registry))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            if !matches!(
                (model, profile),
                ("parent-model", Some("provider-a"))
                    | ("shared-child-model", Some("provider-a" | "provider-b"))
            ) {
                return Err(agent::ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    reason: "unexpected test route".into(),
                });
            }
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    provider: Some(agent::ModelProviderKind::Other),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    for profile in ["provider-b", "provider-a"] {
        let mut definition = DefaultTeammateDefinition
            .resolve(&lingxi_core::types::AgentId::new(), "worker")
            .await
            .unwrap();
        definition.model = AgentModel::Explicit("shared-child-model".into());
        let context = handler
            .build_context_for_profile(
                lingxi_core::types::AgentId::new(),
                "worker",
                "",
                "",
                Some(profile),
                definition,
            )
            .await
            .unwrap();
        assert_eq!(context.model_profile.as_deref(), Some(profile));
        assert_eq!(
            context.tool_schemas[0]["description"],
            format!("model=Some(\"shared-child-model\"), profile=Some(\"{profile}\")")
        );
    }
}

struct StaticSystemPromptRenderer(&'static str);

#[async_trait]
impl TeammateSystemPromptRenderer for StaticSystemPromptRenderer {
    async fn render_default_system_prompt(&self) -> String {
        self.0.to_string()
    }
}

struct TaggedDiagnosticsSource(usize);

#[async_trait]
impl lingxi_core::host::NewDiagnosticsSource for TaggedDiagnosticsSource {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        Some(format!("diagnostics-cursor-{}", self.0))
    }
}

#[tokio::test]
async fn build_context_creates_an_independent_diagnostics_source_per_teammate() {
    let next_cursor = Arc::new(AtomicUsize::new(0));
    let factory_counter = next_cursor.clone();
    let handler =
        model_test_handler(None).with_new_diagnostics_source_factory(Arc::new(move || {
            let cursor = factory_counter.fetch_add(1, Ordering::SeqCst) + 1;
            Arc::new(TaggedDiagnosticsSource(cursor))
                as Arc<dyn lingxi_core::host::NewDiagnosticsSource>
        }));

    let first_definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "first")
        .await
        .unwrap();
    let second_definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "second")
        .await
        .unwrap();
    let first = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "first",
            "team",
            "task one",
            first_definition,
        )
        .await
        .unwrap();
    let second = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "second",
            "team",
            "task two",
            second_definition,
        )
        .await
        .unwrap();

    let first_source = first.new_diagnostics_source.expect("first source");
    let second_source = second.new_diagnostics_source.expect("second source");
    assert!(!Arc::ptr_eq(&first_source, &second_source));
    assert_eq!(
        first_source.take_new_diagnostics_block().await.as_deref(),
        Some("diagnostics-cursor-1")
    );
    assert_eq!(
        second_source.take_new_diagnostics_block().await.as_deref(),
        Some("diagnostics-cursor-2")
    );
    assert_eq!(next_cursor.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn build_context_renders_default_addendum_then_custom_prompt() {
    let handler = model_test_handler(None).with_system_prompt_renderer(Arc::new(
        StaticSystemPromptRenderer("BASE DEFAULT PROMPT\n"),
    ));
    let mut def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    def.system_prompt = Some("CUSTOM AGENT PROMPT".to_string());

    let ctx = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "alpha",
            "task",
            def,
        )
        .await
        .unwrap();
    let expected = format!(
        "BASE DEFAULT PROMPT\n{TEAMMATE_SYSTEM_PROMPT_ADDENDUM}\n\n# Custom Agent Instructions\nCUSTOM AGENT PROMPT"
    );
    assert_eq!(
        ctx.rendered_system_prompt.as_deref(),
        Some(expected.as_str())
    );
}

#[tokio::test]
async fn build_context_carries_owning_session_interactivity() {
    let handler = model_test_handler(None).with_session_interactive(false);
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "alpha",
            "task",
            def,
        )
        .await
        .unwrap();

    assert_eq!(ctx.session_interactive, Some(false));
    assert!(ctx.persistent);
    assert!(!ctx.is_async);
}

#[tokio::test]
async fn build_context_resolves_inherit_to_default_model() {
    // DefaultTeammateDefinition yields AgentModel::Inherit; with a default
    // model wired the teammate ctx carries a concrete wire id (folded via
    // the same `resolve_agent_model` seam as the spawner).
    let handler = model_test_handler(Some("claude-opus-4-7"));
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "alpha",
            "go research",
            def,
        )
        .await
        .expect("context should build");
    assert!(
        matches!(&ctx.agent_definition.model, AgentModel::Explicit(m) if m == "claude-opus-4-7")
    );
    // Swarm identity threaded onto the context (claude-code
    // `TeammateContext.agentName` / `.teamName`).
    assert_eq!(ctx.agent_name.as_deref(), Some("lead"));
    assert_eq!(ctx.team_name.as_deref(), Some("alpha"));
    // The initial Agent prompt becomes a team-lead teammate message.
    assert_eq!(ctx.prompt_messages.len(), 1);
    assert_eq!(
        ctx.prompt_messages[0].text_content(),
        "<teammate-message teammate_id=\"team-lead\">\ngo research\n</teammate-message>"
    );
}

// Full teammate parity (P1): the handler inherits a budget enforcer, seeds
// the initial Agent prompt as the first user message, and runs the shared
// tool resolver when a registry is wired (advertising a real pool instead of
// the prior chat-only empty set).
#[tokio::test]
async fn build_context_wires_budget_description_and_tool_resolution() {
    struct DummyBudget;
    #[async_trait]
    impl lingxi_core::host::budget::BudgetEnforcerHandle for DummyBudget {
        async fn check_and_charge(
            &self,
            _: u64,
        ) -> Result<(), lingxi_core::host::budget::BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }
    let handler = model_test_handler(Some("claude-opus-4-7"))
        .with_budget_enforcer(Arc::new(DummyBudget))
        // An EMPTY registry still exercises the resolution path (returns an
        // empty pool); a populated registry is covered by the agent crate's
        // `resolve_subagent_tools` tests (shared code path).
        .with_tool_registry(Arc::new(agent::ToolRegistry::new()));
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "alpha",
            "do the task",
            def,
        )
        .await
        .expect("context should build");
    // Budget inherited (was None before this fix).
    assert!(
        ctx.budget.is_some(),
        "teammate must inherit the budget enforcer"
    );
    // Description seeded through the canonical team-lead envelope.
    assert_eq!(ctx.prompt_messages.len(), 1);
    assert_eq!(
        ctx.prompt_messages[0].text_content(),
        "<teammate-message teammate_id=\"team-lead\">\ndo the task\n</teammate-message>"
    );
    // The resolver ran (empty registry ⇒ empty pool, but the path is wired —
    // no panic, and the allow-list mirrors the advertised set).
    assert_eq!(ctx.tool_schemas.len(), ctx.allowed_tools.len());
}

#[tokio::test]
async fn build_context_without_default_model_leaves_model_raw() {
    // No default model wired → legacy behavior: Inherit is left untouched.
    let handler = model_test_handler(None);
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await
        .expect("context should build");
    assert!(matches!(&ctx.agent_definition.model, AgentModel::Inherit));
    // Empty team_name spawns standalone → team_name is None (leader default).
    assert_eq!(ctx.agent_name.as_deref(), Some("lead"));
    assert_eq!(ctx.team_name, None);
    // Empty description ⇒ no seed message (parks awaiting first injection).
    assert!(ctx.prompt_messages.is_empty());
}

#[tokio::test]
async fn build_context_preserves_the_profile_that_serves_the_inherited_child_route() {
    let handler = model_test_handler(Some("claude-sonnet-5"))
        .with_default_model_profile(Some("team-provider".into()));
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await
        .expect("route is supplied by the host provider");

    assert!(matches!(
        &ctx.agent_definition.model,
        AgentModel::Explicit(model) if model == "claude-sonnet-5"
    ));
    assert_eq!(ctx.model_profile.as_deref(), Some("team-provider"));
}

#[tokio::test]
async fn build_context_uses_shared_live_session_model_and_profile_selection() {
    let live_selection: Arc<std::sync::OnceLock<agent::DefaultModelSelectionProvider>> =
        Arc::new(std::sync::OnceLock::new());
    assert!(live_selection
        .set(Arc::new(|| {
            Ok(Some(agent::DefaultModelSelection {
                model: "live-sonnet".into(),
                model_profile: Some("live-provider".into()),
                model_resolution_context: agent::ModelResolutionContext {
                    route: agent::ModelRouteFacts {
                        model: "live-sonnet".into(),
                        profile: Some("live-provider".into()),
                        provider: Some(agent::ModelProviderKind::Other),
                        ..Default::default()
                    },
                    family_defaults: agent::FamilyModelDefaults {
                        opus: Some("live-opus".into()),
                        sonnet: Some("live-sonnet".into()),
                        haiku: Some("live-haiku".into()),
                        fable: None,
                    },
                    ..Default::default()
                },
            }))
        }))
        .is_ok());
    let handler = model_test_handler(Some("boot-model"))
        .with_default_model_selection_provider_handle(live_selection);
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await
        .expect("live selection carries its resolved route");

    assert!(matches!(
        &ctx.agent_definition.model,
        AgentModel::Explicit(model) if model == "live-sonnet"
    ));
    assert_eq!(ctx.model_profile.as_deref(), Some("live-provider"));
}

#[tokio::test]
async fn alias_child_keeps_the_profile_that_resolved_its_final_model() {
    use agent::model_resolution::{
        FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
        ModelRouteFacts,
    };

    let handler = model_test_handler(Some("parent-model"))
        .with_default_model_profile(Some("provider-a".into()))
        .with_model_resolution_context_provider(Arc::new(
            |model: &str, profile: Option<&str>| match (model, profile) {
                ("parent-model", Some("provider-a")) => Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.into(),
                        profile: Some("provider-a".into()),
                        provider: Some(ModelProviderKind::Other),
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        sonnet: Some("shared-child-model".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ("shared-child-model", Some("provider-a")) => Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.into(),
                        profile: Some("provider-a".into()),
                        provider: Some(ModelProviderKind::Other),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ("shared-child-model", None) => Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: vec!["provider-a".into(), "provider-b".into()],
                }),
                _ => Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    reason: "unexpected test route".into(),
                }),
            },
        ));
    let mut definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    definition.model = AgentModel::Alias("sonnet".into());

    let context = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "",
            "",
            definition,
        )
        .await
        .expect("the alias was resolved under provider-a");

    assert!(matches!(
        context.agent_definition.model,
        AgentModel::Explicit(ref model) if model == "shared-child-model"
    ));
    assert_eq!(context.model_profile.as_deref(), Some("provider-a"));
}

#[tokio::test]
async fn explicit_child_profile_overrides_the_default_route() {
    use agent::model_resolution::{
        FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
        ModelRouteFacts,
    };

    let handler = model_test_handler(Some("parent-model"))
        .with_default_model_profile(Some("provider-a".into()))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            let (profile, sonnet) = match (model, profile) {
                ("parent-model", Some("provider-a")) => ("provider-a", "provider-a-sonnet"),
                ("sonnet", Some("provider-b")) => ("provider-b", "provider-b-sonnet"),
                ("provider-b-sonnet", Some("provider-b")) => ("provider-b", "provider-b-sonnet"),
                _ => {
                    return Err(ModelResolutionError::RouteUnavailable {
                        model: model.into(),
                        profile: profile.map(str::to_owned),
                        reason: "unexpected test route".into(),
                    })
                }
            };
            Ok(ModelResolutionContext {
                route: ModelRouteFacts {
                    model: model.into(),
                    profile: Some(profile.into()),
                    provider: Some(ModelProviderKind::Other),
                    ..Default::default()
                },
                family_defaults: FamilyModelDefaults {
                    sonnet: Some(sonnet.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    let mut definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    definition.model = AgentModel::Alias("sonnet".into());

    let context = handler
        .build_context_for_profile(
            lingxi_core::types::AgentId::new(),
            "lead",
            "",
            "",
            Some("provider-b"),
            definition,
        )
        .await
        .expect("explicit profile selects its own alias route");

    assert!(matches!(
        context.agent_definition.model,
        AgentModel::Explicit(ref model) if model == "provider-b-sonnet"
    ));
    assert_eq!(context.model_profile.as_deref(), Some("provider-b"));
}

#[tokio::test]
async fn teammate_cross_provider_model_uses_child_route_without_retargeting_parent() {
    use agent::model_resolution::{
        ModelProviderKind, ModelResolutionContext, ModelResolutionError, ModelRouteFacts,
    };
    let queries = Arc::new(StdMutex::new(Vec::new()));
    let observed = queries.clone();
    let handler = model_test_handler(Some("claude-sonnet-4-5"))
        .with_default_model_profile(Some("anthropic".into()))
        .with_model_resolution_context_provider(Arc::new(
            move |model: &str, profile: Option<&str>| {
                observed
                    .lock()
                    .unwrap()
                    .push((model.to_owned(), profile.map(str::to_owned)));
                let (wire_model, selected_profile) = match (model, profile) {
                    ("claude-sonnet-4-5", Some("anthropic")) => (model, "anthropic"),
                    ("gpt-4o", Some("openai")) => (model, "openai"),
                    ("openai/gpt-4o", None) => ("gpt-4o", "openai"),
                    _ => {
                        return Err(ModelResolutionError::RouteUnavailable {
                            model: model.into(),
                            profile: profile.map(str::to_owned),
                            reason: "model is unavailable in this profile".into(),
                        })
                    }
                };
                Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: wire_model.into(),
                        profile: Some(selected_profile.into()),
                        provider: Some(ModelProviderKind::Other),
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
        ));
    for (model, profile) in [("gpt-4o", Some("openai")), ("openai/gpt-4o", None)] {
        let mut definition = DefaultTeammateDefinition
            .resolve(&lingxi_core::types::AgentId::new(), "lead")
            .await
            .unwrap();
        definition.model = AgentModel::Explicit(model.into());
        let mut context = handler
            .build_context_for_profile(
                lingxi_core::types::AgentId::new(),
                "lead",
                "",
                "",
                profile,
                definition,
            )
            .await
            .expect("target provider supports the child model");
        assert!(
            matches!(context.agent_definition.model, AgentModel::Explicit(ref model) if model == "gpt-4o")
        );
        assert_eq!(context.model_profile.as_deref(), Some("openai"));
        apply_spawn_context(
            &mut context,
            lingxi_core::host::SubagentSpawnRequest {
                model_profile: Some("stale-unresolved-profile".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            context.model_profile.as_deref(),
            Some("openai"),
            "local context application cannot overwrite the resolved route"
        );
    }
    assert!(!queries.lock().unwrap().iter().any(
        |(model, profile)| model == "claude-sonnet-4-5" && profile.as_deref() == Some("openai")
    ));
}

#[tokio::test]
async fn teammate_alias_uses_immediate_parent_profile_with_shared_model_id() {
    use agent::model_resolution::{
        FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
        ModelRouteFacts,
    };
    let handler = model_test_handler(Some("shared-parent"))
        .with_default_model_profile(Some("provider-a".into()))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            let (profile, child) = match (model, profile) {
                ("shared-parent", Some("provider-a")) | ("child-a", Some("provider-a")) => {
                    ("provider-a", "child-a")
                }
                ("shared-parent", Some("provider-b")) | ("child-b", Some("provider-b")) => {
                    ("provider-b", "child-b")
                }
                _ => {
                    return Err(ModelResolutionError::RouteUnavailable {
                        model: model.into(),
                        profile: profile.map(str::to_owned),
                        reason: "ambiguous without a provider profile".into(),
                    })
                }
            };
            Ok(ModelResolutionContext {
                route: ModelRouteFacts {
                    model: model.into(),
                    profile: Some(profile.into()),
                    provider: Some(ModelProviderKind::Other),
                    ..Default::default()
                },
                family_defaults: FamilyModelDefaults {
                    sonnet: Some(child.into()),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    let missing_live_selection: Arc<std::sync::OnceLock<agent::DefaultModelSelectionProvider>> =
        Arc::new(std::sync::OnceLock::new());
    assert!(missing_live_selection
        .set(Arc::new(|| Err(
            agent::ModelResolutionError::RouteUnavailable {
                model: "root-route".into(),
                profile: None,
                reason: "root selection is unavailable".into(),
            }
        )))
        .is_ok());
    let handler = handler.with_default_model_selection_provider_handle(missing_live_selection);
    let mut definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    definition.model = AgentModel::Alias("sonnet".into());
    let context = handler
        .build_context_for_selection(
            lingxi_core::types::AgentId::new(),
            "lead",
            "",
            "",
            None,
            Some("shared-parent"),
            Some("provider-b"),
            None,
            definition,
        )
        .await
        .expect("nested parent's route is pinned independently from the child")
        .0;
    assert!(
        matches!(context.agent_definition.model, AgentModel::Explicit(ref model) if model == "child-b")
    );
    assert_eq!(context.model_profile.as_deref(), Some("provider-b"));
}

#[tokio::test]
async fn teammate_environment_model_resolves_before_definition_and_call_profile() {
    let handler = model_test_handler(Some("parent-model"))
        .with_default_model_profile(Some("parent-profile".into()))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            let (model, profile) = match (model, profile) {
                ("parent-model", Some("parent-profile")) => ("parent-model", "parent-profile"),
                ("openai/gpt-4o", None) | ("gpt-4o", Some("openai")) => ("gpt-4o", "openai"),
                _ => {
                    return Err(agent::ModelResolutionError::RouteUnavailable {
                        model: model.into(),
                        profile: profile.map(str::to_owned),
                        reason: "wrong route".into(),
                    })
                }
            };
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.into(),
                    profile: Some(profile.into()),
                    provider: Some(agent::ModelProviderKind::Other),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    for preference in [
        AgentModel::Inherit,
        AgentModel::Alias("sonnet".into()),
        AgentModel::Explicit("unavailable-model".into()),
    ] {
        let mut definition = DefaultTeammateDefinition
            .resolve(&lingxi_core::types::AgentId::new(), "lead")
            .await
            .unwrap();
        definition.model = preference;
        let (context, admitted) = handler
            .build_context_for_selection(
                lingxi_core::types::AgentId::new(),
                "lead",
                "",
                "",
                Some("call-profile"),
                None,
                None,
                Some("openai/gpt-4o"),
                definition,
            )
            .await
            .expect("environmental model owns its complete route");
        assert_eq!(context.model_profile.as_deref(), Some("openai"));
        assert!(
            matches!(context.agent_definition.model, AgentModel::Explicit(ref model) if model == "gpt-4o")
        );
        assert_eq!(admitted.unwrap().model, "gpt-4o");
    }
}

#[tokio::test]
async fn teammate_same_tier_and_bedrock_region_follow_parent_policy() {
    let parent = "eu.anthropic.claude-sonnet-4-5-v1:0";
    let handler = model_test_handler(Some(parent))
        .with_default_model_profile(Some("bedrock".into()))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    provider: Some(agent::ModelProviderKind::Bedrock),
                    ..Default::default()
                },
                family_defaults: agent::FamilyModelDefaults {
                    sonnet: Some("anthropic.claude-sonnet-4-5-v1:0".into()),
                    haiku: Some("anthropic.claude-haiku-4-5-v1:0".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    for (preference, expected) in [
        ("sonnet", parent),
        ("haiku", "eu.anthropic.claude-haiku-4-5-v1:0"),
        (
            "anthropic.claude-haiku-4-5-v1:0",
            "eu.anthropic.claude-haiku-4-5-v1:0",
        ),
    ] {
        let mut definition = DefaultTeammateDefinition
            .resolve(&lingxi_core::types::AgentId::new(), "lead")
            .await
            .unwrap();
        definition.model = AgentModel::Explicit(preference.into());
        let (context, admitted) = handler
            .build_context_for_selection(
                lingxi_core::types::AgentId::new(),
                "lead",
                "",
                "",
                None,
                None,
                None,
                None,
                definition,
            )
            .await
            .unwrap();
        assert!(
            matches!(context.agent_definition.model, AgentModel::Explicit(ref model) if model == expected)
        );
        assert_eq!(admitted.unwrap().model, expected);
        assert_eq!(context.model_profile.as_deref(), Some("bedrock"));
    }
}

#[tokio::test]
async fn named_spawn_rejects_profile_without_child_model() {
    let (_directory, fs, runtime, handler) = make_handler(ScriptedApiClient::new(vec!["unused"]));
    let result = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: Some(lingxi_core::host::SubagentSpawnRequest {
                    model_profile: Some("openai".into()),
                    ..Default::default()
                }),
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "worker".into(),
                team_name: "session".into(),
                description: "inspect".into(),
            },
            ctx(fs, runtime),
        )
        .await;
    assert!(
        matches!(result, Err(TaskError::Internal(ref message)) if message.contains("model_profile requires an explicit child model"))
    );
    assert!(handler.entries.lock().await.is_empty());
}

#[tokio::test]
async fn named_spawn_metadata_keeps_resolved_wire_model_named_sonnet() {
    let (_directory, fs, runtime, handler) = make_handler(ScriptedApiClient::new(vec!["unused"]));
    let handler = handler
        .with_default_model("parent-model")
        .with_default_model_profile(Some("provider-a".into()))
        .with_model_resolution_context_provider(Arc::new(|model: &str, profile: Option<&str>| {
            Ok(agent::ModelResolutionContext {
                route: agent::ModelRouteFacts {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    provider: Some(agent::ModelProviderKind::Other),
                    ..Default::default()
                },
                family_defaults: agent::FamilyModelDefaults {
                    sonnet: Some("sonnet".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
        }));
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: Some(lingxi_core::host::SubagentSpawnRequest {
                    model: Some("sonnet".into()),
                    ..Default::default()
                }),
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "worker".into(),
                team_name: "session".into(),
                description: "inspect".into(),
            },
            ctx(fs, runtime),
        )
        .await
        .unwrap();
    assert_eq!(
        handle.teammate_model_route.as_ref(),
        Some(&("sonnet".into(), Some("provider-a".into())))
    );
}

#[tokio::test]
async fn explicit_child_model_ambiguity_fails_context_admission() {
    use agent::model_resolution::{
        FamilyModelDefaults, ModelProviderKind, ModelResolutionContext, ModelResolutionError,
        ModelRouteFacts,
    };

    let provider_queries = Arc::new(StdMutex::new(Vec::new()));
    let observed_queries = provider_queries.clone();
    let handler = model_test_handler(Some("parent-model")).with_model_resolution_context_provider(
        Arc::new(move |model: &str, profile: Option<&str>| {
            observed_queries
                .lock()
                .unwrap()
                .push((model.to_owned(), profile.map(str::to_owned)));
            match (model, profile) {
                ("parent-model", _) => Ok(ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: model.into(),
                        profile: Some("provider-a".into()),
                        provider: Some(ModelProviderKind::Other),
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        sonnet: Some("shared-child-model".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                ("shared-child-model", None) => Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: vec!["provider-a".into(), "provider-b".into()],
                }),
                _ => Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    reason: "unexpected test route".into(),
                }),
            }
        }),
    );
    let mut definition = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    definition.model = AgentModel::Explicit("shared-child-model".into());

    let result = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "lead",
            "",
            "",
            definition,
        )
        .await;
    let error = match result {
        Err(TaskError::Internal(message)) => message,
        Err(error) => panic!("unexpected context-admission error: {error}"),
        Ok(_) => panic!("ambiguous explicit model must fail before task admission"),
    };
    assert_eq!(
        error,
        ModelResolutionError::AmbiguousRoute {
            model: "shared-child-model".into(),
            profiles: vec!["provider-a".into(), "provider-b".into()],
        }
        .to_string(),
        "the explicit wire model must fail with the resolver's exact multi-profile cause"
    );
    assert_eq!(
        *provider_queries.lock().unwrap(),
        vec![
            ("parent-model".into(), None),
            ("shared-child-model".into(), Some("provider-a".into())),
            ("shared-child-model".into(), None),
        ],
        "a concrete child first tries the parent profile, then preserves the global ambiguity"
    );
}

#[tokio::test]
async fn build_context_does_not_fall_back_to_boot_route_when_live_selection_is_unavailable() {
    let live_selection: Arc<std::sync::OnceLock<agent::DefaultModelSelectionProvider>> =
        Arc::new(std::sync::OnceLock::new());
    assert!(live_selection.set(Arc::new(|| Ok(None))).is_ok());
    let handler = model_test_handler(Some("boot-model"))
        .with_default_model_selection_provider_handle(live_selection);
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();

    let result = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await;
    let Err(error) = result else {
        panic!("missing live route must not silently select boot provider");
    };
    assert!(error
        .to_string()
        .contains("model/provider selection is unavailable"));
}

// ---- #15: opusplan + plan mode resolves an Inherit teammate to Opus -------
//
// The composition root threads `with_permission_mode(cfg.permission_mode)` +
// `with_model_setting(cfg.default_model)` (the RAW user alias, e.g.
// "opusplan") onto the handler alongside
// `with_default_model(resolve_user_specified_model(orch_cfg.model))` — the
// RESOLVED main-loop id (Sonnet for an opusplan install). To stay FAITHFUL to
// production these tests derive the parent the SAME way: feed
// `resolve_user_specified_model("opusplan")` (= "claude-sonnet-5" since the
// 2.1.197 sonnet-family default flip, M1) as
// `with_default_model`, not a hand-picked literal the wired path never emits.
// These three together drive `resolve_agent_model`'s `getRuntimeMainLoopModel`
// branch (model.ts:145-167), proving the wired path end-to-end: an
// `AgentModel::Inherit` teammate on an `opusplan` install IN PLAN MODE resolves
// to Opus (without `[1m]`), NOT the resolved Sonnet main-loop model — i.e. the
// plan-mode swap fires through the builders the composition root populates.

#[tokio::test]
async fn build_context_opusplan_plan_mode_resolves_inherit_to_opus() {
    // Pin firstParty so `getDefaultOpusModel()` is deterministic regardless of
    // any provider env this process inherits.
    let _g = OpusEnvGuard::clear_providers();
    // Derive the parent EXACTLY as the composition root does: resolve the raw
    // "opusplan" alias to the main-loop wire id (Sonnet outside plan mode) —
    // proving the swap below is to OPUS, not a pass-through of a literal.
    let parent = agent::model_resolution::resolve_user_specified_model(
        "opusplan",
        &agent::ModelResolutionContext {
            family_defaults: agent::FamilyModelDefaults {
                opus: Some("claude-opus-4-8".into()),
                sonnet: Some("claude-sonnet-5".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    let handler = model_test_handler(Some(&parent))
        .with_permission_mode(PermissionMode::Plan)
        .with_model_setting("opusplan");
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await
        .expect("context should build");
    assert!(
        matches!(&ctx.agent_definition.model, AgentModel::Explicit(m) if m == "claude-opus-4-8"),
        "opusplan + plan mode must resolve an Inherit teammate to Opus, got {:?}",
        ctx.agent_definition.model
    );
}

#[tokio::test]
async fn build_context_opusplan_default_mode_returns_resolved_parent() {
    // Same opusplan setting but NOT in plan mode → the Inherit branch returns
    // the resolved main-loop model unchanged (Sonnet), proving the swap is
    // gated on plan mode (not on the setting alone). Parent derived via the
    // resolver, exactly as the composition root produces it.
    let _g = OpusEnvGuard::clear_providers();
    let parent = agent::model_resolution::resolve_user_specified_model(
        "opusplan",
        &agent::ModelResolutionContext {
            family_defaults: agent::FamilyModelDefaults {
                opus: Some("claude-opus-4-8".into()),
                sonnet: Some("claude-sonnet-5".into()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        parent, "claude-sonnet-5",
        "opusplan resolves to Sonnet outside plan mode"
    );
    let handler = model_test_handler(Some(&parent))
        .with_permission_mode(PermissionMode::Default)
        .with_model_setting("opusplan");
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "lead")
        .await
        .unwrap();
    let ctx = handler
        .build_context(lingxi_core::types::AgentId::new(), "lead", "", "", def)
        .await
        .expect("context should build");
    assert!(
        matches!(&ctx.agent_definition.model, AgentModel::Explicit(m) if m == "claude-sonnet-5"),
        "opusplan outside plan mode must keep the resolved parent (Sonnet), got {:?}",
        ctx.agent_definition.model
    );
}

/// RAII guard that clears the three provider env vars (Bedrock/Vertex/Foundry)
/// for the duration of a test so `getDefaultOpusModel()` resolves on the
/// firstParty branch deterministically (mirrors `model_resolution`'s test
/// guard). Restores prior values on drop.
struct OpusEnvGuard {
    prev: Vec<(&'static str, Option<String>)>,
}
impl OpusEnvGuard {
    fn clear_providers() -> Self {
        let keys = [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ];
        let prev = keys
            .iter()
            .map(|k| {
                let v = std::env::var(k).ok();
                std::env::remove_var(k);
                (*k, v)
            })
            .collect();
        Self { prev }
    }
}
impl Drop for OpusEnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.prev {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

#[tokio::test]
async fn name_type_and_supports_messages() {
    let api = ScriptedApiClient::new(vec![]);
    let (_d, _fs, _rt, handler) = make_handler(api);
    assert_eq!(handler.name(), "in_process_teammate");
    assert_eq!(handler.task_type(), TaskType::InProcessTeammate);
    assert!(handler.supports_messages(), "teammates accept messages");
}

#[tokio::test]
async fn non_teammate_input_is_rejected() {
    let api = ScriptedApiClient::new(vec![]);
    let (_d, fs, rt, handler) = make_handler(api);
    let res = handler
        .spawn(
            TaskSpawnInput::LocalBash {
                command: "echo".into(),
                timeout: None,
                tool_use_id: None,
            },
            ctx(fs, rt),
        )
        .await;
    assert!(matches!(res, Err(TaskError::Internal(_))));
}

#[tokio::test]
async fn explicit_activation_prevents_provider_and_status_work_before_commit() {
    let api = ScriptedApiClient::new(vec!["answer one"]);
    let api_handle = api.clone();
    let (dir, fs, rt, handler, sink) = make_handler_with_sink(api);
    sink.require_activation();
    let c = ctx(fs.clone(), rt);
    let mut handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: String::new(),
                description: "work".into(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(api_handle.call_count(), 0, "provider must wait for commit");
    assert!(sink.statuses.lock().unwrap().is_empty());

    handle.activate();
    let spool = dir.path().join(format!("{}.output", handle.task_id));
    let body = await_spool(&fs, spool.to_str().unwrap(), |body| {
        body.contains("answer one")
    })
    .await;
    assert!(body.contains("answer one"));
    assert!(api_handle.call_count() >= 1);
    handler.kill(&handle.task_id, c).await.unwrap();
}

#[tokio::test]
async fn spawn_send_message_then_kill_lifecycle() {
    // The persistent runner runs turn-set 1 immediately on spawn (empty
    // history -> "answer one"), then parks. An injected message un-idles it
    // and drives turn-set 2 ("answer two"), proving persistence (the slot
    // did not terminate after turn-set 1). The streaming worker spools a
    // `completed:` line per turn-set. kill then tears the slot down.
    let api = ScriptedApiClient::new(vec!["answer one", "answer two"]);
    let api_handle = api.clone();
    let (dir, fs, rt, handler, sink) = make_handler_with_sink(api);
    let c = ctx(fs.clone(), rt.clone());
    let agent_id = lingxi_core::types::AgentId::new();

    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id,
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();
    assert!(h.task_id.starts_with('t'), "teammate ids prefix 't'");
    assert!(h.cleanup.is_some(), "cleanup hook present");
    assert_eq!(
        handler.entries.lock().await.len(),
        1,
        "spawn registers slot"
    );

    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();

    // Turn-set 1 completes shortly after spawn.
    let body = await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;
    assert!(body.contains("completed:"), "turn-set 1 spooled: {body:?}");
    assert!(body.contains("answer one"), "turn-set 1 text: {body:?}");
    assert_eq!(
        sink.team_activity.lock().unwrap().as_slice(),
        &[(agent_id, true)],
        "the team member becomes active after the pool actually allocates"
    );

    // Inject a message — drives turn-set 2 after the runner had idled.
    handler
        .send_message(&h.task_id, "follow-up question".into(), c.clone())
        .await
        .expect("send_message routes to the live slot while idle");
    let body = await_spool(&fs, &spool_str, |b| b.contains("answer two")).await;
    assert!(body.contains("answer two"), "turn-set 2 text: {body:?}");
    assert_eq!(api_handle.call_count(), 2, "one round-trip per turn-set");
    assert_eq!(
        sink.team_activity.lock().unwrap().as_slice(),
        &[(agent_id, true), (agent_id, true)],
        "a successfully injected UserMessage re-enters active query state"
    );

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while sink.idle_tasks.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both completed turns publish idle");
    assert_eq!(
        sink.idle_tasks.lock().unwrap().as_slice(),
        &[h.task_id.clone(), h.task_id.clone()]
    );
    assert_eq!(
        sink.statuses
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, status)| *status == TaskStatus::Running)
            .count(),
        2,
        "initial turn and mailbox wake each publish running"
    );

    // Kill tears it down; the entry is removed.
    handler.kill(&h.task_id, c.clone()).await.unwrap();
    assert_eq!(
        sink.team_activity.lock().unwrap().as_slice(),
        &[(agent_id, true), (agent_id, true), (agent_id, false)],
        "kill writes inactive before sending UserExit"
    );
    let order = sink.event_order.lock().unwrap();
    let inactive = order
        .iter()
        .rposition(|event| event == "team-active:false")
        .unwrap();
    let killed = order
        .iter()
        .rposition(|event| event == "status:Killed")
        .unwrap();
    assert!(
        inactive < killed,
        "inactive precedes the terminal status: {order:?}"
    );
    drop(order);
    assert!(
        handler.entries.lock().await.is_empty(),
        "kill deregisters the slot"
    );

    // Killing an unknown id is a graceful no-op.
    handler
        .kill("tdeadbeef", c)
        .await
        .expect("kill of unknown id is a no-op success");
}

#[tokio::test]
async fn send_message_to_unknown_task_is_not_found() {
    let api = ScriptedApiClient::new(vec![]);
    let (_d, fs, rt, handler) = make_handler(api);
    let err = handler
        .send_message("tnope", "hi".into(), ctx(fs, rt))
        .await
        .unwrap_err();
    assert!(matches!(err, TaskError::NotFound(_)), "got {err:?}");
}

/// A teammate whose first turn-set hits an API error surfaces `Failed`: the
/// streaming worker spools a `failed:` line and reports `TaskStatus::Failed`
/// (the terminal-break branch — distinct from the non-terminal per-turn-set
/// `Completed`).
#[tokio::test]
async fn failed_turn_set_spools_failed_and_reports_terminal() {
    let api = ScriptedApiClient::new_error("boom");
    let (dir, fs, rt, handler, sink) = make_handler_with_sink(api);
    let c = ctx(fs.clone(), rt.clone());

    let agent_id = lingxi_core::types::AgentId::new();
    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id,
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c,
        )
        .await
        .unwrap();

    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();

    let body = await_spool(&fs, &spool_str, |b| b.contains("failed:")).await;
    assert!(body.contains("failed:"), "failed line spooled: {body:?}");
    assert!(body.contains("boom"), "error text spooled: {body:?}");

    assert_eq!(
        await_terminal(&sink).await,
        Some(TaskStatus::Failed),
        "Failed event reports terminal TaskStatus::Failed"
    );
    assert_eq!(
        sink.team_activity.lock().unwrap().as_slice(),
        &[(agent_id, true), (agent_id, false)],
        "failure closes the team activity interval"
    );
    let order = sink.event_order.lock().unwrap();
    let inactive = order
        .iter()
        .position(|event| event == "team-active:false")
        .unwrap();
    let failed = order.iter().position(|event| event == "failed").unwrap();
    assert!(
        inactive < failed,
        "inactive precedes failure publication: {order:?}"
    );
}

/// cc 2.1.198 (M9): a teammate dying on an API error reports "failed" WITH the
/// failure reason through the `set_failed` seam — the reason the lead-facing
/// `CoordinatorStatusSink` surfaces on the worker (binary @216293689: the
/// in-process runner's catch sends `{idleReason:"failed",
/// completedStatus:"failed", failureReason}` to the leader). Not a bare
/// `set_status(Failed)` that would drop the error text.
#[tokio::test]
async fn failed_turn_set_reports_error_reason_through_set_failed() {
    let api = ScriptedApiClient::new_error("rate limited: 529 overloaded");
    let (_dir, fs, rt, handler, sink) = make_handler_with_sink(api);
    let c = ctx(fs.clone(), rt.clone());

    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c,
        )
        .await
        .unwrap();

    assert_eq!(await_terminal(&sink).await, Some(TaskStatus::Failed));
    let failures = sink.failures();
    assert_eq!(failures.len(), 1, "exactly one set_failed: {failures:?}");
    assert_eq!(failures[0].0, h.task_id, "keyed on the teammate task id");
    assert!(
        failures[0].1.contains("rate limited: 529 overloaded"),
        "the REAL error text reaches the sink (lead), not a sentinel: {failures:?}"
    );
}

/// Once an out-of-band runner exit reaches the handler worker, terminal cleanup
/// removes both the pool slot and the live control block. Later messages must
/// observe the teammate as absent rather than targeting a stale entry.
#[tokio::test]
async fn send_message_after_runner_terminated_is_not_found() {
    let api = ScriptedApiClient::new(vec!["answer one"]);
    let (dir, fs, rt, handler) = make_handler(api);
    let c = ctx(fs.clone(), rt.clone());

    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    // Let turn-set 1 land so the runner is parked and reachable.
    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();
    await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;

    // Tear down the underlying slot out-of-band. The streaming worker observes
    // the closed output channel and performs the same terminal cleanup as any
    // other runner exit.
    let aid = handler
        .entries
        .lock()
        .await
        .get(&h.task_id)
        .map(|e| e.agent_id)
        .unwrap();
    handler
        .pool
        .send_event(&aid, lingxi_core::Event::UserExit)
        .await
        .unwrap();
    // Wait until the slot's inbound channel is observably closed.
    for _ in 0..400 {
        if handler
            .pool
            .send_event(&aid, lingxi_core::Event::UserInterrupt)
            .await
            .is_err()
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    await_entry_removed(&handler, &h.task_id).await;

    let err = handler
        .send_message(&h.task_id, "are you there?".into(), c)
        .await
        .unwrap_err();
    assert!(
        matches!(err, TaskError::NotFound(_)),
        "terminal cleanup removes the stale teammate entry; got {err:?}"
    );
}

/// Like [`make_handler`] but attaches a [`RecordingIdleFirer`] (plus a
/// `RecordingSink`) so a test can observe the per-turn-set `TeammateIdle`
/// fire.
fn make_handler_with_idle_firer(
    api: Arc<ScriptedApiClient>,
) -> (
    tempfile::TempDir,
    Arc<dyn FileSystem>,
    Arc<MockRuntimeSpawner>,
    InProcessTeammateHandler,
    Arc<RecordingIdleFirer>,
) {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let firer = Arc::new(RecordingIdleFirer::default());
    let handler = InProcessTeammateHandler::new(pool, output, api)
        .with_teammate_idle_firer(firer.clone() as Arc<dyn hooks::TeammateIdleFirer>);
    (dir, fs, runtime, handler, firer)
}

/// Yield until the firer has recorded at least `n` fires, or the budget runs
/// out. Returns the recorded fires.
#[allow(clippy::similar_names)] // `fires` is the plural noun form of `firer`'s fires method
async fn await_fires(firer: &Arc<RecordingIdleFirer>, n: usize) -> Vec<hooks::TeammateIdleFire> {
    for _ in 0..400 {
        let fires = firer.fires();
        if fires.len() >= n {
            return fires;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    firer.fires()
}

/// A wired `TeammateIdleFirer` receives one fire per completed turn-set —
/// the "about to go idle" moment — carrying the teammate name and the team
/// name threaded from the spawn input. Driving a second turn-set via an
/// injected message proves it fires again each time the teammate parks.
#[tokio::test]
#[allow(clippy::similar_names)] // `fires` (results) vs `firer` (sender) are semantically distinct
async fn completed_turn_set_fires_teammate_idle_hook() {
    let api = ScriptedApiClient::new(vec!["answer one", "answer two"]);
    let (dir, fs, rt, handler, firer) = make_handler_with_idle_firer(api);
    let c = ctx(fs.clone(), rt.clone());

    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: Some(lingxi_core::host::subagent_spawn::SubagentSpawnRequest {
                    origin_session_id: Some(lingxi_core::types::SessionId::nil()),
                    mode: Some("plan".into()),
                    ..Default::default()
                }),
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();

    // Turn-set 1 completes → exactly one idle fire so far.
    await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;
    let fires = await_fires(&firer, 1).await;
    assert_eq!(fires.len(), 1, "one idle fire after turn-set 1: {fires:?}");
    assert_eq!(fires[0].session_id, lingxi_core::types::SessionId::nil());
    assert_eq!(fires[0].permission_mode, "plan");
    assert_eq!(fires[0].teammate_name, "buddy", "carries the teammate name");
    assert_eq!(
        fires[0].team_name, "alpha",
        "team_name is threaded from the spawn input (claude-code getTeamName())"
    );

    // Inject a message → drives turn-set 2, which parks again → a 2nd fire.
    handler
        .send_message(&h.task_id, "follow-up".into(), c.clone())
        .await
        .unwrap();
    await_spool(&fs, &spool_str, |b| b.contains("answer two")).await;
    let fires = await_fires(&firer, 2).await;
    assert_eq!(fires.len(), 2, "a fire per completed turn-set: {fires:?}");
    assert_eq!(fires[1].session_id, fires[0].session_id);
    assert_eq!(fires[1].permission_mode, "plan");

    handler.kill(&h.task_id, c).await.unwrap();
}

/// With NO firer wired (the default), a completed turn-set is a strict no-op
/// on the hook path: the teammate still runs and parks normally (the spool
/// shows the turn-set), proving the fire is purely additive and absent.
#[tokio::test]
async fn no_idle_firer_is_a_noop() {
    let api = ScriptedApiClient::new(vec!["answer one"]);
    // make_handler builds the handler WITHOUT a teammate idle firer.
    let (dir, fs, rt, handler) = make_handler(api);
    let c = ctx(fs.clone(), rt.clone());

    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: "alpha".into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    // The turn-set completes and parks exactly as before — no firer, no
    // panic, no behavioral change.
    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();
    let body = await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;
    assert!(
        body.contains("completed:"),
        "turn-set still completes: {body:?}"
    );

    handler.kill(&h.task_id, c).await.unwrap();
}

#[test]
fn idle_hook_follow_up_preserves_feedback_and_additional_context_bytes() {
    let outcome = hooks::TeammateIdleOutcome {
        blocking_feedback: vec!["TeammateIdle hook feedback:\nkeep working".into()],
        additional_contexts: vec!["inspect the failing test".into(), "then rerun it".into()],
        ..Default::default()
    };
    assert_eq!(
        teammate_idle_follow_up(&outcome).as_ref().map(|text| text.display.as_str()),
        Some(
            "TeammateIdle hook feedback:\nkeep working\n\n<system-reminder>\nTeammateIdle hook additional context: inspect the failing test\nthen rerun it\n</system-reminder>"
        )
    );
}

#[test]
fn teammate_idle_message_event_keeps_isolated_utf16_units() {
    let text = hooks::ExactHookText::from_utf16(vec![u16::from(b'x'), 0xD800]);
    let event = teammate_idle_user_message_event(text);
    let lingxi_core::Event::UserMessageJsUtf16 {
        content,
        utf16_code_units,
        ..
    } = event
    else {
        panic!("isolated surrogate must use the exact user-message event");
    };
    assert_eq!(content, "x�");
    assert_eq!(utf16_code_units, [u16::from(b'x'), 0xD800]);
}

#[tokio::test]
async fn blocking_idle_hook_feedback_drives_one_follow_up_turn() {
    let api = ScriptedApiClient::new(vec!["answer one", "answer two"]);
    let (dir, fs, rt, handler, firer) = make_handler_with_idle_firer(api);
    firer.push_outcome(hooks::TeammateIdleOutcome {
        blocking_feedback: vec!["TeammateIdle hook feedback:\nkeep working".into()],
        ..Default::default()
    });
    let c = ctx(fs.clone(), rt);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: String::new(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();
    let spool = dir.path().join(format!("{}.output", handle.task_id));
    let body = await_spool(&fs, spool.to_str().unwrap(), |body| {
        body.contains("answer two")
    })
    .await;
    assert!(
        body.contains("answer two"),
        "blocking feedback must re-wake the teammate"
    );
    handler.kill(&handle.task_id, c).await.unwrap();
}

#[tokio::test]
async fn idle_hook_prevent_continuation_terminates_the_teammate() {
    let api = ScriptedApiClient::new(vec!["answer one"]);
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let sink = Arc::new(RecordingSink::default());
    let firer = Arc::new(RecordingIdleFirer::default());
    firer.push_outcome(hooks::TeammateIdleOutcome {
        prevent_continuation: true,
        reason: Some("stop now".into()),
        ..Default::default()
    });
    let handler = InProcessTeammateHandler::new(pool, output, api)
        .with_status_sink(sink.clone())
        .with_teammate_idle_firer(firer);
    let c = ctx(fs, runtime);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: String::new(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    assert_eq!(await_terminal(&sink).await, Some(TaskStatus::Completed));
    await_entry_removed(&handler, &handle.task_id).await;
    let error = handler
        .send_message(&handle.task_id, "too late".into(), c.clone())
        .await
        .unwrap_err();
    assert!(matches!(error, TaskError::NotFound(_)));
    handler.kill(&handle.task_id, c).await.unwrap();
}

#[tokio::test]
async fn hook_follow_up_injection_failure_cleans_status_entry_and_pool_slot() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let sink = Arc::new(RecordingSink::default());
    let firer = GatedIdleFirer::new();
    let handler = InProcessTeammateHandler::new(
        pool.clone(),
        output,
        ScriptedApiClient::new(vec!["answer one"]),
    )
    .with_status_sink(sink.clone())
    .with_teammate_idle_firer(firer.clone());
    let agent_id = lingxi_core::types::AgentId::new();
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id,
                name: "buddy".into(),
                team_name: String::new(),
                description: "work".into(),
            },
            ctx(fs, runtime),
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), firer.started.acquire())
        .await
        .expect("idle hook starts")
        .unwrap()
        .forget();
    pool.deallocate(&agent_id).await.unwrap();
    firer.release.add_permits(1);

    assert_eq!(await_terminal(&sink).await, Some(TaskStatus::Failed));
    for _ in 0..200 {
        if handler.entries.lock().await.is_empty() && pool.slot_count().await == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(handler.entries.lock().await.is_empty());
    assert_eq!(pool.slot_count().await, 0);
    assert!(matches!(
        handler
            .send_message(
                &handle.task_id,
                "too late".into(),
                ctx(
                    Arc::new(InMemoryFs::new()),
                    Arc::new(MockRuntimeSpawner::default()),
                ),
            )
            .await,
        Err(TaskError::NotFound(_))
    ));
}

#[tokio::test]
async fn explicit_kill_during_idle_hook_is_not_overwritten_by_late_feedback() {
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let sink = Arc::new(RecordingSink::default());
    let firer = GatedIdleFirer::new();
    let handler =
        InProcessTeammateHandler::new(pool, output, ScriptedApiClient::new(vec!["answer one"]))
            .with_status_sink(sink.clone())
            .with_teammate_idle_firer(firer.clone());
    let c = ctx(fs, runtime);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: String::new(),
                description: "work".into(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), firer.started.acquire())
        .await
        .expect("idle hook starts")
        .unwrap()
        .forget();
    handler.kill(&handle.task_id, c).await.unwrap();
    assert_eq!(sink.last_status(), Some(TaskStatus::Killed));

    firer.release.add_permits(1);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    assert_eq!(
        sink.last_status(),
        Some(TaskStatus::Killed),
        "late hook feedback must not replace an explicit kill"
    );
    assert!(sink.failures().is_empty());
}

#[tokio::test]
async fn message_received_while_busy_waits_for_the_idle_poll() {
    let api = GatedApiClient::new();
    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        8,
    ));
    let handler = InProcessTeammateHandler::new(pool, output, api.clone());
    let c = ctx(fs, runtime);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: String::new(),
                description: "initial".into(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        api.first_started.acquire(),
    )
    .await
    .expect("first provider request starts")
    .unwrap()
    .forget();
    handler
        .send_message(
            &handle.task_id,
            "queued one\n\nqueued two".into(),
            c.clone(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        api.calls.load(Ordering::SeqCst),
        1,
        "a busy teammate must not cancel and restart its provider request"
    );

    api.release_first.add_permits(1);
    for _ in 0..300 {
        if api.calls.load(Ordering::SeqCst) >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert_eq!(api.calls.load(Ordering::SeqCst), 2);
    {
        let histories = api.histories.lock().unwrap();
        assert!(
            histories[1]
                .iter()
                .any(|message| message.text_content() == "queued one\n\nqueued two"),
            "the drained mailbox batch becomes one next turn after idle"
        );
    }
    handler.kill(&handle.task_id, c).await.unwrap();
}

/// `TeammateIdleFire`'s payload maps 1:1 to `HookEvent::TeammateIdle`'s
/// wire fields — a guard that the seam's struct stays aligned with the event.
#[test]
fn idle_fire_payload_shape() {
    let fire = hooks::TeammateIdleFire {
        session_id: lingxi_core::types::SessionId::nil(),
        permission_mode: "default".into(),
        teammate_name: "buddy".into(),
        team_name: "alpha".into(),
    };
    assert_eq!(fire.teammate_name, "buddy");
    assert_eq!(fire.team_name, "alpha");
}

// ---- Swarm auto-claim (oracle 2.1.223 zvb / Vvb / rIp) -------------------

/// Serializes the env-mutating auto-claim tests (`LINGXI_CONFIG_DIR`).
use crate::handlers::CONFIG_DIR_ENV_LOCK as CLAIM_ENV_LOCK;

/// Point the todo store at a throwaway config dir; restore on drop.
struct ClaimEnvGuard {
    prev_config: Option<std::ffi::OsString>,
    prev_list: Option<std::ffi::OsString>,
    dir: std::path::PathBuf,
    _lock: std::sync::MutexGuard<'static, ()>,
}
impl ClaimEnvGuard {
    fn new() -> Self {
        let lock = CLAIM_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lingxi-teammate-claim-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let guard = Self {
            prev_config: std::env::var_os(branding::CONFIG_DIR_ENV),
            prev_list: std::env::var_os("LINGXI_TASK_LIST_ID"),
            dir: dir.clone(),
            _lock: lock,
        };
        std::env::set_var(branding::CONFIG_DIR_ENV, &dir);
        std::env::remove_var("LINGXI_TASK_LIST_ID");
        guard
    }
}
impl Drop for ClaimEnvGuard {
    fn drop(&mut self) {
        match &self.prev_config {
            Some(v) => std::env::set_var(branding::CONFIG_DIR_ENV, v),
            None => std::env::remove_var(branding::CONFIG_DIR_ENV),
        }
        match &self.prev_list {
            Some(v) => std::env::set_var("LINGXI_TASK_LIST_ID", v),
            None => std::env::remove_var("LINGXI_TASK_LIST_ID"),
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn todo(
    subject: &str,
    status: lingxi_core::TodoState,
    owner: Option<&str>,
) -> task_store::TodoTask {
    let mut t =
        task_store::TodoTask::new(subject.into(), "desc".into(), None, serde_json::Map::new());
    t.status = status;
    t.owner = owner.map(str::to_string);
    t
}

#[test]
fn pick_next_task_skips_owned_blocked_and_non_pending() {
    use lingxi_core::TodoState::{Completed, InProgress, Pending};
    let mut blocked = todo("blocked", Pending, None);
    blocked.id = "4".into();
    blocked.blocked_by = vec!["2".into()];
    let mut done_blocked = todo("blocked by done", Pending, None);
    done_blocked.id = "5".into();
    done_blocked.blocked_by = vec!["1".into()];
    let tasks = vec![
        {
            let mut t = todo("done", Completed, None);
            t.id = "1".into();
            t
        },
        {
            let mut t = todo("busy", InProgress, None);
            t.id = "2".into();
            t
        },
        {
            let mut t = todo("owned", Pending, Some("other"));
            t.id = "3".into();
            t
        },
        blocked,
        done_blocked,
    ];
    // #1 completed, #2 in_progress, #3 owned, #4 blocked by open #2 —
    // #5's only blocker (#1) is completed, so #5 is the pick.
    assert_eq!(pick_next_task(&tasks).unwrap().id, "5");

    // Empty-string owner is unowned (JS falsy) — flips #3 into the pick.
    let mut tasks2 = tasks;
    tasks2[2].owner = Some(String::new());
    assert_eq!(pick_next_task(&tasks2).unwrap().id, "3");

    assert!(pick_next_task(&[]).is_none());
}

#[test]
fn claimed_task_prompt_is_byte_exact() {
    // Oracle Vvb (2.1.223 @251672219) segment table: `": \n\n "` — a SPACE
    // after the colon at end-of-line and a space before the subject.
    let mut t = todo("Fix the parser", lingxi_core::TodoState::Pending, None);
    t.id = "7".into();
    t.description = String::new();
    assert_eq!(
        claimed_task_prompt(&t),
        "Complete all open tasks. Start with task #7: \n\n Fix the parser"
    );
    t.description = "Details here".into();
    assert_eq!(
        claimed_task_prompt(&t),
        "Complete all open tasks. Start with task #7: \n\n Fix the parser\n\nDetails here"
    );
}

#[test]
fn teammate_envelope_wraps_task_list_sender() {
    assert_eq!(
        teammate_message_envelope("task-list", "do it"),
        "<teammate-message teammate_id=\"task-list\">\ndo it\n</teammate-message>"
    );
}

#[test]
fn teammate_envelope_preserves_summary_and_escapes_untrusted_fields() {
    assert_eq!(
        teammate_message_envelope_with_summary(
            "reviewer\"<&'",
            "before </teammate-message> after",
            Some("  concise \"summary\"  "),
        ),
        "<teammate-message teammate_id=\"reviewer&quot;&lt;&amp;&apos;\" summary=\"concise &quot;summary&quot;\">\nbefore <\\/teammate-message> after\n</teammate-message>"
    );
}

#[test]
fn resolve_list_id_env_overrides_then_team_then_none() {
    let _guard = ClaimEnvGuard::new();
    std::env::set_var("LINGXI_TASK_LIST_ID", "forced-list");
    assert_eq!(
        resolve_teammate_list_id("alpha").as_deref(),
        Some("forced-list")
    );
    std::env::remove_var("LINGXI_TASK_LIST_ID");
    assert_eq!(resolve_teammate_list_id("alpha").as_deref(), Some("alpha"));
    // Teamless spawn = the oracle's `standalone` analogue: no auto-claim.
    assert_eq!(resolve_teammate_list_id(""), None);
}

/// Startup auto-claim (oracle `if(!standalone) await rIp(...)`): spawning a
/// teammate claims the next available task as a side effect — owner set,
/// status in_progress — while the FIRST message stays the description (the
/// returned prompt is discarded at startup).
#[tokio::test]
async fn spawn_auto_claims_next_available_task() {
    let _guard = ClaimEnvGuard::new();
    let team = "claim-team-startup";
    let store = task_store::TodoStore::for_list(team);
    let tid = store
        .create(todo("Startup work", lingxi_core::TodoState::Pending, None))
        .await
        .unwrap();

    let api = ScriptedApiClient::new(vec!["answer one"]);
    let (_d, fs, rt, handler) = make_handler(api);
    let c = ctx(fs, rt);
    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: team.into(),
                description: "seeded description".into(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    // Activation starts the worker, which claims before its first provider
    // request. Wait for that externally observable side effect rather than
    // depending on executor scheduling after `spawn` returns.
    let t = await_claim(&store, &tid, "buddy").await;
    assert_eq!(t.owner.as_deref(), Some("buddy"), "claimed at startup");
    assert_eq!(t.status, lingxi_core::TodoState::InProgress);

    handler.kill(&h.task_id, c).await.unwrap();
}

#[tokio::test]
async fn pool_allocation_failure_rolls_back_startup_claim() {
    let _guard = ClaimEnvGuard::new();
    let team = "claim-team-pool-full";
    let store = task_store::TodoStore::for_list(team);
    let task_id = store
        .create(todo(
            "Must remain available",
            lingxi_core::TodoState::Pending,
            None,
        ))
        .await
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let runtime = Arc::new(MockRuntimeSpawner::default());
    let output = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let pool = Arc::new(StateMachinePool::new(
        runtime.clone() as Arc<dyn RuntimeSpawner>,
        0,
    ));
    let sink = Arc::new(RecordingSink::default());
    sink.require_activation();
    let handler =
        InProcessTeammateHandler::new(pool, output, ScriptedApiClient::new(vec!["unused"]))
            .with_status_sink(sink.clone());
    let mut handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: team.into(),
                description: "work".into(),
            },
            ctx(fs, runtime),
        )
        .await
        .unwrap();

    handle.activate();
    assert_eq!(await_terminal(&sink).await, Some(TaskStatus::Failed));
    let task = store.get(&task_id).await.unwrap();
    assert_eq!(task.owner, None, "failed startup must release the claim");
    assert_eq!(task.status, lingxi_core::TodoState::Pending);
    assert!(handler.entries.lock().await.is_empty());
}

/// Embedded hosts inject an app-private config home for the Task* tools and
/// reminders; teammate auto-claim must read that SAME task-list root instead of
/// the process-global `HOME` / `LINGXI_CONFIG_DIR`.
#[tokio::test]
async fn spawn_auto_claims_next_available_task_from_injected_config_home() {
    let guard = ClaimEnvGuard::new();
    let team = "claim-team-explicit-home";
    let config_home = guard.dir.join("host-owned-config");
    let store = task_store::TodoStore::for_list_at(&config_home, team);
    let tid = store
        .create(todo(
            "Host-owned work",
            lingxi_core::TodoState::Pending,
            None,
        ))
        .await
        .unwrap();

    let api = ScriptedApiClient::new(vec!["answer one"]);
    let (_d, fs, rt, handler) = make_handler(api);
    let handler = handler.with_config_home(config_home.clone());
    let c = ctx(fs, rt);
    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: team.into(),
                description: "seeded description".into(),
            },
            c.clone(),
        )
        .await
        .unwrap();

    let t = await_claim(&store, &tid, "buddy").await;
    assert_eq!(
        t.owner.as_deref(),
        Some("buddy"),
        "startup claim must read the injected config home"
    );
    assert_eq!(t.status, lingxi_core::TodoState::InProgress);
    assert!(
        task_store::TodoStore::for_list(team)
            .list()
            .await
            .is_empty(),
        "legacy env-root store must stay untouched"
    );

    handler.kill(&h.task_id, c).await.unwrap();
}

/// Idle auto-claim (oracle poll loop @251675509): a task created AFTER the
/// teammate parks is claimed by the 500ms idle tick and its Vvb prompt is
/// self-injected as the next user message, driving turn-set 2.
#[tokio::test]
async fn idle_poll_claims_late_task_and_drives_next_turn_set() {
    let _guard = ClaimEnvGuard::new();
    let team = "claim-team-idle";
    let store = task_store::TodoStore::for_list(team);

    let api = ScriptedApiClient::new(vec!["answer one", "answer two"]);
    let api_handle = api.clone();
    let (dir, fs, rt, handler) = make_handler(api);
    let c = ctx(fs.clone(), rt);
    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: team.into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();
    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();

    // Turn-set 1 completes and the teammate parks (idle window opens).
    let body = await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;
    assert!(body.contains("completed:"), "turn-set 1 parked: {body:?}");

    // NOW a task appears on the shared list.
    let tid = store
        .create(todo("Late work", lingxi_core::TodoState::Pending, None))
        .await
        .unwrap();

    // Within a few ticks the idle poller claims it and injects the prompt,
    // waking the runner into turn-set 2.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let t = store.get(&tid).await.unwrap();
        if t.owner.as_deref() == Some("buddy")
            && t.status == lingxi_core::TodoState::InProgress
            && api_handle.call_count() >= 2
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "idle poller never claimed the late task: owner={:?} status={:?} calls={}",
            t.owner,
            t.status,
            api_handle.call_count()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let body = await_spool(&fs, &spool_str, |b| b.contains("answer two")).await;
    assert!(body.contains("answer two"), "turn-set 2 ran: {body:?}");

    handler.kill(&h.task_id, c).await.unwrap();
}

/// After kill the idle poller stops: a task seeded post-kill stays unclaimed.
#[tokio::test]
async fn killed_teammate_stops_claiming() {
    let _guard = ClaimEnvGuard::new();
    let team = "claim-team-killed";
    let store = task_store::TodoStore::for_list(team);

    let api = ScriptedApiClient::new(vec!["answer one"]);
    let (dir, fs, rt, handler) = make_handler(api);
    let c = ctx(fs.clone(), rt);
    let h = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id: lingxi_core::types::AgentId::new(),
                name: "buddy".into(),
                team_name: team.into(),
                description: String::new(),
            },
            c.clone(),
        )
        .await
        .unwrap();
    let spool = dir.path().join(format!("{}.output", h.task_id));
    let spool_str = spool.to_str().unwrap().to_string();
    let _ = await_spool(&fs, &spool_str, |b| b.contains("answer one")).await;

    handler.kill(&h.task_id, c).await.unwrap();

    let tid = store
        .create(todo(
            "Post-kill work",
            lingxi_core::TodoState::Pending,
            None,
        ))
        .await
        .unwrap();
    // Two full tick intervals: a live poller would have claimed by now.
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let t = store.get(&tid).await.unwrap();
    assert_eq!(t.owner, None, "killed teammate must not claim");
    assert_eq!(t.status, lingxi_core::TodoState::Pending);
}

#[tokio::test]
async fn build_context_keeps_session_transcript_storage() {
    let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
    let directory = PathBuf::from("/session/subagents");
    let handler = model_test_handler(None).with_transcript(fs.clone(), directory.clone());
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "worker")
        .await
        .unwrap();
    let context = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "worker",
            "session-12345678",
            "work",
            def,
        )
        .await
        .unwrap();
    assert_eq!(context.transcript_subdir, directory);
    assert!(Arc::ptr_eq(context.transcript_fs.as_ref().unwrap(), &fs));
}

#[tokio::test]
async fn assigned_teammate_color_reaches_actual_agent_context() {
    let handler = model_test_handler(None);
    let def = DefaultTeammateDefinition
        .resolve(&lingxi_core::types::AgentId::new(), "worker")
        .await
        .unwrap();
    let mut context = handler
        .build_context(
            lingxi_core::types::AgentId::new(),
            "worker",
            "session-12345678",
            "work",
            def,
        )
        .await
        .unwrap();
    let provenance = lingxi_core::host::subagent_spawn::AgentSpawnProvenance {
        hook_caller: lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!(
            "trusted-plugin"
        )),
        hook_origin: lingxi_core::host::task_registry::FieldPresence::Value(serde_json::json!([
            "trusted-plugin",
            "parent-api"
        ])),
    };
    apply_spawn_context(
        &mut context,
        lingxi_core::host::SubagentSpawnRequest {
            teammate_color: Some("blue".into()),
            agent_spawn_provenance: provenance.clone(),
            ..Default::default()
        },
    );
    assert_eq!(context.display.color, AgentColor::Blue);
    assert_eq!(context.agent_spawn_provenance, provenance);
}

struct PlanReviewTransport {
    requests: StdMutex<Vec<serde_json::Value>>,
    modes: StdMutex<Vec<String>>,
}
#[async_trait]
impl lingxi_core::host::ToolInvoker for PlanReviewTransport {
    async fn invoke(
        &self,
        name: &str,
        _input: serde_json::Value,
        ctx: lingxi_core::host::tool_invoker::SubagentInvocationContext,
    ) -> Result<serde_json::Value, lingxi_core::host::tool_invoker::ToolInvokerError> {
        let mode = ctx.mode_override.unwrap_or_default();
        self.modes.lock().unwrap().push(mode.clone());
        assert_eq!(ctx.frozen_command_denies, vec!["Bash(rm)"]);
        if name == "Write" && mode == "plan" {
            return Err(lingxi_core::host::tool_invoker::ToolInvokerError::Abort(
                "plan mode disallows mutation".into(),
            ));
        }
        Ok(serde_json::json!({"ok":true}))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
#[async_trait]
impl lingxi_core::host::mailbox::MailboxRouterHandle for PlanReviewTransport {
    async fn route(
        &self,
        from: &str,
        to: &str,
        message: lingxi_core::host::mailbox::MailboxMessage,
    ) -> Result<lingxi_core::host::mailbox::RouteAck, lingxi_core::host::mailbox::MailboxError>
    {
        assert_eq!(from, "planner");
        assert_eq!(to, "team-lead");
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_str(&message.content).unwrap());
        Ok(lingxi_core::host::mailbox::RouteAck {
            claimed_at: std::time::SystemTime::now(),
            claim_window_secs: 30,
        })
    }
}
fn plan_invocation() -> lingxi_core::host::tool_invoker::SubagentInvocationContext {
    lingxi_core::host::tool_invoker::SubagentInvocationContext {
        input_projection: None,
        cancellation_token: lingxi_core::host::CancellationToken::new(),
        permission_pause_observer: None,
        instruction_context: None,
        fork_context: None,
        tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
        parent_agent_id: None,
        origin_session_id: None,
        agent_name: Some("planner".into()),
        team_name: Some("team".into()),
        is_async: false,
        is_non_interactive_session: false,
        can_show_permission_prompts: true,
        cwd: None,
        tool_use_id: None,
        assistant_message_id: None,
        depth: 1,
        observer: None,
        parent_model: None,
        parent_model_profile: None,
        agent_spawn_provenance: Default::default(),
        tool_context_state: None,
        assistant_message: None,
        same_turn_tool_uses: Vec::new(),
        current_history: Vec::new(),
        mode_override: Some("plan".into()),
        request_source: None,
        frozen_command_denies: vec!["Bash(rm)".into()],
    }
}
#[tokio::test]
async fn lead_review_rejection_keeps_plan_then_approval_changes_next_tool_permission() {
    use lingxi_core::host::{
        teammate_plan::{PlanApprovalResponse, TeammatePlanRequester},
        ToolInvoker,
    };
    let transport = Arc::new(PlanReviewTransport {
        requests: StdMutex::new(vec![]),
        modes: StdMutex::new(vec![]),
    });
    let sink = Arc::new(RecordingSink::default());
    let control = crate::handlers::teammate_plan::PlanAwareInvoker::new(
        transport.clone(),
        transport.clone(),
        Arc::new(InMemoryFs::new()),
        "planner".into(),
        "team".into(),
        "/plans/planner.md".into(),
    )
    .with_status_sink(sink.clone(), "task-plan".into());
    let submitted = control
        .submit(serde_json::json!({"plan":"Test the change, then implement"}))
        .await
        .unwrap();
    let id = submitted["requestId"].as_str().unwrap().to_owned();
    assert!(control.awaiting());
    assert!(control
        .invoke("Write", serde_json::json!({}), plan_invocation())
        .await
        .is_err());
    assert_eq!(
        control
            .apply(PlanApprovalResponse {
                request_id: id,
                approved: false,
                feedback: Some("add rollback".into()),
                permission_mode: None
            })
            .await
            .unwrap(),
        "[Plan Rejected] add rollback"
    );
    assert!(control
        .invoke("Write", serde_json::json!({}), plan_invocation())
        .await
        .is_err());
    let next = control
        .submit(serde_json::json!({"plan":"Test, implement and preserve rollback"}))
        .await
        .unwrap();
    let reply = PlanApprovalResponse {
        request_id: next["requestId"].as_str().unwrap().into(),
        approved: true,
        feedback: None,
        permission_mode: Some("default".into()),
    };
    assert_eq!(
        control.apply(reply.clone()).await.unwrap(),
        "[Plan Approved] You can now proceed with implementation"
    );
    assert!(
        control.apply(reply).await.is_none(),
        "duplicate response ignored"
    );
    assert!(control
        .invoke("Write", serde_json::json!({}), plan_invocation())
        .await
        .is_ok());
    assert_eq!(
        *transport.modes.lock().unwrap(),
        vec!["plan", "plan", "default"]
    );
    assert_eq!(
        transport.requests.lock().unwrap()[0]["type"],
        "plan_approval_request"
    );
    assert_eq!(
        *sink.awaiting_plan.lock().unwrap(),
        vec![true, false, true, false],
        "duplicate replies do not add status changes"
    );
}
#[tokio::test]
async fn mismatched_plan_review_never_elevates_permission() {
    use lingxi_core::host::{
        teammate_plan::{PlanApprovalResponse, TeammatePlanRequester},
        ToolInvoker,
    };
    let transport = Arc::new(PlanReviewTransport {
        requests: StdMutex::new(vec![]),
        modes: StdMutex::new(vec![]),
    });
    let control = crate::handlers::teammate_plan::PlanAwareInvoker::new(
        transport.clone(),
        transport,
        Arc::new(InMemoryFs::new()),
        "planner".into(),
        "team".into(),
        "/plans/planner.md".into(),
    );
    control
        .submit(serde_json::json!({"plan":"proposal"}))
        .await
        .unwrap();
    let text = control
        .apply(PlanApprovalResponse {
            request_id: "wrong".into(),
            approved: true,
            feedback: None,
            permission_mode: Some("bypassPermissions".into()),
        })
        .await
        .unwrap();
    assert!(text.starts_with("[Plan Rejected]"));
    assert!(control
        .invoke("Write", serde_json::json!({}), plan_invocation())
        .await
        .is_err());
}

struct PlanModeAvailability(bool);
#[async_trait]
impl lingxi_core::host::PermissionGate for PlanModeAvailability {
    async fn check(&self, _: &str, _: &serde_json::Value) -> lingxi_core::host::PermissionDecision {
        lingxi_core::host::PermissionDecision::Allow
    }
    fn can_request_auto_mode(&self) -> bool {
        self.0
    }
    fn can_request_bypass_permissions(&self) -> bool {
        self.0
    }
}
#[tokio::test]
async fn approved_modes_use_live_host_availability() {
    use lingxi_core::host::{
        teammate_plan::{PlanApprovalResponse, TeammatePlanRequester},
        ToolInvoker,
    };
    for (mode, available, expected) in [
        ("auto", false, "default"),
        ("auto", true, "auto"),
        ("bypassPermissions", false, "default"),
        ("bypassPermissions", true, "bypassPermissions"),
        ("acceptEdits", false, "acceptEdits"),
    ] {
        let transport = Arc::new(PlanReviewTransport {
            requests: StdMutex::new(vec![]),
            modes: StdMutex::new(vec![]),
        });
        let control = crate::handlers::teammate_plan::PlanAwareInvoker::new(
            transport.clone(),
            transport.clone(),
            Arc::new(InMemoryFs::new()),
            "planner".into(),
            "team".into(),
            "/plans/planner.md".into(),
        )
        .with_permission_gate(Some(Arc::new(PlanModeAvailability(available))));
        let result = control
            .submit(serde_json::json!({"plan":"proposal"}))
            .await
            .unwrap();
        control
            .apply(PlanApprovalResponse {
                request_id: result["requestId"].as_str().unwrap().into(),
                approved: true,
                feedback: None,
                permission_mode: Some(mode.into()),
            })
            .await
            .unwrap();
        control
            .invoke("Read", serde_json::json!({}), plan_invocation())
            .await
            .unwrap();
        assert_eq!(transport.modes.lock().unwrap()[0], expected);
    }
}

#[tokio::test]
async fn typed_plan_verdict_waits_for_idle_then_resumes_with_approval_prose() {
    let api = GatedApiClient::new();
    let (dir, fs, rt, handler) = make_handler(api.clone());
    let transport = Arc::new(PlanReviewTransport {
        requests: StdMutex::new(vec![]),
        modes: StdMutex::new(vec![]),
    });
    let sink = Arc::new(RecordingSink::default());
    let handler = handler
        .with_status_sink(sink.clone())
        .with_tool_invoker(transport.clone())
        .with_plan_approval_mailbox(transport);
    let agent_id = lingxi_core::types::AgentId::new();
    let context = ctx(fs.clone(), rt);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                agent_id,
                name: "planner".into(),
                team_name: "team".into(),
                description: "make a plan".into(),
                inheritance: None,
                spawn_request: Some(lingxi_core::host::SubagentSpawnRequest {
                    mode: Some("plan".into()),
                    ..Default::default()
                }),
            },
            context.clone(),
        )
        .await
        .unwrap();
    api.first_started.acquire().await.unwrap().forget();
    let requester = lingxi_core::host::teammate_plan::requester(&agent_id).unwrap();
    let submitted = requester
        .submit(serde_json::json!({"plan":"Scoped proposal"}))
        .await
        .unwrap();
    handler
        .apply_plan_approval(
            &handle.task_id,
            lingxi_core::host::teammate_plan::PlanApprovalResponse {
                request_id: submitted["requestId"].as_str().unwrap().into(),
                approved: true,
                feedback: None,
                permission_mode: Some("default".into()),
            },
            context.clone(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        api.calls.load(Ordering::SeqCst),
        1,
        "approval does not interrupt an active query"
    );
    api.release_first.add_permits(1);
    let spool = dir.path().join(format!("{}.output", handle.task_id));
    await_spool(&fs, spool.to_str().unwrap(), |body| body.contains("second")).await;
    let histories = api.histories.lock().unwrap();
    assert!(histories[1].iter().any(|message| {
        message
            .text_content()
            .contains("[Plan Approved] You can now proceed with implementation")
    }));
    assert!(!histories[1]
        .iter()
        .any(|message| message.text_content().contains("plan_approval_response")));
    drop(histories);
    assert_eq!(*sink.awaiting_plan.lock().unwrap(), vec![true, false]);
    handler.kill(&handle.task_id, context).await.unwrap();
}

#[tokio::test]
async fn teammate_plan_file_comes_from_the_published_identity() {
    // `ay(agentId)`: with an identity published, the teammate's plan file sits
    // in the SESSION's plans directory — the one the write carve-out matches —
    // not under the config home.
    let api = GatedApiClient::new();
    let (dir, fs, rt, handler) = make_handler(api.clone());
    let transport = Arc::new(PlanReviewTransport {
        requests: StdMutex::new(vec![]),
        modes: StdMutex::new(vec![]),
    });
    let plans_dir = dir.path().join(".lingxi/plans");
    let handler = handler
        .with_tool_invoker(transport.clone())
        .with_plan_approval_mailbox(transport)
        .with_plan_files(Arc::new(
            lingxi_core::host::plan_files::PlanFileMatcher::with_identity(
                lingxi_core::host::plan_files::PlanFileIdentity {
                    plans_dir: plans_dir.clone(),
                    slug: "brave-quiet-otter".into(),
                    workshop_enabled: false,
                },
            ),
        ));
    let agent_id = lingxi_core::types::AgentId::new();
    let context = ctx(fs.clone(), rt);
    let handle = handler
        .spawn(
            TaskSpawnInput::InProcessTeammate {
                agent_id,
                name: "planner".into(),
                team_name: "team".into(),
                description: "make a plan".into(),
                inheritance: None,
                spawn_request: Some(lingxi_core::host::SubagentSpawnRequest {
                    mode: Some("plan".into()),
                    ..Default::default()
                }),
            },
            context.clone(),
        )
        .await
        .unwrap();
    api.first_started.acquire().await.unwrap().forget();
    let requester = lingxi_core::host::teammate_plan::requester(&agent_id).unwrap();
    let expected = plans_dir.join(format!("brave-quiet-otter-agent-{}.md", agent_id.as_uuid()));
    assert_eq!(
        requester.writable_plan_path(),
        Some(expected.to_str().unwrap())
    );
    handler.kill(&handle.task_id, context).await.unwrap();
}
