//! from the turn loop's tool-dispatch chokepoint (`turn_loop.rs`).
//!
//! Byte-faithful to claude-code: after a tool runs, a SUCCESSFUL result fires
//! `PostToolUse` (`executePostToolUseHooks`) and a FAILED result fires
//! `PostToolUseFailure` (`executePostToolUseFailureHooks`, `utils/hooks.ts:3492`)
//! — never both. Both arms are best-effort: a hook that itself fails must NOT
//! break the turn.
//!
//! Scenarios:
//! 1. A tool that ERRORS fires `PostToolUseFailure` (and NOT `PostToolUse`),
//!    carrying the tool name / id + the raw error string.
//! 2. A tool that SUCCEEDS fires `PostToolUse` (and NOT `PostToolUseFailure`).
//! 3. A `PostToolUseFailure` hook that itself returns an error outcome does NOT
//!    break the turn (the loop still reaches `end_turn`).
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{ConversationMessage, HookId, HttpRequest, HttpResponse, ToolUseId};
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

// ---- Tools: one that always succeeds, one that always errors ----

/// A no-op tool that ALWAYS succeeds with a fixed payload.
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

/// A tool that ALWAYS errors with a fixed message.
struct AlwaysFailTool;
#[async_trait]
impl Tool for AlwaysFailTool {
    fn name(&self) -> &str {
        "AlwaysFail"
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
        "fail".into()
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
        Err(ToolError::Internal("disk on fire".into()))
    }
}

// ---- Recording hook: captures which event variant fired ----

/// What the recording handler observed for a post-dispatch hook event.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Post {
        tool_name: String,
        tool_use_id: String,
    },
    PostFailure {
        tool_name: String,
        tool_input: serde_json::Value,
        error: String,
        tool_use_id: String,
    },
}

/// Records every `PostToolUse` / `PostToolUseFailure` event it sees into a
/// shared log so the test can assert exactly which one fired. Returns no
/// decision/response (a pass-through observer).
struct RecordingHandler {
    log: Arc<Mutex<Vec<Seen>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-post"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        match event {
            HookEvent::PostToolUse {
                tool_name,
                tool_use_id,
                ..
            } => self.log.lock().unwrap().push(Seen::Post {
                tool_name: tool_name.clone(),
                tool_use_id: tool_use_id.to_string(),
            }),
            HookEvent::PostToolUseFailure {
                tool_name,
                tool_input,
                error,
                tool_use_id,
                ..
            } => self.log.lock().unwrap().push(Seen::PostFailure {
                tool_name: tool_name.clone(),
                tool_input: tool_input.clone(),
                error: error.clone(),
                tool_use_id: tool_use_id.to_string(),
            }),
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

/// A `PostToolUseFailure` hook that itself FAILS (non-success outcome). Proves
/// the failure-hook arm is best-effort — a broken hook must not break the turn.
struct FailingFailureHook;
#[async_trait]
impl BuiltinHookHandler for FailingFailureHook {
    fn id(&self) -> &str {
        "broken-failure-hook"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the failure hook itself blew up".into(),
            exit_code: Some(1),
            response: None,
        }
    }
}

struct FailureContextHook;

