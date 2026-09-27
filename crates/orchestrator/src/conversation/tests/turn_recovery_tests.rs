use super::*;
use crate::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};

#[test]
fn compact_focus_and_hook_instructions_are_separate_paragraphs() {
    assert_eq!(
        merge_compact_instructions(Some("focus on Rust"), Some("preserve test output")).as_deref(),
        Some("focus on Rust\n\npreserve test output")
    );
}

#[test]
fn sdk_compact_metadata_keys_are_camelized_recursively() {
    let metadata = camelize_json_keys(serde_json::json!({
        "trigger": "manual",
        "pre_tokens": 42,
        "preserved_segment": {
            "head_uuid": "head",
            "anchor_uuid": "anchor",
            "tail_uuid": "tail"
        },
        "pre_compact_discovered_tools": ["Read"]
    }));
    assert_eq!(metadata["trigger"], "manual");
    assert_eq!(metadata["preTokens"], 42);
    assert_eq!(metadata["preservedSegment"]["headUuid"], "head");
    assert_eq!(metadata["preservedSegment"]["anchorUuid"], "anchor");
    assert_eq!(metadata["preservedSegment"]["tailUuid"], "tail");
    assert_eq!(metadata["preCompactDiscoveredTools"][0], "Read");
}
use crate::OrchestratorConfig;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::HookEventType;
use hooks::executor::BuiltinHookHandler;
use hooks::registry::HookRegistry;
use hooks::response::{HookDecision, HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use llm_runtime::ContentBlock as LlmContentBlock;
use platform_api::{HttpError, HttpTransport, OutputEvent, RuntimeError, RuntimeSpawner};
use protocol::{HookId, HttpRequest, HttpResponse};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::sync::{Notify, RwLock};

async fn wait_for_prewarm_capture(
    api: &MockApiClient,
) -> Vec<crate::test_support::MockPrewarmCall> {
    for _ in 0..50 {
        let captured = api.captured_prewarm().await;
        if !captured.is_empty() {
            return captured;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    api.captured_prewarm().await
}

struct BlockingPrewarmApiClient {
    active: Arc<AtomicBool>,
    started: Notify,
}

impl BlockingPrewarmApiClient {
    fn new() -> Self {
        Self {
            active: Arc::new(AtomicBool::new(false)),
            started: Notify::new(),
        }
    }

    async fn wait_started(&self) {
        loop {
            let notified = self.started.notified();
            if self.active.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::timeout(Duration::from_secs(1), notified)
                .await
                .expect("startup prewarm should start");
        }
    }

    async fn wait_inactive(&self) {
        for _ in 0..50 {
            if !self.active.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("startup prewarm should have been aborted");
    }
}

struct ActivePrewarmGuard(Arc<AtomicBool>);

impl Drop for ActivePrewarmGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[async_trait]
impl OrchestratorApiClient for BlockingPrewarmApiClient {
    async fn messages_create(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _msgs: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<LlmResponse, LlmError> {
        Err(LlmError::Transport {
            message: "blocking prewarm api does not serve messages_create".into(),
        })
    }

    async fn prewarm_responses_websocket(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _messages: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<(), LlmError> {
        self.active.store(true, Ordering::SeqCst);
        self.started.notify_waiters();
        let _guard = ActivePrewarmGuard(self.active.clone());
        std::future::pending::<()>().await;
        Ok(())
    }
}

// ---- unused HTTP / Runtime stubs (Builtin hooks never touch them) ----
struct UnusedHttp;
#[async_trait]
impl HttpTransport for UnusedHttp {
    async fn request(&self, _r: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
    async fn stream_sse(
        &self,
        _r: HttpRequest,
    ) -> Result<platform_api::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest("unused".into()))
    }
}
struct UnusedRuntime;
#[async_trait]
impl RuntimeSpawner for UnusedRuntime {
    async fn spawn(
        &self,
        _n: &str,
        _t: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) -> Result<platform_api::BackgroundTaskHandle, RuntimeError> {
        Err(RuntimeError::Internal("unused".into()))
    }
    async fn sleep(&self, _d: Duration) {}
    async fn cancel(&self, _h: &platform_api::BackgroundTaskHandle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Records every `Stop` / `StopFailure` lifecycle event it sees as
/// `"Stop:<reason>"` / `"StopFailure:<error>"`. Pass-through (no decision).
struct RecordingLifecycleHandler {
    log: Arc<StdMutex<Vec<String>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingLifecycleHandler {
    fn id(&self) -> &str {
        "rec-lifecycle"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        match event {
            HookEvent::Stop { reason } => {
                self.log.lock().unwrap().push(format!("Stop:{reason}"));
            }
            HookEvent::StopFailure { error } => {
                self.log
                    .lock()
                    .unwrap()
                    .push(format!("StopFailure:{error}"));
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

/// `PreCompact` hook that ALWAYS blocks — exercises the compaction abort
/// (TS `xhe` sets `blockedBy` from the blocked result; `VJn` / proactive /
/// reactive all honor it by aborting the pass).
struct BlockingPreCompactHandler;
#[async_trait]
impl BuiltinHookHandler for BlockingPreCompactHandler {
    fn id(&self) -> &str {
        "block-precompact"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::PreCompact { .. }).then(|| HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("[guard] compaction not allowed".into()),
            ..Default::default()
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

struct InstructingPreCompactHandler;
#[async_trait]
impl BuiltinHookHandler for InstructingPreCompactHandler {
    fn id(&self) -> &str {
        "instruct-precompact"
    }

    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Success,
            stdout: "preserve the test evidence".into(),
            stderr: String::new(),
            exit_code: Some(0),
            response: None,
        }
    }
}

/// `SessionStart` hook that emits `hookSpecificOutput.additionalContext`
/// (`Some`) or nothing (`None`) — exercises the SESSIONSTART.CTX consumption.
struct SessionStartCtxHandler {
    ctx: Option<String>,
}
#[async_trait]
impl BuiltinHookHandler for SessionStartCtxHandler {
    fn id(&self) -> &str {
        "sess-ctx"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::SessionStart { .. }).then(|| HookResponse {
            additional_context: self.ctx.clone(),
            ..Default::default()
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

async fn exec_session_start_ctx(ctx: Option<String>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook("sess-ctx", HookEventType::SessionStart));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(SessionStartCtxHandler { ctx }));
    Arc::new(exec)
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

async fn exec_recording(
    log: Arc<StdMutex<Vec<String>>>,
    events: &[HookEventType],
) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    {
        let mut r = registry.write().await;
        for ev in events {
            r.register(builtin_hook("rec-lifecycle", ev.clone()));
        }
    }
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingLifecycleHandler { log }));
    Arc::new(exec)
}

async fn exec_blocking_pre_compact() -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook("block-precompact", HookEventType::PreCompact));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(BlockingPreCompactHandler));
    Arc::new(exec)
}

async fn exec_instructing_pre_compact() -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "instruct-precompact",
        HookEventType::PreCompact,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(InstructingPreCompactHandler));
    Arc::new(exec)
}

fn compact_orch(hooks: Arc<HookExecutorImpl>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

// A blocking PreCompact hook aborts compaction in every route (TS `xhe` /
// `VJn`): `fire_pre_compact` surfaces the block detail so the caller can
// throw ("Compaction blocked by PreCompact hook: …") / log + skip.
#[tokio::test]
async fn pre_compact_block_returns_detail_else_none() {
    let blocked = compact_orch(exec_blocking_pre_compact().await)
        .fire_pre_compact("manual", None)
        .await;
    assert_eq!(
        blocked.blocked_by.as_deref(),
        Some("[guard] compaction not allowed"),
        "a blocking PreCompact hook must surface its blockedBy detail"
    );

    // No PreCompact hook registered → None → compaction proceeds unchanged.
    let proceed = compact_orch(noop_hook_executor())
        .fire_pre_compact("auto", None)
        .await;
    assert_eq!(proceed.blocked_by, None, "no block → compaction proceeds");
}

#[tokio::test]
async fn pre_compact_success_stdout_becomes_summary_instructions() {
    let outcome = compact_orch(exec_instructing_pre_compact().await)
        .fire_pre_compact("manual", Some("focus on Rust"))
        .await;
    assert_eq!(outcome.blocked_by, None);
    assert_eq!(
        outcome.additional_instructions.as_deref(),
        Some("preserve the test evidence")
    );
}

/// A Stop hook that blocks EXACTLY ONCE, then passes. Used by tests that need
/// precisely one stop-hook continuation, isolated from the consecutive-block
/// CAP (`LINGXI_STOP_HOOK_BLOCK_CAP`, default 8): a block-every-time hook
/// would now drive up to 8 continuations, so a test asserting a single
/// continuation must bound the blocking deterministically.
struct BlockOnceStopHandler {
    blocked: std::sync::atomic::AtomicBool,
}
#[async_trait]
impl BuiltinHookHandler for BlockOnceStopHandler {
    fn id(&self) -> &str {
        "block-stop"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let first = matches!(event, HookEvent::Stop { .. })
            && !self.blocked.swap(true, std::sync::atomic::Ordering::SeqCst);
        let response = first.then(|| HookResponse {
            decision: Some(HookDecision::Block),
            reason: Some("keep going".into()),
            system_message: Some("[stop-hook] please continue".into()),
            ..Default::default()
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

async fn exec_block_once_stop() -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook("block-stop", HookEventType::Stop));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(BlockOnceStopHandler {
        blocked: std::sync::atomic::AtomicBool::new(false),
    }));
    Arc::new(exec)
}

/// A Stop hook that requests `continue:false` (preventContinuation) with a
/// fixed `stopReason` — terminates the agent loop (FIX C).
struct PreventStopHandler {
    reason: Option<String>,
}
#[async_trait]
impl BuiltinHookHandler for PreventStopHandler {
    fn id(&self) -> &str {
        "prevent-stop"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        let response = matches!(event, HookEvent::Stop { .. }).then(|| HookResponse {
            prevent_continuation: true,
            reason: self.reason.clone(),
            ..Default::default()
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

async fn exec_prevent_stop(reason: Option<String>) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry
        .write()
        .await
        .register(builtin_hook("prevent-stop", HookEventType::Stop));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(PreventStopHandler { reason }));
    Arc::new(exec)
}

/// Seed a history far past the hard blocking limit. The default model
/// (`claude-opus-4-8`) is natively 1M as of 2.1.198 (M1b), so the
/// blocking limit sits just under 1M tokens; 8M chars ≈ 2M tokens
/// (estimator is chars/4), comfortably over.
async fn seed_over_blocking_limit(orch: &ConversationOrchestrator) {
    let session = orch.session();
    let mut s = session.lock().await;
    s.history.push(ConversationMessage::user(
        MessageId::new(),
        "x".repeat(8_000_000),
    ));
}

struct RewakeResponses(std::sync::Mutex<Vec<String>>);

#[async_trait]
impl crate::prompt::async_hook_response::AsyncHookResponseProvider for RewakeResponses {
    async fn take_pending_responses(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[tokio::test]
async fn async_hook_rewake_runs_without_persisting_a_synthetic_user_prompt() {
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("rewake", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "continued"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_async_hook_responses(Arc::new(RewakeResponses(std::sync::Mutex::new(vec![
        "background verification finished".into(),
    ]))));

    let outcome = orch.run_async_hook_rewake().await.expect("rewake turn");
    assert_eq!(outcome, TurnOutcome::EndTurn);

    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 1);
    assert!(calls[0].messages.iter().any(|message| {
        message.is_meta()
            && message
                .text_content()
                .contains("background verification finished")
    }));
    assert!(
        calls[0]
            .messages
            .iter()
            .all(|message| message.is_meta() || !message.text_content().is_empty()),
        "the provider request must not contain a synthetic empty human prompt"
    );

    let history = orch.session.lock().await.history.clone();
    assert_eq!(history.len(), 1, "only the assistant response is durable");
    assert!(matches!(history[0], ConversationMessage::Assistant { .. }));
}

// -------- RECOV.1 — streaming blocking-limit preempt --------

#[tokio::test]
async fn recov1_streaming_blocking_limit_preempts_before_opening_stream() {
    // One valid end_turn turn is scripted; if the preempt regresses the
    // stream opens (captured_calls == 1) and the prompt-too-long text is
    // absent — both asserted against below.
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "should not be reached"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    seed_over_blocking_limit(&orch).await;

    let outcome = orch
        .run_turn_streaming("go")
        .await
        .expect("turn ends without a hard error");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );

    // The stream was NEVER opened — the preempt short-circuited the API call.
    assert!(
        streaming.captured_calls().await.is_empty(),
        "the blocking-limit preempt must NOT open the stream"
    );

    // The byte-exact prompt-too-long message + an EndTurn("blocking_limit").
    // The PROACTIVE blocking-limit preempt ends with the DISTINCT terminal
    // reason `blocking_limit` (the binary keeps it separate from the
    // reactive-exhausted `prompt_too_long`); the surfaced message text is
    // still the byte-exact "Prompt is too long".
    let events = output.snapshot().await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Text { text } if text == "Prompt is too long")),
        "byte-exact prompt-too-long message must be surfaced; events={events:#?}"
    );
    assert!(
        events.iter().any(
            |e| matches!(e, OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "blocking_limit")
        ),
        "the proactive preempt must end with stop_reason blocking_limit; events={events:#?}"
    );
}

#[tokio::test]
async fn recov1_streaming_blocking_limit_does_not_trigger_budget_continuation() {
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "should not be reached"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            enable_token_budget: true,
            token_budget: Some(500_000),
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    seed_over_blocking_limit(&orch).await;

    let outcome = orch
        .run_turn_streaming("go")
        .await
        .expect("turn ends without a hard error");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { turn_count: 1, .. }),
        "{outcome:?}"
    );
    assert!(
        streaming.captured_calls().await.is_empty(),
        "the blocking-limit preempt must still short-circuit before opening the stream"
    );
    let history = orch.session().lock().await.history.clone();
    assert!(
        !history.iter().any(|m| matches!(
            m,
            protocol::ConversationMessage::User { content, .. }
                if matches!(
                    content.first(),
                    Some(protocol::ContentBlock::Text { text }) if text.starts_with("Stopped at ")
                )
        )),
        "terminal API-error ends must not inject a budget-continuation nudge"
    );
    let events = output.snapshot().await;
    let end = events.iter().rev().find_map(|e| match e {
        OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.as_str()),
        _ => None,
    });
    assert_eq!(end, Some("blocking_limit"));
}

// -------- terminal stop-reason API errors (claude.ts:2266-2292) --------

#[tokio::test]
async fn terminal_model_context_window_exceeded_surfaces_api_error() {
    // `model_context_window_exceeded` has no recovery path, so it hits the
    // terminal arm directly and must surface claude-code's byte-locked
    // API-error message (`claude.ts:2279`) before ending the turn.
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "partial answer"),
        content_block_stop(0),
        message_delta_stop("model_context_window_exceeded"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn_streaming("go").await.expect("turn ends");

    let events = output.snapshot().await;
    assert!(
        events.iter().any(|e| matches!(e, OutputEvent::Text { text }
            if text == "API Error: The model has reached its context window limit.")),
        "byte-exact context-window-exceeded API error must be surfaced; events={events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::EndTurn { stop_reason, .. }
            if stop_reason == "model_context_window_exceeded")),
        "the turn must end with stop_reason model_context_window_exceeded; events={events:#?}"
    );
}

#[tokio::test]
async fn terminal_refusal_without_fallback_surfaces_safety_message() {
    // A `refusal` with no `refusalFallbackModel` configured hits the terminal
    // arm (the swap arm `continue`s only when a fallback is set), so it must
    // surface claude-code's byte-locked `U2e` refusal message — the model-label
    // branch (resolved via `marketing_name_for_model`), non-interactive suffix
    // (`interactive_permissions` defaults false).
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m", "claude-opus-4-8"),
        content_block_start_text(0),
        text_delta(0, "partial"),
        content_block_stop(0),
        message_delta_stop("refusal"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-8".to_string();
    assert!(
        cfg.refusal_fallback_model.is_none(),
        "default config must have no refusal fallback (else the swap arm runs)"
    );
    let orch = ConversationOrchestrator::new_with_streaming(
        cfg,
        Arc::new(MockApiClient::new(vec![])),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.run_turn_streaming("go").await.expect("turn ends");

    let events = output.snapshot().await;
    let expected = "API Error: Opus 4.8's safeguards flagged this message (https://www.anthropic.com/legal/aup). This sometimes happens with safe, normal conversations. LingXi can't respond to this request with Opus 4.8.\n\nTry rephrasing the request in a new session or change your model.\n\nLearn more: https://support.claude.com/en/articles/15363606";
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Text { text } if text == expected)),
        "byte-exact U2e refusal message must be surfaced; events={events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::EndTurn { stop_reason, .. }
            if stop_reason == "refusal")),
        "the turn must end with stop_reason refusal; events={events:#?}"
    );
}

