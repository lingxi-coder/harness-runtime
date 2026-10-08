//! tool-dispatch chokepoint (`turn_loop.rs`) after the subagent-spawning `Agent`
//! tool completes.
//!
//! Parity with claude-code: `executeStopHooks(…, subagentId, …)`
//! (`utils/hooks.ts:3653-3678`) builds `hook_event_name: 'SubagentStop'` when a
//! subagent's query loop stops, keyed on `toolUseContext.agentId` being set. The
//! unified stop chokepoint (`runStopHooks`/`stopHooks.ts`) runs at the natural
//! end of completed and failed child loops; cancellation does not fire it.
//! The `LingXi` port spawns subagents only
//! through the registered, turn_loop-dispatched `Agent` tool, so a COMPLETED
//! dispatch of that tool means the subagent's loop has stopped — the fire lives
//! in `dispatch_tool_uses` immediately after the tool returns (alongside
//! `PostToolUse`/`WorktreeCreate`). Same TIMING (subagent stopped); the
//! child runner owns its frontmatter hooks, while this chokepoint owns the
//! session and plugin hooks.
//!
//! Scenarios:
//! 1. A successful `Agent` dispatch fires `SubagentStop` with `status:"completed"`
//!    and the dispatched `subagent_type` carried on the hook context's
//!    `agent_type` (so the wire payload's `agent_type` is faithful).
//! 2. A FAILED allocated child fires `SubagentStop` with `status:"failed"` and
//!    its real identity. Admission failures have no child lifecycle.
//! 3. The removed `Task` name does not fire `SubagentStop`.
//! 4. A non-Agent tool never fires `SubagentStop`.
//! 5. A `SubagentStop` hook that itself fails does NOT break the turn
//!    (best-effort, like the `PostToolUse`/`WorktreeCreate` arms).
use llm_runtime::ContentBlock as LlmContentBlock;

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse, ToolUseId};
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
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

/// A stand-in for the real `Agent` tool: registered under a configurable name
/// so the dispatch chokepoint keys only on the current Agent name for
/// it. Skips the real spawner wiring — the `SubagentStop` fire is downstream of
/// the dispatch outcome, so a fake tool that echoes/fails is sufficient to
/// exercise it. The real tool's success result carries `subagent_type`, but the
/// fire sources `subagent_type` from the tool INPUT, so we keep the input shape.
struct FakeAgentTool {
    name: &'static str,
    fail: bool,
}

static FAKE_AGENT_CHILD_ID: once_cell::sync::Lazy<lingxi_core::types::AgentId> =
    once_cell::sync::Lazy::new(lingxi_core::types::AgentId::new);
#[async_trait]
impl Tool for FakeAgentTool {
    fn name(&self) -> &str {
        self.name
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
        "run a subagent".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if self.fail {
            return Err(ToolError::SubagentFailed {
                agent_id: *FAKE_AGENT_CHILD_ID,
                reason: "Agent: subagent failed".into(),
                terminal_hooks_owned: false,
            });
        }
        let subagent_type = input
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        Ok(ToolCallResult {
            data: json!({
                "subagent_type": subagent_type,
                "result": "done",
                "agentId": FAKE_AGENT_CHILD_ID.as_uuid().to_string(),
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A non-Agent tool (different name) that always succeeds.
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
        Ok(ToolCallResult {
            data: json!({ "content": "all good" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ---- Recording hook: captures every SubagentStop event it sees ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenStop {
    /// `agent_id` stringified (the wire `agent:UUID` shape) — proves a real id
    /// rode on the event.
    agent_id: String,
    status: String,
    /// The hook context's `agent_type` — the dispatched `subagent_type`
    /// (claude-code's `agentType`).
    agent_type: Option<String>,
}

struct RecordingHandler {
    log: Arc<Mutex<Vec<SeenStop>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-subagent-stop"
    }
    async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> HookResult {
        if let HookEvent::SubagentStop {
            agent_id, status, ..
        } = event
        {
            self.log.lock().unwrap().push(SeenStop {
                agent_id: agent_id.to_string(),
                status: status.clone(),
                agent_type: ctx.agent_type.clone(),
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

/// A `SubagentStop` hook that itself FAILS — proves the fire is best-effort.
struct FailingStopHook;
#[async_trait]
impl BuiltinHookHandler for FailingStopHook {
    fn id(&self) -> &str {
        "broken-subagent-stop-hook"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the subagent-stop hook itself blew up".into(),
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

async fn exec_recording(log: Arc<Mutex<Vec<SeenStop>>>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-subagent-stop",
        HookEventType::SubagentStop,
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
async fn successful_agent_fires_subagent_stop_completed() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenStop>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeAgentTool {
        name: "Agent",
        fail: false,
    }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("spawn an agent").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "exactly one SubagentStop event must fire: {seen:?}"
    );
    assert_eq!(seen[0].status, "completed");
    // The dispatched `subagent_type` rides on the hook context's `agent_type`.
    assert_eq!(seen[0].agent_type.as_deref(), Some("general-purpose"));
    assert_eq!(seen[0].agent_id, FAKE_AGENT_CHILD_ID.to_string());
}

#[tokio::test]
async fn failed_agent_still_fires_subagent_stop_failed() {
    // The subagent STOPPED even though the dispatch errored — claude-code's stop
    // chokepoint runs at the loop's natural end regardless of outcome.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "code-reviewer", "prompt": "go" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenStop>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeAgentTool {
        name: "Agent",
        fail: true,
    }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("spawn an agent").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "a failed Agent dispatch must STILL fire SubagentStop: {seen:?}"
    );
    assert_eq!(seen[0].status, "failed");
    assert_eq!(seen[0].agent_id, FAKE_AGENT_CHILD_ID.to_string());
    assert_eq!(seen[0].agent_type.as_deref(), Some("code-reviewer"));
}

#[tokio::test]
async fn removed_task_alias_does_not_fire_subagent_stop() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Task",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenStop>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeAgentTool {
        name: "Task",
        fail: false,
    }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("spawn a task").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert!(
        seen.is_empty(),
        "removed Task name must not fire SubagentStop: {seen:?}"
    );
}

#[tokio::test]
async fn non_agent_tool_does_not_fire_subagent_stop() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysOk", json!({}));
    let log = Arc::new(Mutex::new(Vec::<SeenStop>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysOkTool));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("do it").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    assert!(
        log.lock().unwrap().is_empty(),
        "a non-Agent tool must not fire SubagentStop"
    );
}

#[tokio::test]
async fn failing_subagent_stop_hook_does_not_break_turn() {
    // The successful Agent dispatch fires SubagentStop; the registered hook
    // itself returns a non-success outcome. The turn must STILL complete
    // (best-effort, identical to the PostToolUse / WorktreeCreate arms).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-subagent-stop-hook",
        HookEventType::SubagentStop,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingStopHook));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeAgentTool {
        name: "Agent",
        fail: false,
    }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch
        .run_turn("spawn an agent")
        .await
        .expect("turn must succeed despite the subagent-stop hook itself failing");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "a failing SubagentStop hook must not break the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}
