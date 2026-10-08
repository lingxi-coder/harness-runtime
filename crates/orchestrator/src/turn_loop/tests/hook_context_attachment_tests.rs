use super::{
    ConversationOrchestrator, PostToolBatchDispatch, dispatch_tool_uses_tracked,
    post_tool_batch_identity, run_post_tool_batch_hooks,
};
use crate::OrchestratorConfig;
use crate::test_support::{MockApiClient, MockOutputStream, NoOpPermissionGate};
use async_trait::async_trait;
use hooks::attachment::HookPublicationGuard;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::{BuiltinHookHandler, HookExecutorImpl};
use hooks::registry::HookRegistry;
use hooks::response::HookResponse;
use hooks::{HookContext, HookOutcome, HookResult};
use lingxi_core::types::{ContentBlock, ConversationMessage, HookId, MessageId, ToolUseId};
use serde_json::json;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
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
    started: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait]
impl BuiltinHookHandler for FixedPostHook {
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let Some(started) = &self.started {
            started.notify_one();
        }
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
    exec.register_builtin(Arc::new(FixedPostHook {
        response,
        started: None,
    }));
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

struct FailedAgentTool(
    Option<lingxi_core::types::AgentId>,
    &'static str,
    Option<Arc<HookExecutorImpl>>,
);

#[async_trait]
impl Tool for FailedAgentTool {
    fn name(&self) -> &str {
        "Agent"
    }
    fn input_schema(&self) -> &serde_json::Value {
        EchoTool.input_schema()
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
    async fn check_permissions(
        &self,
        input: &serde_json::Value,
        ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        EchoTool.check_permissions(input, ctx).await
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "Failure lifecycle fixture".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if let (Some(child), Some(executor)) = (self.0, &self.2) {
            let session = ctx.session.as_ref().unwrap().lock().await.session_id;
            executor
                .subagent_stop_firer(session)
                .unwrap()
                .fire(
                    child,
                    "general-purpose",
                    lingxi_core::host::subagent_spawn::SubagentStopStatus::Failed,
                )
                .await;
        }
        Err(match self.0 {
            Some(agent_id) => ToolError::SubagentFailed {
                agent_id,
                reason: self.1.into(),
                terminal_hooks_owned: self.2.is_some(),
            },
            None => ToolError::Internal(self.1.into()),
        })
    }
}

struct RecordingSubagentLifecycle(Arc<Mutex<Vec<(HookEvent, HookContext)>>>);

#[async_trait]
impl BuiltinHookHandler for RecordingSubagentLifecycle {
    async fn handle(&self, event: &HookEvent, ctx: &HookContext) -> HookResult {
        self.0.lock().unwrap().push((event.clone(), ctx.clone()));
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
            response: None,
        }
    }
    fn id(&self) -> &str {
        "record-subagent-lifecycle"
    }
}

#[tokio::test]
async fn failed_agent_dispatch_consumes_its_child_route_and_admission_failure_has_no_lifecycle() {
    for (failed_child, reason, owned) in [
        (true, "provider request failed", false),
        (true, "owned foreground failure", true),
        (false, "spawn admission failed", false),
        // The real Agent tool's Killed result becomes this ordinary tool
        // error. Cancellation never runs the child's stop hooks.
        (false, "Agent: subagent was killed", false),
    ] {
        let child_id = lingxi_core::types::AgentId::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut hook_registry = HookRegistry::new();
        hook_registry.register(HookDefinition {
            id: HookId::new(),
            name: "record-subagent-lifecycle".into(),
            events: vec![HookEventType::SubagentStart, HookEventType::SubagentStop],
            if_condition: None,
            executor: DefHookExecutor::Builtin {
                handler_id: "record-subagent-lifecycle".into(),
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
        });
        let mut executor = HookExecutorImpl::new(
            Arc::new(tokio::sync::RwLock::new(hook_registry)),
            Arc::new(UnusedHttp),
            Arc::new(UnusedRuntime),
        );
        executor.register_builtin(Arc::new(RecordingSubagentLifecycle(seen.clone())));
        let hooks = Arc::new(executor);
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(FailedAgentTool(
            failed_child.then_some(child_id),
            reason,
            owned.then(|| hooks.clone()),
        )));
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig {
                model: "parent-model".into(),
                ..Default::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(registry),
            hooks.clone(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let orch = ConversationOrchestrator::into_shared(orch);
        let session_id = {
            let mut session = orch.session.lock().await;
            session.model_profile = Some("parent-profile".into());
            session.session_id
        };
        let child_route = hooks::HookModelSelection {
            model: "child-model".into(),
            model_profile: Some("child-profile".into()),
        };
        let child_history = hooks::PromptHookTranscript {
            messages: vec![ConversationMessage::user(
                MessageId::new(),
                "child-only evidence".into(),
            )],
            ..Default::default()
        };
        hooks.publish_agent_prompt_transcript(
            session_id,
            child_id,
            child_route.clone(),
            child_history.clone(),
            hooks::AgentStopMetadata {
                owner: owned.then(|| hooks.subagent_stop_firer(session_id).unwrap()),
                ..Default::default()
            },
        );
        let uses = vec![(
            ToolUseId::new(),
            "Agent".into(),
            json!({"subagent_type":"general-purpose"}),
            None,
        )];
        let (results, _, _, _) = dispatch_tool_uses_tracked(&orch, &uses, None)
            .await
            .expect("dispatch");
        let expected_content = format!("Error: {reason}");
        assert!(
            matches!(&results[0], ContentBlock::ToolResult { is_error: Some(true), content, .. }
            if content == &expected_content)
        );
        let calls = seen.lock().unwrap();
        if failed_child {
            assert_eq!(
                calls.len(),
                1,
                "a terminal failure cannot manufacture SubagentStart"
            );
            assert!(
                matches!(&calls[0].0, HookEvent::SubagentStop { agent_id, status, .. }
                if *agent_id == child_id && status == "failed")
            );
            assert_eq!(calls[0].1.model_selection.as_ref(), Some(&child_route));
            assert_eq!(
                calls[0].1.prompt_transcript.as_ref().unwrap().messages,
                child_history.messages
            );
            assert!(hooks
                .take_agent_prompt_transcript(session_id, child_id)
                .is_none());
        } else {
            assert!(
                calls.is_empty(),
                "admission and cancellation have no stop lifecycle"
            );
            assert!(hooks
                .take_agent_prompt_transcript(session_id, child_id)
                .is_some());
        }
    }
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
    exec.register_builtin(Arc::new(FixedPostHook {
        response,
        started: None,
    }));
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

struct CancellingBatchHook {
    observed_guard: Arc<AtomicBool>,
}

#[async_trait]
impl BuiltinHookHandler for CancellingBatchHook {
    async fn handle(&self, _event: &HookEvent, ctx: &HookContext) -> HookResult {
        if let Some(guard) = ctx.publication_guard.as_ref() {
            self.observed_guard.store(true, Ordering::Release);
            if let Some(root) = guard.generation_cancellation_token() {
                root.cancel();
            }
        }
        HookResult {
            outcome: HookOutcome::Success,
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            response: Some(HookResponse {
                additional_context: Some("stale context".into()),
                prevent_continuation: true,
                ..HookResponse::default()
            }),
        }
    }

    fn id(&self) -> &str {
        "cancelling-batch"
    }
}

#[tokio::test]
async fn post_tool_batch_executor_receives_and_honors_generation_guard() {
    let observed_guard = Arc::new(AtomicBool::new(false));
    let hook = HookDefinition {
        id: HookId::new(),
        name: "cancelling-batch".into(),
        events: vec![HookEventType::PostToolBatch],
        if_condition: None,
        executor: DefHookExecutor::Builtin {
            handler_id: "cancelling-batch".into(),
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
    let mut executor = HookExecutorImpl::new(
        Arc::new(tokio::sync::RwLock::new(registry)),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    executor.register_builtin(Arc::new(CancellingBatchHook {
        observed_guard: Arc::clone(&observed_guard),
    }));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        Arc::new(executor),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    let root = lingxi_core::host::CancellationToken::new();
    let fence = crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
        root.clone(),
        Arc::new(tokio::sync::Mutex::new(())),
    );
    let outcome = run_post_tool_batch_hooks(
        &orch,
        PostToolBatchDispatch {
            tool_calls: vec![hooks::events::PostToolBatchCall {
                tool_name: "Echo".into(),
                tool_input: json!({}),
                tool_use_id: ToolUseId::new(),
                tool_response: Some(json!("done")),
            }],
            publication_guard: Some(Arc::new(fence)),
        },
    )
    .await;

    assert!(
        observed_guard.load(Ordering::Acquire),
        "HookContext carries the guard"
    );
    assert!(
        root.is_cancelled(),
        "the handler retired its originating generation"
    );
    assert!(!outcome.prevent_continuation);
    assert!(outcome.injected_messages.is_empty());
    assert!(
        orch.prompt_runtime
            .guarded_prompt_messages
            .lock()
            .await
            .is_empty()
    );
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0].0.to_json_string().unwrap(),
        format!(
            r#"{{"type":"hook_additional_context","content":["POST-CTX"],"hookName":"PostToolUse:Echo","toolUseID":"{id}","hookEvent":"PostToolUse"}}"#
        )
    );
}

async fn screen_injected(
    orch: &ConversationOrchestrator,
    injected: &[(ConversationMessage, ToolUseId)],
) -> Vec<ConversationMessage> {
    let mut messages = injected
        .iter()
        .map(|(message, _)| message.clone())
        .collect();
    orch.screen_mod_persisted_attachments(&mut messages).await;
    messages
}

#[tokio::test]
async fn mod_screens_hook_context_without_changing_its_recorded_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("hook-context-mod.js");
    std::fs::write(
        &module,
        r#"let rewrites = 0;
        export function register(on) {
          on('prompt.attachment', { type: 'hook_additional_context' }, ($, e, next) => {
            if (e.origin.kind !== 'hook') throw new Error('missing hook origin');
            if (e.origin.event === 'PostToolUse') return { text: null };
            return next({ ...e, text: `modded ${++rewrites} ${e.origin.event}: ${e.text}` });
          });
          on('prompt.attachment', { type: 'hook_blocking_error' }, ($, e) => ({
            text: `modded ${e.origin.event} block`,
          }));
          on('prompt.attachment', { type: 'hook_stopped_continuation' }, () => ({ text: null }));
          on('prompt.submit', ($, e, next) => {
            $.ui.invalidate('prompt.attachment');
            return next(e);
          });
        }"#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("hook-context-mod", dir.path(), &module, json!({}))
        .await
        .unwrap();

    let mut pre_registry = HookRegistry::new();
    pre_registry.set_mod_host(host.clone());
    let pre = orch_with_pre_hook(
        HookResponse {
            additional_context: Some("PRE-CTX".into()),
            ..HookResponse::default()
        },
        None,
    )
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(pre_registry)));
    let (_, _, injected, _) = dispatch_tool_uses_tracked(&pre, &uses(), None)
        .await
        .unwrap();
    assert!(
        injected
            .iter()
            .any(|(message, _)| message.text_content().contains("PRE-CTX"))
    );
    let original = injected[0].0.clone();
    pre.session.lock().await.history.push(original.clone());
    let prepared = pre
        .prepare_turn_step(
            crate::conversation::ModelCallPath::Batched,
            None,
            true,
            true,
            None,
        )
        .await
        .unwrap();
    assert!(prepared.snapshot.iter().any(|message| {
        message
            .text_content()
            .contains("modded 1 PreToolUse: PreToolUse:Echo hook additional context: PRE-CTX")
    }));
    assert!(
        pre.session
            .lock()
            .await
            .history
            .iter()
            .any(|message| message == &original)
    );
    let mut cached = pre.session.lock().await.history.clone();
    let mut turn_reminders = Vec::new();
    let mut guarded_async_hook_reminders = Vec::new();
    pre.reattach_outgoing_context(
        &mut cached,
        None,
        None,
        &mut turn_reminders,
        &mut guarded_async_hook_reminders,
        &prepared.context_announcements,
        false,
    )
    .await;
    assert!(
        cached
            .iter()
            .any(|message| message.text_content().contains("modded 1 PreToolUse"))
    );
    host.dispatch_with_log_at_session(
        "prompt.submit",
        json!({"text":"invalidate"}),
        &pre,
        |event| async move { Ok(event) },
        |_, _| async {},
    )
    .await
    .unwrap();
    let mut refreshed = pre.session.lock().await.history.clone();
    pre.reattach_outgoing_context(
        &mut refreshed,
        None,
        None,
        &mut turn_reminders,
        &mut guarded_async_hook_reminders,
        &prepared.context_announcements,
        false,
    )
    .await;
    assert!(
        refreshed
            .iter()
            .any(|message| message.text_content().contains("modded 2 PreToolUse"))
    );
    assert!(
        pre.session
            .lock()
            .await
            .history
            .iter()
            .any(|message| message == &original)
    );

    let mut post_registry = HookRegistry::new();
    post_registry.set_mod_host(host.clone());
    let post = orch_with_post_hook(HookResponse {
        additional_context: Some("POST-CTX".into()),
        ..HookResponse::default()
    })
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(post_registry)));
    let use_batch = uses();
    let (_, _, injected, _) = dispatch_tool_uses_tracked(&post, &use_batch, None)
        .await
        .unwrap();
    assert!(
        injected
            .iter()
            .any(|(message, _)| message.text_content().contains("POST-CTX"))
    );
    assert!(
        screen_injected(&post, &injected)
            .await
            .iter()
            .all(|message| !message.text_content().contains("POST-CTX"))
    );
    let recorded = post.take_queued_hook_attachments(&use_batch[0].0).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0.value["type"], "hook_additional_context");
    assert_eq!(recorded[0].0.value["content"], json!(["POST-CTX"]));

    let mut block_registry = HookRegistry::new();
    block_registry.set_mod_host(host.clone());
    let blocked = orch_with_post_hook(HookResponse {
        decision: Some(hooks::response::HookDecision::Block),
        reason: Some("BLOCKED".into()),
        ..HookResponse::default()
    })
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(block_registry)));
    let use_batch = uses();
    let (_, _, injected, _) = dispatch_tool_uses_tracked(&blocked, &use_batch, None)
        .await
        .unwrap();
    assert!(
        screen_injected(&blocked, &injected)
            .await
            .iter()
            .any(|message| {
                message.text_content()
                    == "<system-reminder>\nmodded PostToolUse block\n</system-reminder>"
            })
    );
    let recorded = blocked.take_queued_hook_attachments(&use_batch[0].0).await;
    assert!(
        recorded
            .iter()
            .any(|attachment| attachment.0.value["type"] == "hook_blocking_error")
    );

    let mut stop_registry = HookRegistry::new();
    stop_registry.set_mod_host(host);
    let stopped = orch_with_post_hook(HookResponse {
        prevent_continuation: true,
        reason: Some("STOPPED".into()),
        ..HookResponse::default()
    })
    .with_hook_registry(Arc::new(tokio::sync::RwLock::new(stop_registry)));
    let use_batch = uses();
    let (_, _, injected, _) = dispatch_tool_uses_tracked(&stopped, &use_batch, None)
        .await
        .unwrap();
    assert!(
        injected
            .iter()
            .any(|(message, _)| { message.text_content().contains("hook stopped continuation") })
    );
    assert!(
        screen_injected(&stopped, &injected)
            .await
            .iter()
            .all(|message| !message.text_content().contains("hook stopped continuation"))
    );
    let recorded = stopped.take_queued_hook_attachments(&use_batch[0].0).await;
    assert!(
        recorded
            .iter()
            .any(|attachment| attachment.0.value["type"] == "hook_stopped_continuation")
    );
}

