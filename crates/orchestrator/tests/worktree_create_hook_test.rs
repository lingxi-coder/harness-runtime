//! WorktreeCreate belongs to the WorktreeManager lifecycle, before creation.
//! The dispatch layer must never synthesize a second after-the-fact event from
//! a tool name. Hook manager lifecycle coverage lives in hooks/worktree_manager.
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
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        static SCHEMA_SOURCE: once_cell::sync::Lazy<
            lingxi_core::types::utf16_json::Utf16JsonProjection,
        > = once_cell::sync::Lazy::new(|| tool_api::tool_trait::generated_builtin_schema(&SCHEMA));
        &SCHEMA_SOURCE
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> bool {
        false
    }
    fn is_read_only(&self, _input: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        false
    }
    async fn validate_input(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
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
    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "enter worktree".into()
    }
    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }
    async fn call(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if self.fail {
            return Err(ToolError::Internal(
                "git error: not a git repository".into(),
            ));
        }
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(
                &json!({ "path": self.path, "branch_name": self.branch_name }),
            ),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
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
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
        static SCHEMA_SOURCE: once_cell::sync::Lazy<
            lingxi_core::types::utf16_json::Utf16JsonProjection,
        > = once_cell::sync::Lazy::new(|| tool_api::tool_trait::generated_builtin_schema(&SCHEMA));
        &SCHEMA_SOURCE
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> bool {
        true
    }
    fn is_read_only(&self, _input: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
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
    async fn description(
        &self,
        _input: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _opts: &DescriptionOptions,
    ) -> String {
        "ok".into()
    }
    async fn prompt(
        &self,
        _opts: &PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        String::new().into()
    }
    async fn call(
        &self,
        _input: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: tool_api::tool_trait::generated_builtin_result(&json!({ "content": "all good" })),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
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
            stdout: String::new().into(),
            stderr: String::new().into(),
            exit_code: None,
            response: None,
        }
    }
}

/// A hook must remain unreachable through a tool that only impersonates the name.
struct FailingWorktreeHook;
#[async_trait]
impl BuiltinHookHandler for FailingWorktreeHook {
    fn id(&self) -> &str {
        "broken-worktree-hook"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new().into(),
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
        on_failure: None,
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
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: tool_name.into(),
                input: llm_runtime::services::sdk::protocol::JsonPayload::from_generated_value(
                    &input,
                )
                .expect("generated tool input fixture"),
            }],
            Some("tool_use"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        ),
    ]))
}

#[tokio::test]
async fn successful_enter_worktree_does_not_synthesize_after_creation_hook() {
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

    assert!(
        log.lock().unwrap().is_empty(),
        "the manager, not tool dispatch, owns WorktreeCreate"
    );
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
async fn tool_name_cannot_trigger_an_unrelated_worktree_create_hook() {
    // This fake tool does not create through WorktreeManager. Its name cannot
    // trigger a lifecycle hook, including one which would fail real creation.
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
