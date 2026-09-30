//! tool-dispatch chokepoint (`turn_loop.rs`) immediately BEFORE the
//! subagent-spawning `Agent` tool (legacy alias `Task`) begins.
//!
//! Parity with claude-code: `executeSubagentStartHooks(agentId, agentType, …)`
//! (`utils/hooks.ts:3932-3952`) builds `hook_event_name: 'SubagentStart'` at the
//! START of a subagent's run (`runAgent.ts:532`), the counterpart to the
//! `SubagentStop` fired when it ends. The `LingXi` port spawns subagents only
//! through the registered, turn_loop-dispatched `Agent` tool, so the start of
//! that tool's dispatch IS the subagent spawn — the fire lives in
//! `dispatch_tool_uses` immediately before `tool_handle.call()`, after the
//! pre-hook + permission gate have cleared.
//!
//! Scenarios:
//! 1. An `Agent` dispatch fires `SubagentStart` with the dispatched
//!    `subagent_type` carried on the hook context's `agent_type` and a real
//!    `agent:UUID` id on the wire payload's required field.
//! 2. The legacy `Task` alias fires `SubagentStart` identically.
//! 3. A non-Agent tool never fires `SubagentStart`.
//! 4. No `SubagentStart` hook registered → the spawn is a strict no-op
//!    (byte-identical: the turn still reaches `end_turn`).
//! 5. A `SubagentStart` hook that itself fails does NOT break the turn.
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