struct GatePublicationGuard {
    root: lingxi_core::host::CancellationToken,
    lock: Arc<tokio::sync::Mutex<()>>,
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl HookPublicationGuard for GatePublicationGuard {
    fn is_current(&self) -> bool {
        !self.root.is_cancelled()
    }

    fn generation_cancellation_token(&self) -> Option<lingxi_core::host::CancellationToken> {
        Some(self.root.clone())
    }

    fn publish_if_current<'a>(
        &'a self,
        publication: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let root = self.root.clone();
        let lock = Arc::clone(&self.lock);
        let entered = self.entered.lock().unwrap().take();
        Box::pin(async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            let _lease = tokio::select! {
                biased;
                () = root.cancelled() => return false,
                lease = lock.lock_owned() => lease,
            };
            if root.is_cancelled() {
                return false;
            }
            tokio::select! {
                biased;
                () = root.cancelled() => false,
                () = publication => true,
            }
        })
    }

    fn commit_if_current<'a>(
        &'a self,
        mutation: Pin<Box<dyn Future<Output = ()> + Send + 'a>>,
    ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
        let root = self.root.clone();
        let lock = Arc::clone(&self.lock);
        let entered = self.entered.lock().unwrap().take();
        Box::pin(async move {
            if let Some(entered) = entered {
                let _ = entered.send(());
            }
            let _lease = tokio::select! {
                biased;
                () = root.cancelled() => return false,
                lease = lock.lock_owned() => lease,
            };
            if root.is_cancelled() {
                return false;
            }
            mutation.await;
            true
        })
    }
}

