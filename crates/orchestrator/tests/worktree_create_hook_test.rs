//! tool-dispatch chokepoint (`turn_loop.rs`) after a successful `EnterWorktree`.
//!
//! Parity with claude-code: `executeWorktreeCreateHook(slug)` (`utils/hooks.ts:4928`)
//! runs the `WorktreeCreate` hook when a worktree is created. claude-code fires it
//! from the worktree-creation logic (`createWorktreeForSession`); the port
//! creates worktrees only through the registered, turn_loop-dispatched
//! `EnterWorktree` tool, so the fire lives in `dispatch_tool_uses` immediately
//! after the tool succeeds — same TIMING, the fire just lives in the dispatch loop
//! (alongside `PostToolUse`). The wire payload carries only `name` — the requested
//! slug — exactly as claude-code passes the bare slug to the hook.
//!
//! Scenarios:
//! 1. A successful `EnterWorktree` fires `WorktreeCreate`, carrying the requested
//!    slug as `name` and the resolved `path` / `branch` from the tool result.
//! 2. A FAILED `EnterWorktree` (errored create — no worktree exists) does NOT fire
//!    `WorktreeCreate`.
//! 3. A non-worktree tool never fires `WorktreeCreate`.
//! 4. A `WorktreeCreate` hook that itself fails does NOT break the turn
//!    (best-effort, like the `PostToolUse` arm).
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse, ToolUseId};
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

// ---- unused HTTP / Runtime stubs (Builtin hooks never touch them) ----
struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

// ---- Tools ----

/// A stand-in for the real `EnterWorktree` tool: registered under the same
/// `"EnterWorktree"` name and returning the same `{path, branch_name}` result
/// shape the real tool produces. Configurable: succeed with a fixed payload, or
/// always error (an errored create makes no worktree).
struct FakeEnterWorktreeTool {
    fail: bool,
    path: String,
    branch_name: String,
}
#[async_trait]
impl Tool for FakeEnterWorktreeTool {
    fn name(&self) -> &str {
        "EnterWorktree"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        false
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        false
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "enter worktree".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if self.fail {
            return Err(ToolError::Internal(
                "git error: not a git repository".into(),
            ));
        }
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: json!({ "path": self.path, "branch_name": self.branch_name }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A non-worktree tool (different name) that always succeeds.
struct AlwaysOkTool;
#[async_trait]
impl Tool for AlwaysOkTool {
    fn name(&self) -> &str {
        "AlwaysOk"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "ok".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: json!({ "content": "all good" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ---- Recording hook: captures every WorktreeCreate event it sees ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenWorktree {
    name: String,
    path: PathBuf,
    branch: String,
}

struct RecordingHandler {
    log: Arc<Mutex<Vec<SeenWorktree>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-worktree"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::WorktreeCreate { name, path, branch } = event {
            self.log.lock().unwrap().push(SeenWorktree {
                name: name.clone(),
                path: path.clone(),
                branch: branch.clone(),
            });
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
}

/// A `WorktreeCreate` hook that itself FAILS — proves the fire is best-effort.
struct FailingWorktreeHook;
#[async_trait]
impl BuiltinHookHandler for FailingWorktreeHook {
    fn id(&self) -> &str {
        "broken-worktree-hook"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the worktree hook itself blew up".into(),
            exit_code: Some(1),
            response: None,
        }
    }
}

fn builtin_hook(handler_id: &str, event_type: HookEventType) -> HookDefinition {
    HookDefinition {
        id: HookId::new(),
        name: handler_id.into(),
        events: vec![event_type],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: handler_id.into(),
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
    }
}

async fn exec_recording(log: Arc<Mutex<Vec<SeenWorktree>>>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-worktree",
        HookEventType::WorktreeCreate,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log }));
    Arc::new(exec)
}

fn orch_with(
    api: Arc<MockApiClient>,
    hooks: Arc<HookExecutorImpl>,
    tools: ToolRegistry,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(tools),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

/// Two API turns: turn 1 emits a `tool_use` for `tool_name` with `input`, turn 2 ends.
fn two_turn_api(
    tool_use_id: ToolUseId,
    tool_name: &str,
    input: serde_json::Value,
) -> Arc<MockApiClient> {
    Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall { input_projection: None,
                id: tool_use_id.to_string(),
                name: tool_name.into(),
                input,
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        ),
    ]))
}

#[tokio::test]
async fn successful_enter_worktree_fires_worktree_create_with_name_and_path() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "EnterWorktree",
        json!({ "slug": "user/feature" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenWorktree>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeEnterWorktreeTool {
        fail: false,
        path: "/tmp/repo-A/.lingxi/worktrees/user+feature".into(),
        branch_name: "worktree-user+feature".into(),
    }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("make a worktree").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "exactly one WorktreeCreate event must fire: {seen:?}"
    );
    // `name` is the requested slug — the only field on the claude-code wire payload.
    assert_eq!(seen[0].name, "user/feature");
    // The resolved path + branch are threaded as engine-side context.
    assert_eq!(
        seen[0].path,
        PathBuf::from("/tmp/repo-A/.lingxi/worktrees/user+feature")
    );
    assert_eq!(seen[0].branch, "worktree-user+feature");
}

#[tokio::test]
async fn failed_enter_worktree_does_not_fire_worktree_create() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id.clone(),
        "EnterWorktree",
        json!({ "slug": "user/feature" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenWorktree>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeEnterWorktreeTool {
        fail: true,
        path: String::new(),
        branch_name: String::new(),
    }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("make a worktree").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert!(
        seen.is_empty(),
        "an errored EnterWorktree must NOT fire WorktreeCreate: {seen:?}"
    );
}

#[tokio::test]
async fn non_worktree_tool_does_not_fire_worktree_create() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysOk", json!({}));
    let log = Arc::new(Mutex::new(Vec::<SeenWorktree>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysOkTool));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("do it").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    assert!(
        log.lock().unwrap().is_empty(),
        "a non-worktree tool must not fire WorktreeCreate"
    );
}

#[tokio::test]
async fn failing_worktree_create_hook_does_not_break_turn() {
    // The successful EnterWorktree fires WorktreeCreate; the registered hook
    // itself returns a non-success outcome. The turn must STILL complete
    // (best-effort, identical to the PostToolUse arm).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "EnterWorktree",
        json!({ "slug": "user/feature" }),
    );

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-worktree-hook",
        HookEventType::WorktreeCreate,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingWorktreeHook));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeEnterWorktreeTool {
        fail: false,
        path: "/tmp/repo-A/.lingxi/worktrees/user+feature".into(),
        branch_name: "worktree-user+feature".into(),
    }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch
        .run_turn("make a worktree")
        .await
        .expect("turn must succeed despite the worktree hook itself failing");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "a failing WorktreeCreate hook must not break the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}
