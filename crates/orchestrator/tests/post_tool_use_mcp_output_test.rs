//! MCP tool's output via `hookSpecificOutput.updatedMCPToolOutput`, threaded
//! back into the model-facing tool result.
//!
//! Parity with claude-code: `parseHookJSONOutput` extracts
//! `updatedMCPToolOutput` for the `PostToolUse` case (`utils/hooks.ts:646-649`);
//! the tool-hook runner substitutes it for the tool output ONLY when the tool is
//! an MCP tool (`isMcpTool(tool)`, `toolHooks.ts:146` / `toolExecution.ts:1494`).
//!
//! Scenarios:
//! 1. An MCP tool + a `PostToolUse` hook returning `updatedMCPToolOutput` →
//!    the model-facing tool result is the REWRITTEN output.
//! 2. A non-MCP tool + the same hook → the result is UNCHANGED (the
//!    `isMcpTool` gate suppresses the mutation).
//! 3. An MCP tool + NO hook (or a hook that doesn't set the field) → the result
//!    is the tool's own output, byte-identical (strict no-op).
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

// ---- unused HTTP / Runtime stubs ----
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

// ---- A configurable tool: `is_mcp` is a flag so one impl covers both cases ----
struct ConfigurableTool {
    name: &'static str,
    is_mcp: bool,
}
#[async_trait]
impl Tool for ConfigurableTool {
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
    fn is_mcp(&self) -> bool {
        self.is_mcp
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
        "configurable".into()
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
            // The tool's own output the model would see absent any mutation.
            data: json!({ "content": "ORIGINAL_OUTPUT" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

// ---- A PostToolUse hook that REPLACES the output via updatedMCPToolOutput ----
struct RewriteOutputHook;
#[async_trait]
impl BuiltinHookHandler for RewriteOutputHook {
    fn id(&self) -> &str {
        "rewrite-mcp-output"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(HookResponse {
                updated_mcp_tool_output: Some(json!({ "content": "REWRITTEN_BY_HOOK" })),
                ..Default::default()
            }),
        }
    }
}

// ---- A PostToolUse hook that does NOT set updatedMCPToolOutput (pure observer) ----
struct ObserverHook;
#[async_trait]
impl BuiltinHookHandler for ObserverHook {
    fn id(&self) -> &str {
        "observe-only"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
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

async fn exec_with(
    handler: Arc<dyn BuiltinHookHandler>,
    handler_id: &str,
) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook(handler_id, HookEventType::PostToolUse));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(handler);
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

fn two_turn_api(tool_use_id: ToolUseId, tool_name: &str) -> Arc<MockApiClient> {
    Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: tool_name.into(),
                input: json!({}),
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

/// The model-facing tool-result content lands in the turn-2 history (the
/// `ToolResult` block sent back to the model). Find that message rather than
/// assuming it is last because transient reminders may trail it.
async fn turn2_result_payload(api: &MockApiClient) -> String {
    let captured = api.captured_msgs().await;
    assert_eq!(captured.len(), 2, "expected 2 API turns");
    let turn2 = &captured[1];
    turn2
        .iter()
        .map(|message| serde_json::to_string(message).unwrap())
        .find(|payload| payload.contains("tool_result"))
        .expect("turn-2 history has a tool-result message")
}

#[tokio::test]
async fn mcp_tool_output_is_rewritten_by_hook() {
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "McpTool");
    let hooks = exec_with(Arc::new(RewriteOutputHook), "rewrite-mcp-output").await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ConfigurableTool {
        name: "McpTool",
        is_mcp: true,
    }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch.run_turn("call mcp tool").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let payload = turn2_result_payload(&api).await;
    assert!(
        payload.contains("REWRITTEN_BY_HOOK"),
        "an MCP tool's output must be REPLACED by the hook: {payload}"
    );
    assert!(
        !payload.contains("ORIGINAL_OUTPUT"),
        "the original output must be gone after the rewrite: {payload}"
    );
}

#[tokio::test]
async fn non_mcp_tool_output_is_not_mutated() {
    // The SAME rewriting hook, but the tool is NOT an MCP tool → the
    // `isMcpTool` gate suppresses the mutation; the result is unchanged.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "PlainTool");
    let hooks = exec_with(Arc::new(RewriteOutputHook), "rewrite-mcp-output").await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ConfigurableTool {
        name: "PlainTool",
        is_mcp: false,
    }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch.run_turn("call plain tool").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let payload = turn2_result_payload(&api).await;
    assert!(
        payload.contains("ORIGINAL_OUTPUT"),
        "a non-MCP tool's output must be UNCHANGED: {payload}"
    );
    assert!(
        !payload.contains("REWRITTEN_BY_HOOK"),
        "the hook's replacement must NOT apply to a non-MCP tool: {payload}"
    );
}

#[tokio::test]
async fn mcp_tool_with_no_mutating_hook_is_unchanged() {
    // An MCP tool + a PostToolUse hook that does NOT set updatedMCPToolOutput →
    // strict no-op: the model sees the tool's own output.
    let tool_use_id = ToolUseId::new();
    let api = two_turn_api(tool_use_id.clone(), "McpTool");
    let hooks = exec_with(Arc::new(ObserverHook), "observe-only").await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ConfigurableTool {
        name: "McpTool",
        is_mcp: true,
    }));
    let orch = orch_with(api.clone(), hooks, tools);

    let outcome = orch.run_turn("call mcp tool").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let payload = turn2_result_payload(&api).await;
    assert!(
        payload.contains("ORIGINAL_OUTPUT"),
        "no mutating hook → the output must be byte-identical: {payload}"
    );
    assert!(
        !payload.contains("REWRITTEN_BY_HOOK"),
        "no replacement was returned, so none must be applied: {payload}"
    );
}