// -------- RECOV.2 — StopFailure fires on an api-error turn-end --------

#[tokio::test]
async fn recov2_stop_failure_fires_on_api_error_end_and_stop_does_not() {
    // A history over the blocking limit ⇒ the batched proactive preempt ends
    // with terminal reason `blocking_limit`, which is an api-error end (the
    // surfaced message's api-error field is `invalid_request`). `StopFailure`
    // must fire (error == "invalid_request"); the normal `Stop` hooks must NOT.
    let log = Arc::new(StdMutex::new(Vec::<String>::new()));
    let hooks = exec_recording(
        log.clone(),
        &[HookEventType::Stop, HookEventType::StopFailure],
    )
    .await;
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        // Never called — the blocking-limit preempt fires before the API call.
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    seed_over_blocking_limit(&orch).await;

    orch.run_turn("go")
        .await
        .expect("turn ends without a hard error");

    let seen = log.lock().unwrap().clone();
    assert!(
        seen.iter().any(|s| s == "StopFailure:invalid_request"),
        "StopFailure must fire on the api-error end with error=invalid_request: {seen:?}"
    );
    assert!(
        !seen.iter().any(|s| s.starts_with("Stop:")),
        "the normal Stop hooks must NOT fire on an api-error end: {seen:?}"
    );
}

