use super::{dispatch_tool_uses_tracked, post_tool_batch_identity, ConversationOrchestrator};
use crate::test_support::{MockApiClient, MockOutputStream, NoOpPermissionGate};
use crate::OrchestratorConfig;
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::{BuiltinHookHandler, HookExecutorImpl};
use hooks::registry::HookRegistry;
use hooks::response::HookResponse;
use hooks::{HookContext, HookOutcome, HookResult};
use lingxi_core::types::{ContentBlock, ConversationMessage, HookId, ToolUseId};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
    ValidationError,
};

struct EchoTool;

#[async_trait]
impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
        &SCHEMA
    }
    /// Declared so a PostToolUse `updatedToolOutput` can FAIL validation
    /// and exercise the `hook_error_during_execution` arm.
    fn output_schema(&self) -> Option<&serde_json::Value> {
        static OUT: once_cell::sync::Lazy<serde_json::Value> = once_cell::sync::Lazy::new(|| {
            json!({
                "type": "object",
                "properties": { "out": { "type": "string" } },
                "required": ["out"]
            })
        });
        Some(&OUT)
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
        _ctx: &ToolUseContext,
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &ToolUseContext,
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "echo".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult {
            data: json!({ "out": "ECHOED-OUTPUT" }),
            model_content: Some("ECHOED-OUTPUT".into()),
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

struct UnusedHttp;
#[async_trait]
impl lingxi_core::host::HttpTransport for UnusedHttp {
    async fn request(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
    async fn stream_sse(
        &self,
        _req: lingxi_core::types::HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
        Err(lingxi_core::host::HttpError::InvalidRequest(
            "unused".into(),
        ))
    }
}

struct UnusedRuntime;
#[async_trait]
impl lingxi_core::host::RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _name: &str,
        _task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        Err(lingxi_core::host::RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: std::time::Duration) {}
    async fn cancel(
        &self,
        _h: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

struct FixedPostHook {
    response: HookResponse,
}

#[async_trait]
impl BuiltinHookHandler for FixedPostHook {
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(self.response.clone()),
        }
    }
    fn id(&self) -> &str {
        "fixed-post"
    }
}

fn post_hook_executor(response: HookResponse) -> Arc<HookExecutorImpl> {
    let hook = HookDefinition {
        id: HookId::new(),
        name: "fixed-post".into(),
        events: vec![HookEventType::PostToolUse],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: "fixed-post".into(),
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
    let reg = Arc::new(tokio::sync::RwLock::new(registry));
    let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FixedPostHook { response }));
    Arc::new(exec)
}

fn orch_with_post_hook(response: HookResponse) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        post_hook_executor(response),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

fn uses() -> Vec<(ToolUseId, String, serde_json::Value, Option<String>)> {
    vec![(ToolUseId::new(), "Echo".into(), json!({}), None)]
}

/// Same as [`post_hook_executor`] but registered for `PostToolBatch`, the
/// once-per-batch event fired after every tool in the batch has run.
fn batch_hook_executor(response: HookResponse) -> Arc<HookExecutorImpl> {
    let hook = HookDefinition {
        id: HookId::new(),
        name: "fixed-batch".into(),
        events: vec![HookEventType::PostToolBatch],
        if_condition: None,
        // Must match `FixedPostHook::id()` — the registry resolves the
        // builtin by handler id, and a mismatch silently never fires.
        executor: DefHookExecutor::Builtin {
            handler_id: "fixed-post".into(),
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
    let reg = Arc::new(tokio::sync::RwLock::new(registry));
    let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FixedPostHook { response }));
    Arc::new(exec)
}

fn orch_with_batch_hook(response: HookResponse) -> ConversationOrchestrator {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        batch_hook_executor(response),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

/// A `PostToolBatch` hook's `preventContinuation` STOPS the turn.
///
/// The batch fire used to discard its aggregate entirely
/// (`let _batch_agg = …`) under a comment calling `PostToolBatch`
/// "observational". The oracle disagrees (2.1.220 @233161375):
///
/// ```js
/// if(Mn.blockingError)Mr=!0,Qn??=Mn.blockingError.blockingError;
/// if(Mn.preventContinuation)Mr=!0,Qn??=Mn.stopReason
/// …
/// if(Mr)return yield Va({type:"hook_stopped_continuation",
///   message:Qn||"Execution stopped by PostToolBatch hook",
///   hookName:"PostToolBatch",toolUseID:rt,hookEvent:"PostToolBatch"},f),
///   n$e(er,a),{reason:"hook_stopped"}
/// ```
///
/// `{reason:"hook_stopped"}` is a turn-ending return, so the flag must
/// propagate — unlike the PostToolUse case, where the port deliberately
/// leaves the open question noted rather than guessing.
#[tokio::test]
async fn post_tool_batch_prevent_continuation_stops_the_turn() {
    let orch = orch_with_batch_hook(HookResponse {
        prevent_continuation: true,
        reason: Some("BATCH-STOP".into()),
        ..HookResponse::default()
    });
    let (_results, prevent, _injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(
        prevent,
        "a PostToolBatch hook requesting preventContinuation must end the turn"
    );
}

/// The MODEL must be told why the turn stopped.
///
/// The oracle yields the attachment into the message stream and derives the
/// prose from it later, in `normalizeAttachmentForAPI` (@238107808):
/// `hook_stopped_continuation:(e)=>[zr({content:Ww(`${e.hookName} hook
/// stopped continuation: ${e.message}`),isMeta:!0})]`. This port has no such
/// normalize layer — every other site (`Stop`, `PreToolUse`, `PostToolUse`)
/// builds the `<system-reminder>` prose explicitly beside the attachment —
/// so the batch site must too, or the stop reaches the transcript but never
/// the model.
///
/// Both records carry the SAME synthetic `hook-<uuid>` id, so the prose and
/// the attachment describe one event rather than drifting apart.
#[tokio::test]
async fn post_tool_batch_stop_is_explained_to_the_model() {
    let orch = orch_with_batch_hook(HookResponse {
        prevent_continuation: true,
        reason: Some("BATCH-STOP".into()),
        ..HookResponse::default()
    });
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    let stop_msg = injected
        .iter()
        .find(|(m, _)| m.text_content().contains("hook stopped continuation"))
        .expect("the batch stop must be explained to the model");
    assert_eq!(
        stop_msg.0.text_content(),
        "<system-reminder>\nPostToolBatch hook stopped continuation: BATCH-STOP\n</system-reminder>"
    );
    assert!(
        stop_msg.0.is_meta(),
        "the model-facing reminder is an ephemeral rendering of the durable attachment"
    );
    assert!(
        stop_msg.1.as_str().starts_with("hook-"),
        "prose and attachment must share the synthetic batch id, got {}",
        stop_msg.1.as_str()
    );
}

/// A `PostToolBatch` hook's `additionalContext` reaches the model.
///
/// The same discarded aggregate carried this too (@233161375, inside the
/// per-hook loop and therefore BEFORE the stop check):
///
/// ```js
/// if(Mn.additionalContexts&&Mn.additionalContexts.length>0){
///   let ko=Va({type:"hook_additional_context",content:Mn.additionalContexts,
///     hookName:"PostToolBatch",toolUseID:rt,hookEvent:"PostToolBatch"},f);
///   yield ko,Qe.push(ko)}
/// ```
///
/// It is independent of `preventContinuation`: a batch hook can contribute
/// context without stopping anything.
#[tokio::test]
async fn post_tool_batch_additional_context_reaches_the_model() {
    let orch = orch_with_batch_hook(HookResponse {
        additional_context: Some("BATCH-CTX".into()),
        ..HookResponse::default()
    });
    let (_results, prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(!prevent, "additionalContext alone must not stop the turn");
    let ctx_msg = injected
        .iter()
        .find(|(m, _)| m.text_content().contains("BATCH-CTX"))
        .expect("the batch additionalContext must reach the model");
    assert_eq!(
        ctx_msg.0.text_content(),
        "<system-reminder>\nPostToolBatch hook additional context: BATCH-CTX\n</system-reminder>"
    );
}

/// Ordering: the oracle yields `hook_additional_context` inside the per-hook
/// loop and the stop record only AFTER it, so a hook doing both produces the
/// context first.
#[tokio::test]
async fn batch_additional_context_is_ordered_before_the_stop() {
    let orch = orch_with_batch_hook(HookResponse {
        additional_context: Some("CTX".into()),
        prevent_continuation: true,
        reason: Some("STOP".into()),
        ..HookResponse::default()
    });
    let (_results, prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(prevent);
    let texts: Vec<String> = injected
        .iter()
        .map(|(m, _)| m.text_content())
        .filter(|t| t.contains("PostToolBatch"))
        .collect();
    assert_eq!(
        texts,
        vec![
            "<system-reminder>\nPostToolBatch hook additional context: CTX\n</system-reminder>",
            "<system-reminder>\nPostToolBatch hook stopped continuation: STOP\n</system-reminder>",
        ]
    );
}

/// A quiet batch hook injects nothing — the guard must not add a message to
/// every turn.
#[tokio::test]
async fn a_quiet_post_tool_batch_hook_injects_no_message() {
    let orch = orch_with_batch_hook(HookResponse::default());
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(
        !injected
            .iter()
            .any(|(m, _)| m.text_content().contains("hook stopped continuation")),
        "no stop, no explanation"
    );
}

/// `Mr` is set by a blocking error too, not only by `preventContinuation`.
#[tokio::test]
async fn post_tool_batch_blocking_error_also_stops_the_turn() {
    let orch = orch_with_batch_hook(HookResponse {
        decision: Some(hooks::response::HookDecision::Block),
        reason: Some("BATCH-BLOCK".into()),
        ..HookResponse::default()
    });
    let (_results, prevent, _injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(
        prevent,
        "`if(Mn.blockingError)Mr=!0` — a batch blocking error stops the turn too"
    );
}

/// A batch hook that asks for nothing leaves the turn alone — the guard
/// must not turn every batch into a stop.
#[tokio::test]
async fn a_quiet_post_tool_batch_hook_does_not_stop_the_turn() {
    let orch = orch_with_batch_hook(HookResponse::default());
    let (_results, prevent, _injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses(), None)
        .await
        .expect("dispatch");
    assert!(!prevent, "a no-op PostToolBatch hook must not end the turn");
}

/// The record the oracle yields alongside the stop: `hookName` and
/// `hookEvent` are the bare literal `PostToolBatch` (NOT suffixed with a
/// tool name the way `PostToolUse:{tool}` is), and `toolUseID` is the
/// SYNTHETIC `hook-${uuid}` the oracle binds as `rt` — no real tool's id,
/// because the event covers the whole batch.
#[test]
fn post_tool_batch_stopped_continuation_matches_the_oracle_shape() {
    let attachment = hooks::stopped_continuation_attachment(
        &post_tool_batch_identity(),
        "Execution stopped by PostToolBatch hook",
    );
    let id = attachment["toolUseID"].as_str().expect("toolUseID");
    assert!(
        id.starts_with("hook-"),
        "the batch attachment carries a synthetic `hook-<uuid>` id, got {id}"
    );
    assert_eq!(
        serde_json::to_string(&attachment).unwrap(),
        format!(
            r#"{{"type":"hook_stopped_continuation","message":"Execution stopped by PostToolBatch hook","hookName":"PostToolBatch","toolUseID":"{id}","hookEvent":"PostToolBatch"}}"#
        )
    );
}

/// The PostToolUse `additionalContext` reaches the model EXACTLY ONCE — as
/// the injected `isMeta` rendering — and is NOT also concatenated onto the
/// tool_result string.
#[tokio::test]
async fn post_tool_use_additional_context_is_not_folded_into_the_tool_result() {
    let orch = orch_with_post_hook(HookResponse {
        additional_context: Some("POST-CTX".into()),
        ..HookResponse::default()
    });
    let uses = uses();
    let (results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let ContentBlock::ToolResult { content, .. } = &results[0] else {
        panic!("expected ToolResult");
    };
    assert!(
        !content.contains("POST-CTX"),
        "claude never folds PostToolUse additionalContext into the \
         tool_result string (BIN off 235420375 / 234726655), got: {content}"
    );
    assert_eq!(injected.len(), 1, "exactly one model-facing rendering");
    assert!(
        injected[0].0.is_meta(),
        "the rendering is `zr({{isMeta:true}})` (BIN off 238107100)"
    );
}

/// The same context is queued as ONE `hook_additional_context` attachment
/// keyed to the tool, ready for the driver to flush after the tool_result.
#[tokio::test]
async fn post_tool_use_additional_context_is_queued_as_an_attachment() {
    let orch = orch_with_post_hook(HookResponse {
        additional_context: Some("POST-CTX".into()),
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    let _ = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        serde_json::to_string(&queued[0]).unwrap(),
        format!(
            r#"{{"type":"hook_additional_context","content":["POST-CTX"],"hookName":"PostToolUse:Echo","toolUseID":"{id}","hookEvent":"PostToolUse"}}"#
        )
    );
}

/// O2: a PostToolUse hook that BLOCKS produces a `hook_blocking_error`
/// attachment AND a model-facing `isMeta` rendering.
///
/// Before this, `post_agg.decision` was never read at all — a PostToolUse
/// hook exiting 2 produced NOTHING in the port, while the oracle produces
/// both records (BIN off 234726074 for the attachment, 238107476 for the
/// prose, which is one of the few hook attachments that IS model-facing).
///
/// `blockingError.command` is `qq(hook)`; the fixture hook is a Builtin, so
/// that renders as its handler id.
#[tokio::test]
async fn post_tool_use_block_emits_a_blocking_error_attachment_and_meta() {
    let orch = orch_with_post_hook(HookResponse {
        decision: Some(hooks::response::HookDecision::Block),
        reason: Some("nope".into()),
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");

    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        serde_json::to_string(&queued[0]).unwrap(),
        format!(
            r#"{{"type":"hook_blocking_error","hookName":"PostToolUse:Echo","toolUseID":"{id}","hookEvent":"PostToolUse","blockingError":{{"blockingError":"nope","command":"fixed-post"}}}}"#
        )
    );
    assert_eq!(injected.len(), 1, "one model-facing rendering");
    assert_eq!(
        injected[0].0.text_content(),
        "<system-reminder>\nPostToolUse:Echo hook blocking error from command: \"fixed-post\": nope\n</system-reminder>"
    );
}

/// The blocking-error default reason is `"Blocked by hook"` (capital B) —
/// `e.reason||"Blocked by hook"` at BIN off 237775430.
#[tokio::test]
async fn post_tool_use_block_without_a_reason_uses_the_oracle_default() {
    let orch = orch_with_post_hook(HookResponse {
        decision: Some(hooks::response::HookDecision::Block),
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    let _ = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(
        queued[0]["blockingError"]["blockingError"],
        "Blocked by hook"
    );
}

/// O2: the PostToolUse `preventContinuation` message was already
/// byte-correct for the MODEL, but nothing was ever PERSISTED. The oracle
/// records a `hook_stopped_continuation` attachment beside it
/// (BIN off 234726408), whose `message` sits SECOND in key order.
#[tokio::test]
async fn post_tool_use_prevent_continuation_is_persisted_as_an_attachment() {
    let orch = orch_with_post_hook(HookResponse {
        prevent_continuation: true,
        reason: Some("POST-STOP".into()),
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    // NOTE: the returned `prevent` flag is deliberately NOT asserted here.
    // The port sets its loop flag from the PRE-hook aggregate only
    // (`turn_loop.rs:2646`); `post_agg.prevent_continuation` reaches the
    // message/attachment but not the flag. Whether the oracle's `return` at
    // BIN off 234726408 ends the whole turn or only the post-hook generator
    // is a SEPARATE question this cluster did not investigate — see the
    // residual note rather than assuming an answer here.
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");

    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        serde_json::to_string(&queued[0]).unwrap(),
        format!(
            r#"{{"type":"hook_stopped_continuation","message":"POST-STOP","hookName":"PostToolUse:Echo","toolUseID":"{id}","hookEvent":"PostToolUse"}}"#
        )
    );
    // The model-facing prose is UNCHANGED by this work.
    assert_eq!(
        injected[0].0.text_content(),
        "<system-reminder>\nPostToolUse:Echo hook stopped continuation: POST-STOP\n</system-reminder>"
    );
}

/// Both records for one hook, in the oracle's yield order: the
/// blocking-error comes BEFORE the stopped-continuation (BIN off 234726074
/// yields `hook_blocking_error`, then `preventContinuation` returns).
#[tokio::test]
async fn blocking_error_is_ordered_before_stopped_continuation() {
    let orch = orch_with_post_hook(HookResponse {
        decision: Some(hooks::response::HookDecision::Block),
        reason: Some("both".into()),
        prevent_continuation: true,
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    let _ = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let queued = orch.take_queued_hook_attachments(&id).await;
    let kinds: Vec<_> = queued
        .iter()
        .map(|v| v["type"].as_str().unwrap_or_default())
        .collect();
    assert_eq!(kinds, ["hook_blocking_error", "hook_stopped_continuation"]);
}

/// END-TO-END (O1): the value recorded at DISPATCH reaches the transcript
/// LINE. Guards against `record_tool_use_result` being computed but never
/// published — the record site lives in `turn_loop.rs` and the consume site
/// in `conversation.rs`, so neither file's unit tests alone prove the seam.
#[tokio::test]
async fn dispatched_tool_result_reaches_the_transcript_as_tool_use_result() {
    use lingxi_core::types::MessageId;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        crate::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    let uses = uses();
    let (results, _prevent, _injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let msg = ConversationMessage::User {
        id: MessageId::new(),
        content: results,
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    assert!(
        raw.contains(r#""toolUseResult":{"out":"ECHOED-OUTPUT"}"#),
        "the tool's structured `data` must reach the line verbatim, got: {raw}"
    );
}

/// O3: a PostToolUse `updatedToolOutput` that fails the tool's output
/// schema produces a `hook_error_during_execution` ATTACHMENT and NOTHING
/// the model can see — the renderer maps that attachment type to `[]`
/// (BIN off 238107100). The port used to push the notice text onto the
/// injected channel, so the model read a warning claude suppresses.
#[tokio::test]
async fn schema_mismatch_notice_is_an_attachment_the_model_never_sees() {
    let orch = orch_with_post_hook(HookResponse {
        updated_tool_output: Some(Some(json!({ "unexpected": true }))),
        ..HookResponse::default()
    });
    let uses = uses();
    let id = uses[0].0.clone();
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    assert!(
        !injected
            .iter()
            .any(|(m, _)| m.text_content().contains("does not match")),
        "the schema-mismatch notice must not reach the model: {injected:?}"
    );
    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        queued[0].get("type").and_then(serde_json::Value::as_str),
        Some("hook_error_during_execution")
    );
    assert!(queued[0]
        .get("content")
        .and_then(serde_json::Value::as_str)
        .expect("string content")
        .contains("does not match Echo's output shape"));
}

/// Build an orchestrator whose only hook is a PreToolUse hook returning
/// `response`, optionally with a JSONL writer so persisted attachment
/// LINES (not just queued payloads) can be inspected.
fn orch_with_pre_hook(
    response: HookResponse,
    jsonl: Option<&std::path::Path>,
) -> ConversationOrchestrator {
    let hook = HookDefinition {
        id: HookId::new(),
        name: "fixed-pre".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: "fixed-post".into(),
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
    let mut exec = HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    exec.register_builtin(Arc::new(FixedPostHook { response }));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        Arc::new(exec),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        jsonl.map_or_else(
            || PathBuf::from("/tmp"),
            |p| p.parent().expect("parent").to_path_buf(),
        ),
    );
    match jsonl {
        None => orch,
        Some(path) => {
            let root = path.parent().expect("parent").to_path_buf();
            let fs: Arc<dyn lingxi_core::host::FileSystem> =
                Arc::new(platform_posix::fs::PosixFileSystem::new(root));
            orch.with_jsonl_writer(Arc::new(session::jsonl::writer::JsonlWriter::new(
                path.to_path_buf(),
                fs,
            )))
        }
    }
}

/// O2: the PreToolUse `preventContinuation` record. The model-facing prose
/// was already byte-correct (`Execution stopped by hook` default, BIN off
/// 235403061); only the persisted attachment was missing.
#[tokio::test]
async fn pre_tool_use_prevent_continuation_is_persisted_as_an_attachment() {
    let orch = orch_with_pre_hook(
        HookResponse {
            prevent_continuation: true,
            ..HookResponse::default()
        },
        None,
    );
    let uses = uses();
    let id = uses[0].0.clone();
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        serde_json::to_string(&queued[0]).unwrap(),
        format!(
            r#"{{"type":"hook_stopped_continuation","message":"Execution stopped by hook","hookName":"PreToolUse:Echo","toolUseID":"{id}","hookEvent":"PreToolUse"}}"#
        )
    );
    assert!(
        injected.iter().any(|(m, _)| m.text_content()
            == "<system-reminder>\nPreToolUse:Echo hook stopped continuation: Execution stopped by hook\n</system-reminder>"),
        "the existing prose is unchanged: {injected:?}"
    );
}

/// O2 / Phase 4: a DEFERRED tool persists a `hook_deferred_tool` attachment
/// LINE and sends the model NOTHING.
///
/// The port previously pushed the raw JSON payload onto the injected
/// channel as a plain user message, so the model read a blob the oracle
/// suppresses (`hook_deferred_tool:()=>[]`, BIN off 238109388) while the
/// resume scanner `QAs` (BIN off 237925753) — which greps the transcript
/// for `'"hook_deferred_tool"'` inside a `type:"attachment"` line — found
/// nothing at all.
///
/// The record must be persisted IMMEDIATELY rather than queued: the queue
/// is flushed after a tool_result, and a deferred tool never produces one.
#[tokio::test]
async fn deferred_tool_is_persisted_and_never_shown_to_the_model() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_pre_hook(
        HookResponse {
            decision: Some(hooks::response::HookDecision::Defer),
            ..HookResponse::default()
        },
        Some(&path),
    );
    let uses = uses();
    let id = uses[0].0.clone();
    let (results, prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");

    assert!(prevent, "a deferred tool terminates the turn");
    assert!(results.is_empty(), "the deferred tool never ran");
    assert!(
        !injected
            .iter()
            .any(|(m, _)| m.text_content().contains("hook_deferred_tool")),
        "the model must NEVER see the deferred-tool payload: {injected:?}"
    );

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let line = raw
        .lines()
        .find(|l| l.contains("hook_deferred_tool"))
        .unwrap_or_else(|| panic!("no hook_deferred_tool attachment line in: {raw}"));
    let v: serde_json::Value = serde_json::from_str(line).expect("json line");
    assert_eq!(
        v["type"], "attachment",
        "`QAs` requires the enclosing line to be type:\"attachment\""
    );
    assert_eq!(
        serde_json::to_string(&v["attachment"]).unwrap(),
        format!(
            r#"{{"type":"hook_deferred_tool","toolUseID":"{id}","toolName":"Echo","toolInput":{{}},"hookName":"session","hookEvent":"PreToolUse","permissionMode":"default"}}"#
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_tool_persists_traceparent_when_current_trace_is_attached() {
    let trace_context = telemetry::otel::SerializedTraceContext {
        traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
        tracestate: Some("foo=bar".into()),
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_pre_hook(
        HookResponse {
            decision: Some(hooks::response::HookDecision::Defer),
            ..HookResponse::default()
        },
        Some(&path),
    );
    let uses = uses();
    let id = uses[0].0.clone();
    let (_results, _prevent, _injected, _mods) = telemetry::otel::with_trace_context_future(
        Some(&trace_context),
        dispatch_tool_uses_tracked(&orch, &uses, None),
    )
    .await
    .expect("dispatch");

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let line = raw
        .lines()
        .find(|l| l.contains("hook_deferred_tool"))
        .unwrap_or_else(|| panic!("no hook_deferred_tool attachment line in: {raw}"));
    let v: serde_json::Value = serde_json::from_str(line).expect("json line");
    assert_eq!(
        serde_json::to_string(&v["attachment"]).unwrap(),
        format!(
            r#"{{"type":"hook_deferred_tool","toolUseID":"{id}","toolName":"Echo","toolInput":{{}},"hookName":"session","hookEvent":"PreToolUse","permissionMode":"default","traceparent":"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"}}"#
        )
    );
}

/// A PreToolUse `additionalContext` gets the same treatment.
#[tokio::test]
async fn pre_tool_use_additional_context_is_queued_and_rendered_as_meta() {
    let hook = HookDefinition {
        id: HookId::new(),
        name: "fixed-pre".into(),
        events: vec![HookEventType::PreToolUse],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: "fixed-post".into(),
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
    let reg = Arc::new(tokio::sync::RwLock::new(registry));
    let mut exec = HookExecutorImpl::new(reg, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FixedPostHook {
        response: HookResponse {
            additional_context: Some("PRE-CTX".into()),
            ..HookResponse::default()
        },
    }));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        Arc::new(exec),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let uses = uses();
    let id = uses[0].0.clone();
    let (_results, _prevent, injected, _mods) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch");
    let queued = orch.take_queued_hook_attachments(&id).await;
    assert_eq!(queued.len(), 1, "one attachment, got {queued:?}");
    assert_eq!(
        queued[0]
            .get("hookName")
            .and_then(serde_json::Value::as_str),
        Some("PreToolUse:Echo")
    );
    let rendering = injected
        .iter()
        .find(|(m, _)| matches!(m, ConversationMessage::User { .. }))
        .expect("a rendering");
    assert!(
        rendering.0.is_meta(),
        "the PreToolUse rendering is isMeta too"
    );
}