#[tokio::test]
async fn reset_rejects_queued_hook_attachment_metadata_registration() {
    let orch = orch_with_post_hook(HookResponse::default());
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let held_lease = lock.clone().lock_owned().await;
    let root = lingxi_core::host::CancellationToken::new();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let guard = Arc::new(GatePublicationGuard {
        root: root.clone(),
        lock: Arc::clone(&lock),
        entered: Mutex::new(Some(entered_tx)),
    });
    let stale_message = ConversationMessage::user_meta(
        lingxi_core::types::MessageId::new(),
        "stale hook context".into(),
    );
    let before = orch
        .prompt_runtime
        .mod_persisted_attachments
        .lock()
        .await
        .len();
    let stale_registration = super::tool_dispatch::register_mod_persisted_attachment_if_visible(
        &orch,
        &stale_message,
        "hook_additional_context",
        json!({"kind":"hook","event":"PostToolUse"}),
        Some(guard.as_ref()),
    );
    let reset = async move {
        entered_rx.await.expect("registration queued on the lease");
        root.cancel();
        drop(held_lease);
    };
    let _ = tokio::join!(stale_registration, reset);
    assert_eq!(
        orch.prompt_runtime
            .mod_persisted_attachments
            .lock()
            .await
            .len(),
        before,
        "reset-rejected hook metadata must not remain in prompt state"
    );

    let fresh_root = lingxi_core::host::CancellationToken::new();
    let (fresh_entered_tx, fresh_entered_rx) = tokio::sync::oneshot::channel();
    let fresh_guard = Arc::new(GatePublicationGuard {
        root: fresh_root,
        lock: Arc::new(tokio::sync::Mutex::new(())),
        entered: Mutex::new(Some(fresh_entered_tx)),
    });
    let fresh_message = ConversationMessage::user_meta(
        lingxi_core::types::MessageId::new(),
        "fresh hook context".into(),
    );
    let fresh_registration = super::tool_dispatch::register_mod_persisted_attachment_if_visible(
        &orch,
        &fresh_message,
        "hook_additional_context",
        json!({"kind":"hook","event":"PostToolUse"}),
        Some(fresh_guard.as_ref()),
    );
    let ((), entered) = tokio::join!(fresh_registration, fresh_entered_rx);
    entered.expect("fresh registration entered its lease");
    assert_eq!(
        orch.prompt_runtime
            .mod_persisted_attachments
            .lock()
            .await
            .len(),
        before + 1,
        "a current-generation hook attachment still registers"
    );
}