#[tokio::test]
async fn recov2_batched_blocking_limit_does_not_trigger_budget_continuation() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            enable_token_budget: true,
            token_budget: Some(500_000),
            ..OrchestratorConfig::default()
        },
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    seed_over_blocking_limit(&orch).await;

    let outcome = orch.run_turn("go").await.expect("turn ends cleanly");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { turn_count: 1, .. }),
        "{outcome:?}"
    );
    assert!(
        api.captured_msgs().await.is_empty(),
        "the blocking-limit preempt must short-circuit before any batched API call"
    );
    let history = orch.session().lock().await.history.clone();
    assert!(
        !history.iter().any(|m| matches!(
            m,
            protocol::ConversationMessage::User { content, .. }
                if matches!(
                    content.first(),
                    Some(protocol::ContentBlock::Text { text }) if text.starts_with("Stopped at ")
                )
        )),
        "terminal API-error ends must not inject a budget-continuation nudge"
    );
    let events = output.snapshot().await;
    let end = events.iter().rev().find_map(|e| match e {
        OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.as_str()),
        _ => None,
    });
    assert_eq!(end, Some("blocking_limit"));
}

#[tokio::test]
async fn startup_responses_websocket_prewarm_uses_current_model_profile_system_and_empty_history() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig {
            model: "gpt-5".to_string(),
            ..OrchestratorConfig::default()
        },
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    {
        let mut session = orch.session.lock().await;
        session.model_profile = Some("openai".to_string());
    }

    orch.spawn_startup_responses_websocket_prewarm();

    let captured = wait_for_prewarm_capture(&api).await;
    assert_eq!(captured.len(), 1);
    let call = &captured[0];
    assert_eq!(call.model, "gpt-5");
    assert_eq!(call.profile.as_deref(), Some("openai"));
    assert!(
        call.messages.is_empty(),
        "startup prewarm uses empty history"
    );
    assert!(
        call.system
            .as_deref()
            .is_some_and(|system| !system.is_empty()),
        "startup prewarm must use the assembled system prompt"
    );
}

