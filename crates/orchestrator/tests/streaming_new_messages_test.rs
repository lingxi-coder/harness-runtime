//! non-empty (the Skill-tool shape — the expanded skill prompt) must have those
//! injected messages replayed into session history right after the turn's
//! `tool_result` user message, in tool-dispatch order, with each injected
//! message's id recorded in `SessionState::injected_message_sources` under the
//! injecting tool's `tool_use_id` (faithful port of TS `tagMessagesWithToolUseID`
//! stamping `sourceToolUseID`). This is the streaming counterpart of the batched
//! `turn_loop::tests::injected_message_sources_records_tool_use_id` /
//! `normal_tool_records_no_source_and_serializes_no_field` tests.
//!
//! TS evidence: `services/tools/toolExecution.ts:1565-1570` pushes
//! `result.newMessages` into `resultingMessages` inside the single shared
//! tool-execution generator that BOTH the streaming and non-streaming agent
//! loops drain — there is no separate streaming consumption path in TS, so the
//! Rust streaming driver must append identically to the batched one.

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{
    ContentBlock, ConversationMessage, HookId, HttpRequest, HttpResponse, MessageId, ToolUseId,
};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, noop_hook_executor, text_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::json;
use std::path::PathBuf;
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

/// A `PreToolUse` builtin hook that returns `additionalContext` (the model-facing
/// channel), which the turn loop surfaces as a standalone meta user message
/// (HOOK.1 / claude-code `toolExecution.ts:845`).
struct ContextHook;
#[async_trait]
impl BuiltinHookHandler for ContextHook {
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(HookResponse {
                additional_context: Some("STREAM-CTX".into()),
                ..HookResponse::default()
            }),
        }
    }
    fn id(&self) -> &str {
        "context-pre"
    }
}

/// Build a `HookExecutorImpl` with a single unconditional `PreToolUse` hook that
/// emits the `STREAM-CTX` context.
fn context_pre_hook_executor() -> Arc<HookExecutorImpl> {
    let hook = HookDefinition {
        id: HookId::new(),
        name: "context-pre".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: "context-pre".into(),
        },
        source: HookSource::Session,
        blocking: true,
        timeout: None,
        priority: 0,
        once: false,
        status_message: None,
        async_rewake: false,
        async_timeout: None,
        rewake_message: None,
    };
    let mut registry = HookRegistry::new();
    registry.register(hook);
    let reg = Arc::new(RwLock::new(registry));
    let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(ContextHook));
    Arc::new(exec)
}

/// A tool whose success result carries an extra conversation message in
/// `new_messages` (the Skill-tool shape). The text `"EXPANDED-SKILL-PROMPT"`
/// matches the batched-path test fixture so the two stay in lock-step.
struct InjectingTool;

#[async_trait]
impl Tool for InjectingTool {
    fn name(&self) -> &str {
        "Inject"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
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
        "inject".into()
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
            data: json!({
                "content": "TOOL-RESULT",
                "model_content": "Launching skill: demo",
            }),
            model_content: None,
            new_messages: vec![ConversationMessage::user(
                MessageId::new(),
                "EXPANDED-SKILL-PROMPT".into(),
            )],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

/// A tool whose success result carries NO `new_messages` — the common case for
/// every non-skill tool. The streaming driver must leave history untouched
/// beyond the assistant + `tool_result` messages, byte-identical to before.
struct PlainTool;

#[async_trait]
impl Tool for PlainTool {
    fn name(&self) -> &str {
        "Plain"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
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
        "plain".into()
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
            data: json!({ "content": "TOOL-RESULT", "model_content": "ok" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn registry_with(tool: Arc<dyn Tool>) -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(tool);
    Arc::new(r)
}

fn streaming_orch(
    registry: Arc<ToolRegistry>,
    api: Arc<MockStreamingApiClient>,
) -> ConversationOrchestrator {
    streaming_orch_with_hooks(registry, api, noop_hook_executor())
}

fn streaming_orch_with_hooks(
    registry: Arc<ToolRegistry>,
    api: Arc<MockStreamingApiClient>,
    hooks: Arc<HookExecutorImpl>,
) -> ConversationOrchestrator {
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api,
        registry,
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

/// Turn 1 dispatches `tool_name` once; turn 2 ends with text.
fn two_turns(tu: ToolUseId, tool_name: &str) -> Arc<MockStreamingApiClient> {
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_tool_use(0, tu.clone(), tool_name),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let turn2 = scripted![
        message_start("m2", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    Arc::new(MockStreamingApiClient::with_turns(vec![turn1, turn2]))
}

/// A streaming turn whose tool returns `new_messages` replays those messages
/// into history IMMEDIATELY AFTER the `tool_result` user message, in
/// tool-dispatch order, and records each injected message's id → the injecting
/// tool's `tool_use_id` in `injected_message_sources`.
#[tokio::test]
async fn streaming_replays_tool_injected_new_messages_into_history() {
    let tu = ToolUseId::new();
    let api = two_turns(tu.clone(), "Inject");
    let orch = streaming_orch(registry_with(Arc::new(InjectingTool)), api);

    let outcome = orch.run_turn_streaming("go").await.expect("streaming turn");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "expected EndTurn, got {outcome:?}"
    );

    let session = orch.session();
    let s = session.lock().await;

    // History layout for the tool turn: [user "go", assistant(tool_use),
    // user(tool_result), user("EXPANDED-SKILL-PROMPT"), assistant("done")].
    // The injected user message must come DIRECTLY after the tool_result user
    // message and BEFORE the next assistant message.
    let injected_pos = s
        .history
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "EXPANDED-SKILL-PROMPT")))
        })
        .expect("injected skill-prompt message present in streaming history");

    // The message immediately BEFORE the injected one is the tool_result user
    // message (carries the ToolResult block tagged with this tool_use_id).
    let before = &s.history[injected_pos - 1];
    match before {
        ConversationMessage::User { content, .. } => {
            assert!(
                content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if *tool_use_id == tu)),
                "injected message must sit directly after the tool_result user message"
            );
        }
        other => {
            panic!("expected a tool_result User message before the injected one, got {other:?}")
        }
    }