#[tokio::test]
async fn accepted_assistant_row_keeps_its_fence_until_request_admission() {
    let orch = orch_with_batch_hook(HookResponse::default());
    let root = lingxi_core::host::CancellationToken::new();
    let guard: Arc<dyn HookPublicationGuard> = Arc::new(GatePublicationGuard {
        root: root.clone(),
        lock: Arc::new(tokio::sync::Mutex::new(())),
        entered: Mutex::new(None),
    });
    let row = ConversationMessage::Assistant { per_turn_effort: None,
        id: lingxi_core::types::MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "accepted assistant row".into(),
            citations: None,
        }],
        stop_reason: Some("tool_use".into()),
    };
    let row_id = row.id();

    assert!(
        orch.append_streamed_assistant_to_history(&row, Some(Arc::clone(&guard)), false)
            .await
    );
    assert!(
        orch.session
            .lock()
            .await
            .history
            .iter()
            .any(|message| message.id() == row_id)
    );

    // Reset can happen after the durable history append but before the next
    // Main request is built. Keep the stale (ID, guard) pair so final request
    // admission can remove this row instead of sending it without authority.
    root.cancel();
    let pending = orch
        .prompt_runtime
        .take_guarded_prompt_message_guards()
        .await;
    assert!(pending
        .iter()
        .any(|(message_id, pending_guard)| *message_id == row_id && !pending_guard.is_current()));
    let mut request_messages = vec![row];
    let mut turn_reminders = Vec::new();
    let mut guards = pending;
    crate::prompt::async_hook_response::retain_current_async_hook_reminders(
        &mut request_messages,
        &mut turn_reminders,
        &mut guards,
    );
    assert!(request_messages.is_empty());
    assert!(guards.is_empty());
}