#[tokio::test]
async fn system_prompt_model_identity_follows_switch_model() {
    // Regression (reported): /model switched the ROUTED model, but the <env>
    // identity line ("You are powered by the model named …") stayed frozen
    // at config.model, so a switched-to model (e.g. Fable 5) still saw — and
    // reported — the launch model's identity (Opus 4.8). The prompt identity
    // must track the LIVE session.model that switch_model updates.
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            model: "claude-opus-4-8".to_string(),
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let before = orch.build_system_prompt().await;
    assert!(
        before
            .contains("powered by the model named Opus 4.8. The exact model ID is claude-opus-4-8"),
        "launch identity present: {before}"
    );

    <ConversationOrchestrator as platform_api::OrchestratorHandle>::switch_model(
        &orch,
        "claude-fable-5-1",
        None,
    )
    .await
    .expect("switch_model");

    let after = orch.build_system_prompt().await;
    assert!(
        after.contains(
            "powered by the model named Fable 5.1. The exact model ID is claude-fable-5-1"
        ),
        "identity follows the switch: {after}"
    );
    // The stale identity LINE must be gone. (The static "most recent Claude
    // models … Opus 4.8" catalog sentence is model-independent and stays —
    // so assert on the identity line, not the bare "Opus 4.8" substring.)
    assert!(
        !after.contains("powered by the model named Opus 4.8"),
        "the stale identity line must be gone after switching: {after}"
    );
}