/// Stand-in for the real `Agent` tool, registered under a configurable name
/// (`"Agent"` or the legacy `"Task"` alias) so the dispatch chokepoint keys on
/// it. The `SubagentStart` + `SubagentStop` fires happen AFTER `call()` and read
/// the child's REAL pool id off the result `data.agentId` (C1 seam), so this
/// fake surfaces a fixed `agentId` to prove both events fire with the SAME
/// canonical id (#8).
struct FakeAgentTool {
    name: &'static str,
}
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
        let subagent_type = input
            .get("subagent_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        // Surface a fixed REAL child id (the C1 seam: the real Agent tool emits
        // `agentId = agent_id.to_string()`) so the chokepoint recovers it for
        // BOTH the SubagentStart and SubagentStop fires (#8).
        Ok(ToolCallResult {
            data: json!({
                "subagent_type": subagent_type,
                "result": "done",
                "agentId": FAKE_AGENT_CHILD_ID.to_string(),
            }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Fixed child `AgentId` the [`FakeAgentTool`] surfaces on its result so tests
/// can assert the chokepoint threads the REAL id (not a fresh mint) into both
/// the SubagentStart and SubagentStop fires (#8).
static FAKE_AGENT_CHILD_ID: once_cell::sync::Lazy<lingxi_core::types::AgentId> =
    once_cell::sync::Lazy::new(lingxi_core::types::AgentId::new);

/// A non-Agent tool that always succeeds.
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

// ---- Recording hook: captures every SubagentStart event it sees ----

#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenStart {
    /// `agent_id` stringified (the wire `agent:UUID` shape).
    agent_id: String,
    agent_type: String,
    /// The hook context's `agent_type` — the dispatched `subagent_type`.
    ctx_agent_type: Option<String>,
}

struct RecordingHandler {
    log: Arc<Mutex<Vec<SeenStart>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-subagent-start"
    }
    async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> HookResult {
        if let HookEvent::SubagentStart {
            agent_id,
            agent_type,
            ..
        } = event
        {
            self.log.lock().unwrap().push(SeenStart {
                agent_id: agent_id.to_string(),
                agent_type: agent_type.clone(),
                ctx_agent_type: ctx.agent_type.clone(),
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

/// A `SubagentStart` hook that itself FAILS — proves the fire is best-effort.
struct FailingStartHook;
#[async_trait]
impl BuiltinHookHandler for FailingStartHook {
    fn id(&self) -> &str {
        "broken-subagent-start-hook"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the subagent-start hook itself blew up".into(),
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

async fn exec_recording(log: Arc<Mutex<Vec<SeenStart>>>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-subagent-start",
        HookEventType::SubagentStart,
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
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]))
}

#[tokio::test]
async fn agent_dispatch_fires_subagent_start() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenStart>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeAgentTool { name: "Agent" }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("spawn an agent").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "exactly one SubagentStart event must fire: {seen:?}"
    );
    assert_eq!(seen[0].agent_type, "general-purpose");
    // The dispatched `subagent_type` also rides on the hook context's `agent_type`.
    assert_eq!(seen[0].ctx_agent_type.as_deref(), Some("general-purpose"));
    // #8: the REAL child id (recovered from the result `data.agentId`) rode on
    // the event — NOT a fresh mint.
    assert_eq!(
        seen[0].agent_id,
        FAKE_AGENT_CHILD_ID.to_string(),
        "SubagentStart must carry the child's REAL pool id from data.agentId"
    );
}

#[tokio::test]
async fn legacy_task_alias_fires_subagent_start() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Task",
        json!({ "subagent_type": "code-reviewer", "prompt": "go" }),
    );
    let log = Arc::new(Mutex::new(Vec::<SeenStart>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(FakeAgentTool { name: "Task" }));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("spawn a task").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "the legacy `Task` alias must fire SubagentStart: {seen:?}"
    );
    assert_eq!(seen[0].agent_type, "code-reviewer");
}

#[tokio::test]
async fn non_agent_tool_does_not_fire_subagent_start() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysOk", json!({}));
    let log = Arc::new(Mutex::new(Vec::<SeenStart>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysOkTool));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("do it").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    assert!(
        log.lock().unwrap().is_empty(),
        "a non-Agent tool must not fire SubagentStart"
    );
}

#[tokio::test]
async fn no_subagent_start_hook_registered_is_noop() {
    // No SubagentStart hook registered: the Agent dispatch must still complete
    // the turn (the fire is a strict no-op).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );
    // An empty registry — no hooks of any kind.
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let hooks = Arc::new(HookExecutorImpl::new(
        registry,
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeAgentTool { name: "Agent" }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch.run_turn("spawn an agent").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "no-hook case must be a strict no-op: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}

#[tokio::test]
async fn failing_subagent_start_hook_does_not_break_turn() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-subagent-start-hook",
        HookEventType::SubagentStart,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingStartHook));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeAgentTool { name: "Agent" }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch
        .run_turn("spawn an agent")
        .await
        .expect("turn must succeed despite the subagent-start hook itself failing");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "a failing SubagentStart hook must not break the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}

/// Records the `agent_id` carried by BOTH SubagentStart and SubagentStop events
/// so a test can assert they share ONE canonical id (#8).
struct StartStopIdRecorder {
    start_id: Arc<Mutex<Option<String>>>,
    stop_id: Arc<Mutex<Option<String>>>,
}
#[async_trait]
impl BuiltinHookHandler for StartStopIdRecorder {
    fn id(&self) -> &str {
        "record-start-stop-id"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        match event {
            HookEvent::SubagentStart { agent_id, .. } => {
                *self.start_id.lock().unwrap() = Some(agent_id.to_string());
            }
            HookEvent::SubagentStop { agent_id, .. } => {
                *self.stop_id.lock().unwrap() = Some(agent_id.to_string());
            }
            _ => {}
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

#[tokio::test]
async fn subagent_start_and_stop_share_the_same_real_child_id() {
    // #8: BOTH SubagentStart and SubagentStop must fire with the SAME canonical
    // child id (claude runAgent.ts:347), recovered from the result `data.agentId`
    // — not two fresh divergent mints.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(
        tool_use_id,
        "Agent",
        json!({ "subagent_type": "general-purpose", "prompt": "go" }),
    );
    let start_id = Arc::new(Mutex::new(None));
    let stop_id = Arc::new(Mutex::new(None));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut w = registry.write().await;
        w.register(builtin_hook(
            "record-start-stop-id",
            HookEventType::SubagentStart,
        ));
        w.register(builtin_hook(
            "record-start-stop-id",
            HookEventType::SubagentStop,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(StartStopIdRecorder {
        start_id: start_id.clone(),
        stop_id: stop_id.clone(),
    }));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeAgentTool { name: "Agent" }));
    let orch = orch_with(api, hooks, tools);

    let outcome = orch.run_turn("spawn an agent").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let start = start_id
        .lock()
        .unwrap()
        .clone()
        .expect("SubagentStart fired");
    let stop = stop_id.lock().unwrap().clone().expect("SubagentStop fired");
    let expected = FAKE_AGENT_CHILD_ID.to_string();
    assert_eq!(start, expected, "SubagentStart carries the real child id");
    assert_eq!(stop, expected, "SubagentStop carries the real child id");
    assert_eq!(start, stop, "both events share ONE canonical id");
}

// ---- G008 (WP2a item 3): a Fusion `Agent`-tool result fires NO chokepoint
// SubagentStart/SubagentStop, phantom or otherwise ------------------------

/// Stand-in for the `Agent` tool's Fusion branch (`call_fusion` ->
/// `fusion_tool_result`, `tools/agent/src/agent.rs`): registered under the
/// SAME dispatched name (`"Agent"`) the chokepoint keys on, but its result
/// carries `subagentHooksFired: true` and — unlike [`FakeAgentTool`] — NO
/// `agentId` at all, matching the real `fusion_tool_result`'s `data` shape (a
/// Fusion run is N panels, not one child with a canonical id).
struct FakeFusionTool;
#[async_trait]
impl Tool for FakeFusionTool {
    fn name(&self) -> &str {
        "Agent"
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
        "run fusion".into()
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
            data: json!({
                "runId": "fusion-run-1",
                "status": "completed",
                "decision": null,
                "panels": [],
                "subagentHooksFired": true,
            }),
            model_content: Some("fusion final text".into()),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// Counts every SubagentStart / SubagentStop the hook system fires, of
/// either kind, without caring about the carried id — the fusion assertion
/// is that the COUNT is zero, not what a particular fire looked like.
struct StartStopCounter {
    starts: Arc<Mutex<u32>>,
    stops: Arc<Mutex<u32>>,
}
#[async_trait]
impl BuiltinHookHandler for StartStopCounter {
    fn id(&self) -> &str {
        "count-start-stop"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        match event {
            HookEvent::SubagentStart { .. } => {
                *self.starts.lock().unwrap() += 1;
            }
            HookEvent::SubagentStop { .. } => {
                *self.stops.lock().unwrap() += 1;
            }
            _ => {}
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

#[tokio::test]
async fn fusion_tool_result_fires_no_subagent_hooks() {
    // G008: before WP2a, this combination (`subagentHooksFired: true`, no
    // `agentId`) fell through to the ordinary Agent-tool branch: SubagentStart
    // skipped (runner_fired_start), but SubagentStop fired UNCONDITIONALLY on
    // a freshly-minted id with no transcript — a phantom "fusion" stop paired
    // with nothing. Both must now be zero.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id, "Agent", json!({}));
    let starts = Arc::new(Mutex::new(0_u32));
    let stops = Arc::new(Mutex::new(0_u32));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut w = registry.write().await;
        w.register(builtin_hook(
            "count-start-stop",
            HookEventType::SubagentStart,
        ));
        w.register(builtin_hook(
            "count-start-stop",
            HookEventType::SubagentStop,
        ));
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(StartStopCounter {
        starts: starts.clone(),
        stops: stops.clone(),
    }));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(FakeFusionTool));
    let orch = orch_with(api, hooks, tools);

    let outcome = orch.run_turn("run fusion").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    assert_eq!(
        *starts.lock().unwrap(),
        0,
        "a Fusion tool result must fire ZERO SubagentStart events"
    );
    assert_eq!(
        *stops.lock().unwrap(),
        0,
        "a Fusion tool result must fire ZERO SubagentStop events (no phantom pair)"
    );
}