#[tokio::test]
async fn guarded_injected_row_runs_session_append_under_its_generation_guard() {
    use lingxi_core::types::MessageId;

    let dir = tempfile::tempdir().expect("tempdir");
    let module = dir.path().join("session-append.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('session.append', ($, event, next) => {
            const block = event.message.content[0];
            if (event.message.type === 'user' && block?.type === 'text'
                && block.text === 'injected original') {
              $.ui.log('guarded session.append');
              return next({ ...event, message: { ...event.message, content: [
                { ...block, text: 'injected accepted' }
              ] } });
            }
            return next(event);
          });
        }
        "#,
    )
    .expect("write session.append fixture");

    let host = hooks::mods::ModHost::start(None)
        .await
        .expect("start Mod host");
    host.load("guarded-injected-row", dir.path(), &module, json!({}))
        .await
        .expect("load session.append Mod");
    let mut hook_registry = HookRegistry::new();
    hook_registry.set_mod_host(host);
    let output = Arc::new(MockOutputStream::new());
    let jsonl_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(dir.path().to_path_buf()),
    );
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        jsonl_path.clone(),
        fs,
    ));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(ToolRegistry::new()),
            crate::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(crate::test_support::StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(hook_registry)))
        .with_jsonl_writer(writer),
    );

    let message = ConversationMessage::user(MessageId::new(), "injected original".into());
    let guard: Arc<dyn HookPublicationGuard> = Arc::new(
        crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
            lingxi_core::host::CancellationToken::new(),
            Arc::new(tokio::sync::Mutex::new(())),
        ),
    );
    assert!(
        orch.append_guarded_injected_message(&message, ToolUseId::new(), guard)
            .await
    );

    let history = orch.session.lock().await.history.clone();
    let accepted = history
        .iter()
        .find(|row| row.id() == message.id())
        .expect("injected row in history");
    assert!(matches!(
        accepted,
        ConversationMessage::User { content, .. }
            if matches!(content.as_slice(), [ContentBlock::Text { text, .. }] if text == "injected accepted")
    ));
    let output_events = output.snapshot().await;
    assert!(output_events.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { text, .. }
            if text == "guarded session.append"
    )));
    let transcript = std::fs::read_to_string(jsonl_path).expect("read JSONL");
    assert!(transcript.contains("injected accepted"));

    let stale_message = ConversationMessage::user(MessageId::new(), "injected original".into());
    let stale_root = lingxi_core::host::CancellationToken::new();
    stale_root.cancel();
    let stale_guard: Arc<dyn HookPublicationGuard> = Arc::new(
        crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
            stale_root,
            Arc::new(tokio::sync::Mutex::new(())),
        ),
    );
    assert!(
        !orch
            .append_guarded_injected_message(&stale_message, ToolUseId::new(), stale_guard)
            .await
    );
    assert!(
        !orch
            .session
            .lock()
            .await
            .history
            .iter()
            .any(|row| row.id() == stale_message.id())
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0].0.to_json_string().unwrap(),
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
        queued[0].0.value["blockingError"]["blockingError"],
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0].0.to_json_string().unwrap(),
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
        .map(|(projection, _)| projection.value["type"].as_str().unwrap_or_default())
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
    let msg = ConversationMessage::User { api_message_override: None,
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0]
            .0
            .value
            .get("type")
            .and_then(serde_json::Value::as_str),
        Some("hook_error_during_execution")
    );
    assert!(
        queued[0]
            .0
            .value
            .get("content")
            .and_then(serde_json::Value::as_str)
            .expect("string content")
            .contains("does not match Echo's output shape")
    );
}