#[tokio::test]
async fn non_claude_switch_uses_the_named_identity_form_not_id_only() {
    // A switched-to NON-Claude model must still get the strong "powered by
    // the model named {name}" form each turn (via the catalog display name),
    // not the weak id-only "the model {id}." — so its current identity is
    // asserted clearly. (Also: no Claude-catalog contamination.)
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            model: "claude-opus-4-8".to_string(),
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    <ConversationOrchestrator as platform_api::OrchestratorHandle>::switch_model(
        &orch,
        "deepseek-v4-pro",
        Some("deepseek"),
    )
    .await
    .expect("switch_model");

    let sp = orch.build_system_prompt().await;
    assert!(
        sp.contains(" - You are powered by the model named ")
            && sp.contains("The exact model ID is deepseek-v4-pro."),
        "non-Claude model uses the named form with its exact id: {sp}"
    );
    assert!(
        !sp.contains(" - You are powered by the model deepseek-v4-pro."),
        "must NOT use the weak id-only fallback: {sp}"
    );
    // The prior fix: no Claude model-catalog line for a non-Claude model.
    assert!(
        !sp.contains("claude-fable-5-1"),
        "no Claude catalog contamination for a non-Claude model: {sp}"
    );
}

#[tokio::test]
async fn system_prompt_reflects_session_cwd_swap() {
    // Task 5 (worktree 206 session-cwd plumbing): `EnterWorktree`/
    // `ExitWorktree` swap the shared `tool_api::SessionCwd` cell the tool
    // layer resolves relative paths through. The NEXT system-prompt
    // render must show the SWAPPED directory's env-block
    // `Primary working directory:` line. The trailing gitStatus block is a
    // separate start-of-conversation snapshot and remains frozen.
    let boot_cwd = std::path::PathBuf::from("/tmp/lingxi-session-cwd-boot-fixture");
    let session_cwd = tool_api::SessionCwd::new(boot_cwd.clone(), vec![boot_cwd.clone()]);
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        boot_cwd.clone(),
    )
    .with_session_cwd(session_cwd.clone());

    let before = orch.build_system_prompt().await;
    assert!(
        before.contains(&format!(
            "Primary working directory: {}",
            boot_cwd.display()
        )),
        "boot cwd present before any swap: {before}"
    );

    let worktree_cwd = std::path::PathBuf::from("/tmp/lingxi-session-cwd-worktree-fixture");
    session_cwd.swap(worktree_cwd.clone(), vec![worktree_cwd.clone()]);

    let after = orch.build_system_prompt().await;
    assert!(
        after.contains(&format!(
            "Primary working directory: {}",
            worktree_cwd.display()
        )),
        "system prompt must reflect the swapped worktree cwd: {after}"
    );
    assert!(
        !after.contains(&format!(
            "Primary working directory: {}",
            boot_cwd.display()
        )),
        "the stale boot-cwd line must be gone after the swap: {after}"
    );
}

#[tokio::test]
async fn system_prompt_cwd_stays_at_boot_cwd_when_never_swapped() {
    // INERT INVARIANT: a caller that never calls `.with_session_cwd(...)`
    // gets byte-identical behavior to before Task 5 — the prompt always
    // shows the boot cwd handed to the constructor.
    let boot_cwd = std::path::PathBuf::from("/tmp/lingxi-session-cwd-inert-fixture");
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        boot_cwd.clone(),
    );

    let sp = orch.build_system_prompt().await;
    assert!(
        sp.contains(&format!(
            "Primary working directory: {}",
            boot_cwd.display()
        )),
        "no swap ⇒ boot cwd, exactly as before: {sp}"
    );
}

#[tokio::test]
async fn git_status_stays_frozen_after_session_cwd_swap() {
    fn init_repo(path: &std::path::Path, marker: &str) {
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .expect("git is available")
                .success()
        };
        assert!(git(&["init", "-q"]));
        std::fs::write(path.join(marker), marker).expect("seed marker");
    }

    let boot = tempfile::tempdir().expect("boot repo");
    let worktree = tempfile::tempdir().expect("worktree repo");
    init_repo(boot.path(), "boot-only.txt");
    init_repo(worktree.path(), "worktree-only.txt");

    let session_cwd = tool_api::SessionCwd::new(boot.path().to_path_buf(), Vec::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        boot.path().to_path_buf(),
    )
    .with_session_cwd(session_cwd.clone());

    let before = orch.build_system_prompt().await;
    assert!(before.contains("boot-only.txt"), "before:\n{before}");

    session_cwd.swap(worktree.path().to_path_buf(), Vec::new());
    assert_eq!(session_cwd.cwd(), worktree.path());
    let after = orch.build_system_prompt().await;
    assert!(after.contains("boot-only.txt"), "after:\n{after}");
    assert!(!after.contains("worktree-only.txt"), "after:\n{after}");
}

