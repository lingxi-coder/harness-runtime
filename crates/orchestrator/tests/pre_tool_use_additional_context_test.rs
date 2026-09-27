//! P1-c parity: a PreToolUse hook's `hookSpecificOutput.additionalContext` is
//! injected as its OWN `<system-reminder>` user message — NOT folded into the
//! tool_result content. Mirrors claude-code (`toolExecution.ts:845-846` +
//! `messages.ts:4117` `hook_additional_context` attachment, wrapped via
//! `wrapInSystemReminder`, `messages.ts:3097`). The sibling `systemMessage` (the
//! UI-notice field) is kept DISTINCT: claude-code routes it to a
//! `hook_system_message` attachment whose `normalizeAttachmentForAPI` returns
//! `[]` (`messages.ts:4258`), so it is transcript/user-facing only and NEVER
//! reaches the model — neither folded into the tool_result nor surfaced as a
//! model-facing message.

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use protocol::{ContentBlock, ConversationMessage, HookId, HttpRequest, HttpResponse, ToolUseId};
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

struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _req: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
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
    ) -> Result<platform_api::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(&self, _h: &platform_api::BackgroundTaskHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

struct EchoTool;
#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
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
        "echo".into()
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
            data: json!({ "content": "ORIGINAL_OUTPUT" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A PreToolUse hook that returns BOTH a systemMessage (UI notice) and an
/// additionalContext (model-facing). Only the latter should become a separate
/// `<system-reminder>` message; the former stays folded into the tool_result.
struct AddContextHook;
#[async_trait]
impl BuiltinHookHandler for AddContextHook {
    fn id(&self) -> &str {
        "add-context"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: Some(HookResponse {
                system_message: Some("a UI notice".into()),
                additional_context: Some("USE THE NEW API".into()),
                ..Default::default()
            }),
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
        source: HookSource::Settings(protocol::SettingsScope::User),
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
        .register(builtin_hook(handler_id, HookEventType::PreToolUse));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(handler);
    Arc::new(exec)
}

#[tokio::test]
async fn pre_tool_use_additional_context_is_a_separate_system_reminder_message() {
    let tool_use_id = ToolUseId::new();
    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: tool_use_id.to_string(),
                name: "Echo".into(),
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
    ]));
    let hooks = exec_with(Arc::new(AddContextHook), "add-context").await;
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tools),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("use echo").await.expect("turn ok");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let s = session.lock().await;

    // The tool_result user message: its ToolResult content must NOT contain the
    // additionalContext (it is surfaced as a separate message, not folded), and
    // it must NOT contain the `systemMessage` either. claude-code routes a
    // PreToolUse `systemMessage` to a `hook_system_message` attachment whose
    // `normalizeAttachmentForAPI` returns `[]` (`messages.ts:4258`): it is
    // transcript/user-facing only and NEVER reaches the model. So neither the
    // additionalContext NOR the systemMessage may leak into the tool_result.
    let tool_result_content = s
        .history
        .iter()
        .find_map(|m| match m {
            ConversationMessage::User { content, .. } => content.iter().find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("a tool_result user message exists");
    assert!(
        !tool_result_content.contains("hook additional context"),
        "additionalContext must NOT be folded into the tool_result content: {tool_result_content}"
    );
    assert!(
        !tool_result_content.contains("a UI notice"),
        "systemMessage must NOT reach the model — it is transcript-only (messages.ts:4258): {tool_result_content}"
    );

    // A SEPARATE user message must carry the additionalContext wrapped exactly
    // as claude-code's `hook_additional_context` attachment renders it.
    let expected = "<system-reminder>\nPreToolUse:Echo hook additional context: USE THE NEW API\n</system-reminder>";
    let found_separate = s.history.iter().any(|m| match m {
        ConversationMessage::User { content, .. } => content.iter().any(|b| {
            matches!(
            b, ContentBlock::Text { text } if text == expected)
        }),
        _ => false,
    });
    assert!(
        found_separate,
        "additionalContext must be a SEPARATE <system-reminder> user message; history: {:?}",
        s.history
    );
}