/// Build an orchestrator whose only hook is a PreToolUse hook returning
/// `response`, optionally with a JSONL writer so persisted attachment
/// LINES (not just queued payloads) can be inspected.
fn orch_with_pre_hook(
    response: HookResponse,
    jsonl: Option<&std::path::Path>,
) -> ConversationOrchestrator {
    orch_with_pre_hook_and_output(response, jsonl, Arc::new(MockOutputStream::new()), None).0
}

fn orch_with_pre_hook_and_output(
    response: HookResponse,
    jsonl: Option<&std::path::Path>,
    output: Arc<dyn lingxi_core::host::OutputStream>,
    hook_started: Option<Arc<tokio::sync::Notify>>,
) -> (
    ConversationOrchestrator,
    Arc<tokio::sync::RwLock<HookRegistry>>,
) {
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
    exec.register_builtin(Arc::new(FixedPostHook {
        response,
        started: hook_started,
    }));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(EchoTool) as Arc<dyn Tool>);
    let mod_registry = Arc::new(tokio::sync::RwLock::new(HookRegistry::new()));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        Arc::new(exec),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(crate::test_support::StaticMemoryProvider::empty()),
        jsonl.map_or_else(
            || PathBuf::from("/tmp"),
            |p| p.parent().expect("parent").to_path_buf(),
        ),
    );
    let orch = match jsonl {
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
    };
    (orch, mod_registry)
}