/// The PROMPT half of the interactive-session wiring.
///
/// Race-free and therefore safe to keep in the lib binary: the bullet is gated
/// on `prompt_is_interactive()`, which reads `self.config.interactive_session`
/// off THIS instance, not the process-global flag.
///
/// The other half — that composition also publishes the process-global
/// `session_flags` — cannot be asserted here. Every `ConversationOrchestrator`
/// construction in this binary stores `!config.interactive_session` into that
/// one global (`conversation/wiring.rs`), and ~130 other lib tests construct
/// one concurrently, so a read-back races and fails intermittently. It lives in
/// `tests/interactive_session_flag_test.rs`, which gets its own process.
#[tokio::test]
async fn interactive_session_flag_drives_prompt_guidance() {
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            interactive_permissions: false,
            interactive_session: true,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let prompt = orch.build_system_prompt().await;
    assert!(
        prompt.contains("If you need the user to run a shell command themselves"),
        "interactive CLI prompt guidance must follow the explicit interactive-session flag, \
         even with interactive_permissions=false: {prompt}"
    );
}

#[tokio::test]
async fn streaming_turn_aborts_pending_startup_prewarm_before_opening_stream() {
    let api = Arc::new(BlockingPrewarmApiClient::new());
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![vec![
        message_start("m", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let output = Arc::new(MockOutputStream::new());
    let orch = Arc::new(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        api.clone(),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    orch.spawn_startup_responses_websocket_prewarm();
    api.wait_started().await;

    let outcome = tokio::time::timeout(Duration::from_secs(1), orch.run_turn_streaming("go"))
        .await
        .expect("streaming turn must not wait for startup prewarm")
        .expect("streaming turn completes");

    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );
    api.wait_inactive().await;
    assert!(
        orch.lifecycle_runtime
            .startup_responses_websocket_prewarm
            .lock()
            .expect("startup responses websocket prewarm")
            .is_none(),
        "turn start must clear the pending startup prewarm handle"
    );
    assert_eq!(
        streaming.captured_calls().await.len(),
        1,
        "the real streaming turn should still open normally"
    );
}

#[tokio::test]
async fn clear_session_aborts_startup_prewarm_and_closes_responses_websocket_session() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let orch = Arc::new(ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    orch.transcript
        .post_compact_skill_attachments
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(MessageId::new(), vec!["skill body".to_string()]);
    orch.tools.deferral().mark_loaded(["StaleTool"]);
    orch.compaction_runtime
        .last_response_input_tokens
        .store(42, std::sync::atomic::Ordering::Relaxed);
    orch.compaction_runtime
        .output_token_pool
        .store(7, std::sync::atomic::Ordering::Relaxed);
    orch.model_runtime
        .last_api_call_at_ms
        .store(100, std::sync::atomic::Ordering::Relaxed);
    orch.prompt_runtime
        .sent_skill_names
        .lock()
        .await
        .insert("old-skill".into());
    tool_api::read_file_state::set(
        &orch.prompt_runtime.read_state_map,
        std::env::temp_dir().join("old-session-file"),
        tool_api::read_file_state::ReadFileEntry {
            content: "old".into(),
            mtime_ms: 0,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
    orch.spawn_startup_responses_websocket_prewarm();
    <ConversationOrchestrator as platform_api::OrchestratorHandle>::clear_session(&*orch)
        .await
        .expect("clear session");

    assert_eq!(api.close_responses_ws_count().await, 1);
    assert!(orch.tools.deferral().loaded_tool_names().is_empty());
    assert!(orch.prompt_runtime.sent_skill_names.lock().await.is_empty());
    assert!(orch
        .prompt_runtime
        .read_state_map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
    assert_eq!(
        orch.compaction_runtime
            .last_response_input_tokens
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        orch.compaction_runtime
            .output_token_pool
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    assert_eq!(
        orch.model_runtime
            .last_api_call_at_ms
            .load(std::sync::atomic::Ordering::Relaxed),
        -1
    );
    assert!(
        orch.transcript
            .post_compact_skill_attachments
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty(),
        "clearing a session must not leak post-compact attachment identity into the new session"
    );
}

// -------- SESSIONSTART.CTX — SessionStart additionalContext consumption ----

#[tokio::test]
async fn session_start_additional_context_becomes_persistent_meta_history_message() {
    // A `SessionStart` hook that emits `hookSpecificOutput.additionalContext`
    // must surface it as a persistent `hook_additional_context` meta message
    // in the conversation history (claude-code `processSessionStartHooks`,
    // `sessionStart.ts:163-172` → `messages.ts:4117-4128`), so it rides every
    // subsequent turn. The bytes are the exact `wrapInSystemReminder`
    // (`hookName` = `SessionStart`, multi-line content preserved).
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        exec_session_start_ctx(Some("Project: lingxi\nBranch: main".into())).await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.fire_session_start("startup").await;

    let history = orch.session().lock().await.history.clone();
    assert_eq!(
        history.len(),
        1,
        "exactly one hook_additional_context message; got {history:?}"
    );
    let body = match &history[0] {
        ConversationMessage::User { content, .. } => content
            .iter()
            .filter_map(|b| match b {
                protocol::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("expected a user meta message; got {other:?}"),
    };
    assert_eq!(
        body,
        "<system-reminder>\nSessionStart hook additional context: Project: lingxi\nBranch: main\n</system-reminder>",
        "exact hook_additional_context bytes (hookName=SessionStart, content joined by \\n)"
    );
}

#[tokio::test]
async fn session_start_without_additional_context_pushes_nothing() {
    // Strict no-op: a SessionStart hook that emits no additionalContext leaves
    // the history untouched (the aggregate is discarded exactly as before).
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        exec_session_start_ctx(None).await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    orch.fire_session_start("startup").await;

    assert!(
        orch.session().lock().await.history.is_empty(),
        "no additionalContext ⇒ nothing pushed to history"
    );
}

// -------- RECOV.4 — recovery budget reset on stop-hook continuation -----

#[tokio::test]
async fn recov4_stop_hook_continuation_resets_max_output_tokens_recovery() {
    // Script: max_tokens, max_tokens, end_turn, then max_tokens ×4.
    // With the reset on the stop-hook continuation, the post-continuation
    // episode gets a FRESH budget of MAX_OUTPUT_TOKENS_RECOVERY_LIMIT (3)
    // nudges, so the loop makes exactly 7 API calls and injects 5 nudges.
    // WITHOUT the reset the carried count (2) would exhaust after only 2
    // more calls (5 total, 3 nudges).
    let mt = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "partial".into(),
                cache_control: None,
            }],
            Some("max_tokens"),
        )
    };
    let et = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        )
    };
    let api = Arc::new(MockApiClient::new(vec![
        mt(),
        mt(),
        et(),
        mt(),
        mt(),
        mt(),
        mt(),
    ]));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        // Block-ONCE: this test isolates the recovery-reset on a SINGLE
        // stop-hook continuation. A block-every-time hook would now (post
        // #2 cap-counter) also block the final recovery-exhaustion end and
        // drive further continuations up to LINGXI_STOP_HOOK_BLOCK_CAP
        // (default 8), exhausting the scripted responses.
        exec_block_once_stop().await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("go").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        api.captured_msgs().await.len(),
        7,
        "the stop-hook continuation must reset the recovery budget (fresh 3 nudges ⇒ 7 API calls)"
    );
    let nudges = orch
        .session()
        .lock()
        .await
        .history
        .iter()
        .filter(|m| m.text_content() == MAX_OUTPUT_TOKENS_RECOVERY_NUDGE)
        .count();
    assert_eq!(
        nudges, 5,
        "5 recovery nudges expected across the two episodes (2 before + 3 after the reset)"
    );
}

// -------- FIX C — Stop hook_stopped_continuation meta message -----------

#[tokio::test]
async fn fix_c_stop_prevent_continuation_persists_stopped_message() {
    // Parity with claude-code `query/stopHooks.ts:269-280` — a Stop hook's
    // `continue:false` (preventContinuation) yields a
    // `hook_stopped_continuation` attachment (hookName `Stop`), rendered as
    // an isMeta `<system-reminder>\nStop hook stopped continuation:
    // {stopReason}\n</system-reminder>` user message before the turn
    // terminates. Script a single end_turn, then let the Stop hook prevent
    // continuation.
    let et = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![et])),
        Arc::new(ToolRegistry::new()),
        exec_prevent_stop(Some("STOP-CONTINUATION".into())).await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("go").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::StopHookPrevented { .. }),
        "continue:false must terminate as StopHookPrevented, got {outcome:?}"
    );

    // The exact meta message is appended to live history. The transcript
    // persists only its typed attachment; cold resume derives this message
    // from that single source of truth.
    let session = orch.session();
    let s = session.lock().await;
    let found = s.history.iter().any(|m| {
        m.text_content()
            == "<system-reminder>\nStop hook stopped continuation: STOP-CONTINUATION\n</system-reminder>"
    });
    assert!(
        found,
        "the Stop hook_stopped_continuation meta message must be in history: {:#?}",
        s.history
            .iter()
            .map(protocol::ConversationMessage::text_content)
            .collect::<Vec<_>>()
    );
}