    // The message AFTER the injected one is the next turn's assistant text.
    let after = &s.history[injected_pos + 1];
    assert!(
        matches!(after, ConversationMessage::Assistant { .. }),
        "the injected message must precede the next assistant message, got {after:?}"
    );

    // sourceToolUseID side-table records the injecting tool's tool_use_id.
    let injected_id = s.history[injected_pos].id();
    assert_eq!(
        s.injected_message_sources.get(&injected_id),
        Some(&tu),
        "injected message id maps to the Inject tool's tool_use_id"
    );
    assert_eq!(
        s.injected_message_sources.len(),
        1,
        "exactly one association recorded for one injected message"
    );
}

/// A streaming turn whose tool returns NO `new_messages` (the common case)
/// records nothing in `injected_message_sources`, injects no extra history
/// message, and never serializes a `sourceToolUseID` key — byte-identical to
/// before the SKILLEXEC.3 streaming wiring. Mirrors the batched
/// `normal_tool_records_no_source_and_serializes_no_field` test.
#[tokio::test]
async fn streaming_tool_without_new_messages_leaves_history_unchanged() {
    let tu = ToolUseId::new();
    let api = two_turns(tu, "Plain");
    let orch = streaming_orch(registry_with(Arc::new(PlainTool)), api);

    let outcome = orch.run_turn_streaming("go").await.expect("streaming turn");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "expected EndTurn, got {outcome:?}"
    );

    let session = orch.session();
    let s = session.lock().await;

    // No injected message text anywhere in history.
    assert!(
        !s.history.iter().any(|m| matches!(m, ConversationMessage::User { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "EXPANDED-SKILL-PROMPT")))),
        "a tool with no new_messages must not inject any history message"
    );

    // The side-table is empty and never serializes (#[serde(skip)]).
    assert!(
        s.injected_message_sources.is_empty(),
        "a tool with no injected messages records no source associations"
    );
    let json = serde_json::to_string(&*s).expect("serialize session");
    assert!(
        !json.contains("injected_message_sources"),
        "side-table must not serialize: {json}"
    );
    assert!(
        !json.contains("sourceToolUseID"),
        "no sourceToolUseID key may reach the wire: {json}"
    );

    // Exact history shape (byte-identical to a plain tool turn): user, assistant,
    // tool_result user, assistant. No extra trailing user message.
    assert_eq!(
        s.history.len(),
        4,
        "expected [user, assistant, tool_result, assistant]: {:?}",
        s.history
    );
}

/// HOOK.1 (streaming): a tool whose PreToolUse hook returns `additionalContext`
/// has that context replayed into streaming history as a SEPARATE meta user
/// message DIRECTLY AFTER the `tool_result` (NOT folded into the result
/// content) — the streaming counterpart of the batched
/// `hook1_additional_context_is_a_separate_message_not_folded` test. Confirms
/// the streaming driver (which drains the same `injected` channel) emits it
/// identically to the batched path (claude-code `toolExecution.ts:845`).
#[tokio::test]
async fn streaming_pre_tool_additional_context_is_a_separate_message_after_tool_result() {
    let tu = ToolUseId::new();
    let api = two_turns(tu.clone(), "Plain");
    // The `Plain` tool returns NO new_messages, so the ONLY injected message is
    // the PreToolUse additionalContext reminder.
    let orch = streaming_orch_with_hooks(
        registry_with(Arc::new(PlainTool)),
        api,
        context_pre_hook_executor(),
    );

    let outcome = orch.run_turn_streaming("go").await.expect("streaming turn");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "expected EndTurn, got {outcome:?}"
    );

    let session = orch.session();
    let s = session.lock().await;

    // The tool_result content must NOT carry the context (no folding).
    for m in &s.history {
        if let ConversationMessage::User { content, .. } = m {
            for b in content {
                if let ContentBlock::ToolResult { content, .. } = b {
                    assert!(
                        !content.contains("STREAM-CTX"),
                        "additionalContext must NOT be folded into the streaming tool_result: {content:?}"
                    );
                }
            }
        }
    }

    // The standalone reminder message sits DIRECTLY after the tool_result.
    let ctx_text = "<system-reminder>\nPreToolUse:Plain hook additional context: STREAM-CTX\n</system-reminder>";
    let ctx_pos = s
        .history
        .iter()
        .position(|m| {
            matches!(m, ConversationMessage::User { content, .. }
                if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == ctx_text)))
        })
        .expect("standalone additionalContext message present in streaming history");

    let before = &s.history[ctx_pos - 1];
    match before {
        ConversationMessage::User { content, .. } => assert!(
            content.iter().any(
                |b| matches!(b, ContentBlock::ToolResult { tool_use_id, .. } if *tool_use_id == tu)
            ),
            "the context message must sit directly after the tool_result user message"
        ),
        other => {
            panic!("expected a tool_result User message before the context one, got {other:?}")
        }
    }

    // Tagged with the dispatching tool's tool_use_id (TS toolUseID).
    let ctx_id = s.history[ctx_pos].id();
    assert_eq!(
        s.injected_message_sources.get(&ctx_id),
        Some(&tu),
        "context message id maps to the dispatching tool's tool_use_id"
    );
}