struct BlockingAssistantAppendLog {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    observed: Arc<tokio::sync::Mutex<Vec<String>>>,
}

#[async_trait]
impl lingxi_core::host::OutputStream for BlockingAssistantAppendLog {
    async fn emit_text(&self, _text: &str, _utf16_code_units: Option<&[u16]>) {}

    async fn emit_tool_call(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _input: &serde_json::Value,
     _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {}

    async fn emit_tool_result(
        &self,
        _id: &lingxi_core::types::ToolUseId,
        _tool: &str,
        _model_text: &str,
        _result: &serde_json::Value,
     _projection: Option<&lingxi_core::host::ToolResultProjection>) {}

    async fn emit_end_turn(
        &self,
        _stop_reason: &str,
        _cost: &lingxi_core::host::CostSnapshot,
    ) {}

    async fn emit_mod_log(&self, _plugin: &str, text: &str) {
        if text.starts_with("assistant-row-uuid:") {
            self.observed.lock().await.push(text.to_owned());
            self.started.notify_one();
            self.release.notified().await;
        }
    }
}

/// Exercise the actual streaming W1 dispatcher while an assistant's
/// `session.append` Mod is suspended in `$.log`. A PreToolUse defer writes its
/// attachment through the same generation guard before the assistant's short
/// durable commit chooses its parent.
#[tokio::test]
async fn streaming_w1_defer_commits_between_session_append_and_assistant_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let module = dir.path().join("session-append.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('session.append', async ($, event, next) => {
            if (event.message.type === 'assistant') {
              $.ui.log(`assistant-row-uuid:${event.uuid}`);
            }
            return next(event);
          });
        }
        "#,
    )
    .expect("write session.append Mod fixture");
    let mod_host = hooks::mods::ModHost::start(None)
        .await
        .expect("start Mod host");
    mod_host
        .load(
            "interleaved-append-log",
            dir.path(),
            &module,
            serde_json::json!({}),
        )
        .await
        .expect("load session.append Mod");

    let output = Arc::new(BlockingAssistantAppendLog {
        started: Arc::new(tokio::sync::Notify::new()),
        release: Arc::new(tokio::sync::Notify::new()),
        observed: Arc::new(tokio::sync::Mutex::new(Vec::new())),
    });
    let pre_tool_started = Arc::new(tokio::sync::Notify::new());
    let (orch, mod_registry) = orch_with_pre_hook_and_output(
        HookResponse {
            decision: Some(hooks::response::HookDecision::Defer),
            ..HookResponse::default()
        },
        Some(&session_path),
        output.clone(),
        Some(pre_tool_started.clone()),
    );
    let orch = ConversationOrchestrator::into_shared(orch.with_hook_registry(mod_registry.clone()));
    mod_registry.write().await.set_mod_host(mod_host);

    let prompt = ConversationMessage::user(MessageId::new(), "seed chain".into());
    orch.persist_message_to_jsonl(&prompt).await;
    let (generation_root, publication_lock) = orch
        .lifecycle_runtime
        .session_tool_hook_generation
        .current();
    let fence = crate::autonomous_tool_scheduler::ToolDispatchPublicationFence::new(
        generation_root,
        publication_lock,
    );
    let tool_use_id = ToolUseId::from("toolu_w1_interleaved");
    let assistant = ConversationMessage::Assistant { per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::ToolUse { input_projection: None,
            id: tool_use_id.clone(),
            name: "Echo".into(),
            input: json!({}),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
    };
    orch.session.lock().await.history.push(assistant.clone());

    let assistant_orch = Arc::clone(&orch);
    let assistant_fence = fence.clone();
    let assistant_for_persist = assistant.clone();
    let mut assistant_write = tokio::spawn(async move {
        assistant_orch
            .persist_assistant_per_block(&assistant_for_persist, None, None, Some(assistant_fence))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), output.started.notified())
        .await
        .expect("session.append reached the guarded log sink");

    let tool_use = (tool_use_id.clone(), "Echo".to_string(), json!({}), None);
    let facts = crate::turn_loop::ToolUseDispatchFacts {
        query_history: vec![prompt.clone()],
        assistant_message: assistant.clone(),
        same_turn_tool_uses: Vec::new(),
    };
    let prepared_tool = orch
        .tools
        .find_registered("Echo")
        .expect("registered W1 tool");
    let inherited_context =
        crate::turn_loop::streaming_tool_context_base(&orch, vec![prompt]).await;
    let (dispatch_started_tx, dispatch_started_rx) = tokio::sync::oneshot::channel();
    let w1_orch = Arc::clone(&orch);
    let w1_fence = fence.clone();
    let assistant_id = assistant.id();
    let mut w1_dispatch = tokio::spawn(async move {
        crate::turn_loop::dispatch_streaming_tool_use_owned(
            &w1_orch,
            &tool_use,
            None,
            assistant_id,
            facts,
            prepared_tool,
            Some(inherited_context),
            Some(dispatch_started_tx),
            w1_fence,
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), dispatch_started_rx)
        .await
        .expect("streaming W1 dispatcher admitted")
        .expect("W1 start signal");
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        pre_tool_started.notified(),
    )
    .await
    .expect("actual PreToolUse defer hook started");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(25), &mut w1_dispatch)
            .await
            .is_err(),
        "W1's immediate attachment append waits for the session.append log lease"
    );

    output.release.notify_one();
    let deferred = tokio::time::timeout(std::time::Duration::from_secs(5), &mut w1_dispatch)
        .await
        .expect("W1 defer finishes after log release")
        .expect("W1 dispatcher task")
        .expect("W1 dispatch");
    assert!(
        !deferred.results.iter().any(|block| matches!(
            block,
            ContentBlock::ToolResult { content, .. }
                if content.contains("does not match its output shape")
        )),
        "a deferred core run's null placeholder must not be validated as an Echo output: {:?}",
        deferred.results
    );
    assert!(deferred.prevent_continuation);
    assert!(
        deferred.results.is_empty(),
        "a deferred tool has no tool_result"
    );
    let tool_parents =
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut assistant_write)
            .await
            .expect("assistant commit follows the queued W1 attachment")
            .expect("assistant persistence task");

    let rows = std::fs::read_to_string(&session_path)
        .expect("read JSONL")
        .lines()
        .map(|line| serde_json::from_str::<session::jsonl::schema::JsonlMessage>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].message_type, "attachment");
    assert_eq!(rows[1].extra["attachment"]["type"], "hook_deferred_tool");
    assert_eq!(rows[2].message_type, "assistant");
    assert_eq!(rows[2].parent_uuid.as_deref(), Some(rows[1].uuid.as_str()));
    assert_eq!(
        output.observed.lock().await.as_slice(),
        [format!("assistant-row-uuid:{}", rows[2].uuid)],
        "session.append uses the assistant line's stable UUID"
    );
    assert_eq!(tool_parents.get(&tool_use_id), Some(&rows[2].uuid));
    assert_eq!(
        orch.source_tool_assistant_uuid(&tool_use_id)
            .await
            .as_deref(),
        Some(rows[2].uuid.as_str())
    );
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0].0.to_json_string().unwrap(),
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
        started: None,
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
    assert_eq!(
        queued.len(),
        1,
        "one attachment, got {:?}",
        queued
            .iter()
            .map(|(projection, _)| &projection.value)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        queued[0]
            .0
            .value
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