/// O2: the Stop hook's `preventContinuation` also PERSISTS a
/// `hook_stopped_continuation` attachment line (BIN off 233101239), not
/// just the meta message. `message` sits SECOND in key order and the
/// `toolUseID` is the dispatch's `hook-{uuid}`, matching the
/// `hook_additional_context` record the same Stop dispatch emits.
#[tokio::test]
async fn stop_prevent_continuation_persists_a_stopped_continuation_attachment() {
    let et = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
        dir.path().to_path_buf(),
    ));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![et])),
        Arc::new(ToolRegistry::new()),
        exec_prevent_stop(Some("STOP-CONTINUATION".into())).await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_path_buf(),
    )
    .with_jsonl_writer(writer);

    orch.run_turn("go").await.expect("turn ok");

    let raw = std::fs::read_to_string(&path).expect("read jsonl");
    let line = raw
        .lines()
        .find(|l| l.contains("hook_stopped_continuation"))
        .unwrap_or_else(|| panic!("no hook_stopped_continuation attachment line in: {raw}"));
    let v: serde_json::Value = serde_json::from_str(line).expect("json line");
    assert_eq!(v["type"], "attachment");
    let a = &v["attachment"];
    // Key ORDER is the contract — serde_json is pinned preserve_order.
    let rendered = serde_json::to_string(a).expect("attachment json");
    let tool_use_id = a["toolUseID"].as_str().expect("toolUseID").to_string();
    assert_eq!(
        rendered,
        format!(
            r#"{{"type":"hook_stopped_continuation","message":"STOP-CONTINUATION","hookName":"Stop","toolUseID":"{tool_use_id}","hookEvent":"Stop"}}"#
        )
    );
    assert!(
        tool_use_id.starts_with("hook-"),
        "Stop mints `hook-${{randomUUID()}}`, got {tool_use_id}"
    );
    let duplicate_rows = raw
        .lines()
        .filter_map(|row| serde_json::from_str::<serde_json::Value>(row).ok())
        .filter(|row| {
            row["type"] == "user"
                && row["message"]["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("STOP-CONTINUATION"))
        })
        .count();
    assert_eq!(
        duplicate_rows, 0,
        "the normalized meta message must not be persisted beside its attachment: {raw}"
    );
}