#[async_trait]
impl BuiltinHookHandler for FailureContextHook {
    fn id(&self) -> &str {
        "failure-context-hook"
    }

    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response =
            matches!(event, HookEvent::PostToolUseFailure { .. }).then(|| HookResponse {
                reason: Some("STOP-FAILURE".into()),
                additional_context: Some("failure context".into()),
                prevent_continuation: true,
                ..HookResponse::default()
            });
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response,
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

/// Build an executor with a single handler registered for BOTH `PostToolUse`
/// and `PostToolUseFailure`, so the test observes whichever one the turn loop
/// fires.
async fn exec_recording(log: Arc<Mutex<Vec<Seen>>>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        r.register(builtin_hook("record-post", HookEventType::PostToolUse));
        r.register(builtin_hook(
            "record-post",
            HookEventType::PostToolUseFailure,
        ));
    }
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

/// Two API turns: turn 1 emits a `tool_use` for `tool_name`, turn 2 ends.
fn two_turn_api(tool_use_id: ToolUseId, tool_name: &str) -> Arc<MockApiClient> {
    Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall { input_projection: None,
                id: tool_use_id.to_string(),
                name: tool_name.into(),
                input: json!({}),
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
async fn errored_tool_fires_post_tool_use_failure_not_post_tool_use() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysFail");
    let log = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysFailTool));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("break it").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "exactly one post-dispatch hook event must fire: {seen:?}"
    );
    match &seen[0] {
        Seen::PostFailure {
            tool_name,
            tool_input,
            error,
            tool_use_id: id,
        } => {
            assert_eq!(tool_name, "AlwaysFail");
            assert_eq!(id, &tool_use_id.to_string());
            // The dispatched tool input is now carried on the failure payload
            // (the same `effective_input` the success arm threads). The API
            // turn emitted `input: json!({})`, so the dispatched input is `{}`.
            assert_eq!(tool_input, &json!({}));
            // The raw tool error string is carried verbatim (NOT the
            // "Error: "-prefixed model-facing content).
            assert!(
                error.contains("disk on fire"),
                "failure payload carries the raw error: {error}"
            );
            assert!(
                !error.starts_with("Error: "),
                "must be the raw error, not the model-facing prefixed content: {error}"
            );
        }
        other @ Seen::Post { .. } => {
            panic!("errored tool must fire PostToolUseFailure, got: {other:?}")
        }
    }
}

#[tokio::test]
async fn succeeded_tool_fires_post_tool_use_not_failure() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysOk");
    let log = Arc::new(Mutex::new(Vec::<Seen>::new()));
    let hooks = exec_recording(log.clone()).await;
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(AlwaysOkTool));
    let orch = orch_with(api, hooks, registry);

    let outcome = orch.run_turn("do it").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        1,
        "exactly one post-dispatch hook event must fire: {seen:?}"
    );
    match &seen[0] {
        Seen::Post {
            tool_name,
            tool_use_id: id,
        } => {
            assert_eq!(tool_name, "AlwaysOk");
            assert_eq!(id, &tool_use_id.to_string());
        }
        other @ Seen::PostFailure { .. } => {
            panic!("successful tool must fire PostToolUse, got: {other:?}")
        }
    }
}

#[tokio::test]
async fn failing_post_tool_use_failure_hook_does_not_break_turn() {
    // The errored tool fires PostToolUseFailure; the registered failure hook
    // itself returns a non-success outcome. The turn must STILL complete
    // (best-effort, identical to the PostToolUse arm).
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "AlwaysFail");

    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-failure-hook",
        HookEventType::PostToolUseFailure,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingFailureHook));
    let hooks = Arc::new(exec);

    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(AlwaysFailTool));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch
        .run_turn("break it")
        .await
        .expect("turn must succeed despite the failure hook itself failing");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "a failing PostToolUseFailure hook must not break the turn: {outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        2,
        "the loop still reaches the terminating turn (2 API calls)"
    );
}

#[tokio::test]
async fn failure_hook_feedback_uses_the_failure_event_name_everywhere() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id, "AlwaysFail");
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "failure-context-hook",
        HookEventType::PostToolUseFailure,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailureContextHook));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(AlwaysFailTool));
    let orch = orch_with(api, Arc::new(exec), tools);

    orch.run_turn("break it").await.expect("turn completes");
    let history = orch.session().lock().await.history.clone();
    let rendered = history
        .iter()
        .map(ConversationMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("PostToolUseFailure:AlwaysFail hook stopped continuation: STOP-FAILURE")
    );
    assert!(
        rendered.contains("PostToolUseFailure:AlwaysFail hook additional context: failure context")
    );
    assert!(!rendered.contains("PostToolUse:AlwaysFail hook"));
}