#[tokio::test]
async fn fix_c_stop_prevent_continuation_default_reason() {
    // No `stopReason` → claude's default `'Stop hook prevented continuation'`
    // (`query/stopHooks.ts:271`).
    let et = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "done".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![et])),
        Arc::new(ToolRegistry::new()),
        exec_prevent_stop(None).await,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    orch.run_turn("go").await.expect("turn ok");

    let session = orch.session();
    let s = session.lock().await;
    let found = s.history.iter().any(|m| {
        m.text_content()
            == "<system-reminder>\nStop hook stopped continuation: Stop hook prevented continuation\n</system-reminder>"
    });
    assert!(found, "default stopReason must be used");
}

// ── §9: the rewake wrappers are verified on their own ────────────────────────
//
// §3.2 is explicit that they are not derived from the main streaming wrapper:
// "Rewake 等独立包装的预检查、结束事件及错误映射按其原实现单独验证，不从主
// streaming 包装推导."
//
// The reason shows up immediately. The main streaming entry's pre-cancel emits
// NOTHING — that is `a_pre_cancelled_streaming_turn_emits_no_end_event`.
// `run_task_notification_rewake`'s pre-check does the opposite: it EMITS
// `end_turn` before returning, because the host has already reserved its
// UI/permission lifecycle for this idle turn and something has to close it.
//
// Same shape of guard, opposite obligation. A unified entry wrapper that gave
// every pre-check the main entry's silence would leave the host's lifecycle
// open, and a unified one that gave every pre-cancel an event would break the
// main entry. Only a test per wrapper distinguishes them.

/// A registry with nothing pending — the default for every method.
struct EmptyTaskRegistry;

#[async_trait::async_trait]
impl platform_api::task_registry::TaskRegistryHandle for EmptyTaskRegistry {
    async fn create(
        &self,
        _input: platform_api::task_registry::TaskCreateInput,
    ) -> Result<
        platform_api::task_registry::TaskRecord,
        platform_api::task_registry::TaskRegistryError,
    > {
        unreachable!("the pre-check returns before any registry mutation")
    }
    async fn get(
        &self,
        _id: &str,
    ) -> Result<
        Option<platform_api::task_registry::TaskRecord>,
        platform_api::task_registry::TaskRegistryError,
    > {
        Ok(None)
    }
    async fn list(
        &self,
        _filter: platform_api::task_registry::TaskListFilter,
    ) -> Result<
        Vec<platform_api::task_registry::TaskRecord>,
        platform_api::task_registry::TaskRegistryError,
    > {
        Ok(Vec::new())
    }
    async fn update(
        &self,
        _id: &str,
        _patch: platform_api::task_registry::TaskUpdatePatch,
    ) -> Result<
        platform_api::task_registry::TaskRecord,
        platform_api::task_registry::TaskRegistryError,
    > {
        unreachable!("the pre-check returns before any registry mutation")
    }
    async fn set_status(
        &self,
        _id: &str,
        _status: &str,
    ) -> Result<
        platform_api::task_registry::TaskRecord,
        platform_api::task_registry::TaskRegistryError,
    > {
        unreachable!("the pre-check returns before any registry mutation")
    }
    async fn kill(
        &self,
        _id: &str,
    ) -> Result<
        platform_api::task_registry::TaskRecord,
        platform_api::task_registry::TaskRegistryError,
    > {
        unreachable!("the pre-check returns before any registry mutation")
    }
    async fn output(
        &self,
        _id: &str,
        _offset: Option<u64>,
    ) -> Result<
        platform_api::task_registry::TaskOutputChunk,
        platform_api::task_registry::TaskRegistryError,
    > {
        unreachable!("the pre-check returns before any registry mutation")
    }
}

async fn rewake_end_events(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .iter()
        .filter_map(|e| match e {
            platform_api::OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_task_notification_rewake_precheck_emits_its_end_turn() {
    // Cancelled: returns Cancelled AND still emits, unlike the main entry.
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let outcome = orch
        .run_task_notification_rewake(&EmptyTaskRegistry, cancel)
        .await
        .expect("rewake");
    assert_eq!(outcome, crate::conversation::TurnOutcome::Cancelled);
    assert_eq!(
        rewake_end_events(&output).await,
        vec!["end_turn".to_string()],
        "a pre-cancelled rewake must still close the host's reserved lifecycle with an \
         end_turn. The main streaming entry's pre-cancel emits nothing — the opposite \
         obligation, which is why §3.2 says not to derive one from the other."
    );

    // Nothing pending and not cancelled: EndTurn, and the same emit.
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let outcome = orch
        .run_task_notification_rewake(
            &EmptyTaskRegistry,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("rewake");
    assert_eq!(outcome, crate::conversation::TurnOutcome::EndTurn);
    assert_eq!(
        rewake_end_events(&output).await,
        vec!["end_turn".to_string()],
        "a rewake whose completion was already consumed at the turn gate closes the same way"
    );
}
