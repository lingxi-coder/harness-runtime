//! `MessageDisplay` hook firing (#39, R-O1).
//!
//! claude-code fires `MessageDisplay` at the BEGIN of each assistant-message
//! stream — its display state machine `begin(d)` (BIN off 208862320) checks the
//! `MessageDisplay` gate and initializes the per-message flush state
//! `o={apiMessageId:d, messageId:randomUUID(), turnId:r, index:0, …}`, and the
//! payload builder `aAt` (BIN off 205705090) yields
//! `{hook_event_name:"MessageDisplay", turn_id:e.turnId, message_id:e.messageId,
//! index:e.index, final:e.final, delta:e.delta}`.
//!
//! LingXi fires it ONCE per assistant message at its stream begin (the
//! orchestrator-owned point that holds the hook executor + the pre-allocated
//! assistant id), with `index:0`, `final:false`, an empty `delta` (no delta has
//! streamed yet at begin), a fresh per-turn `turn_id`, and the assistant
//! message's bare UUID as `message_id`.
//!
//! Scenarios:
//! 1. A registered `MessageDisplay` hook FIRES exactly once for a single
//!    assistant-message turn, carrying a non-empty `turn_id`, a valid-UUID
//!    `message_id`, `index == 0`, `final == false`, and an empty `delta`.
//! 2. No `MessageDisplay` hook registered ⇒ firing is a strict no-op (and the
//!    turn still completes normally).

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResponse, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tool_api::registry::ToolRegistry;

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

/// One captured `MessageDisplay` payload (the wire-relevant fields).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fired {
    turn_id: String,
    message_id: String,
    index: u64,
    is_final: bool,
    delta: String,
}

type FiredLog = Arc<Mutex<Vec<Fired>>>;

/// Records the payload of every `MessageDisplay` event it sees, and optionally
/// returns a `displayContent` override (or reports failure) so the
/// completed-message pass can be exercised.
struct RecordingHandler {
    log: FiredLog,
    /// When `Some`, returned as `hookSpecificOutput.displayContent` — the
    /// completed-message pass substitutes it for the on-screen text.
    display: Option<String>,
    /// When `true`, the hook reports `HookOutcome::Error` and no response, so
    /// the completed pass falls back to the original text.
    fail: bool,
}

impl RecordingHandler {
    fn observer(log: FiredLog) -> Self {
        Self {
            log,
            display: None,
            fail: false,
        }
    }
}

#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-message-display"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::MessageDisplay {
            turn_id,
            message_id,
            index,
            is_final,
            delta,
        } = event
        {
            self.log.lock().unwrap().push(Fired {
                turn_id: turn_id.clone(),
                message_id: message_id.clone(),
                index: *index,
                is_final: *is_final,
                delta: delta.clone(),
            });
        }
        if self.fail {
            return HookResult {
                outcome: HookOutcome::Error,
                stdout: String::new(),
                stderr: "boom".into(),
                exit_code: Some(1),
                response: None,
            };
        }
        let response = self.display.clone().map(|d| HookResponse {
            display_content: Some(d),
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

fn single_text_turn() -> Vec<llm_runtime::HistoryEvent> {
    scripted![
        message_start("msg_01", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "hi"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

fn orch_with(
    hooks: Arc<HookExecutorImpl>,
    api: Arc<MockStreamingApiClient>,
) -> (Arc<ConversationOrchestrator>, Arc<MockOutputStream>) {
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        api,
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    ));
    (orch, output)
}

/// Build an executor whose single `MessageDisplay` handler is `handler`.
async fn exec_with(handler: RecordingHandler) -> Arc<HookExecutorImpl> {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-message-display",
        HookEventType::MessageDisplay,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(handler));
    Arc::new(exec)
}

/// (1) A registered `MessageDisplay` hook fires TWICE per assistant message:
/// once at the stream BEGIN (`final:false`, empty delta) and once for the
/// COMPLETED-message pass (`final:true`, the full joined text) — the twin of
/// claude-code's `begin(d)` init plus the `Qff` completed-message fire.
#[tokio::test]
async fn message_display_fires_at_begin_and_on_completed_message() {
    let log: FiredLog = Arc::new(Mutex::new(Vec::new()));
    let exec = exec_with(RecordingHandler::observer(log.clone())).await;

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![single_text_turn()]));
    let (orch, _out) = orch_with(exec, api);

    let outcome = orch
        .run_turn_streaming("say hi")
        .await
        .expect("turn must succeed");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { turn_count, .. } if turn_count == 1),
        "expected a single-turn EndTurn"
    );

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen.len(),
        2,
        "MessageDisplay must fire at begin AND on the completed message: {seen:?}"
    );

    // The at-begin fire.
    let begin = &seen[0];
    assert!(
        !begin.turn_id.is_empty() && uuid::Uuid::parse_str(&begin.turn_id).is_ok(),
        "turn_id must be a fresh UUID, got {:?}",
        begin.turn_id
    );
    assert!(
        uuid::Uuid::parse_str(&begin.message_id).is_ok(),
        "message_id must be a bare uuid, got {:?}",
        begin.message_id
    );
    assert_eq!(begin.index, 0, "at-begin index is 0");
    assert!(!begin.is_final, "at-begin final is false");
    assert_eq!(begin.delta, "", "no delta text has streamed yet at begin");

    // The completed-message pass carries the full joined assistant text.
    let done = &seen[1];
    assert_eq!(done.turn_id, begin.turn_id, "same per-turn id");
    assert!(
        uuid::Uuid::parse_str(&done.message_id).is_ok(),
        "completed-pass message_id is a fresh uuid, got {:?}",
        done.message_id
    );
    assert_eq!(done.index, 0, "completed-pass index is 0");
    assert!(done.is_final, "completed-pass final is true");
    assert_eq!(
        done.delta, "hi",
        "completed pass carries the full joined text"
    );
}

/// (2) No `MessageDisplay` hook registered ⇒ firing is a strict no-op (and the
/// live text streams normally).
#[tokio::test]
async fn message_display_noop_without_a_registered_hook() {
    let log: FiredLog = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    // No MessageDisplay hook registered.
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler::observer(log.clone())));

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![single_text_turn()]));
    let (orch, out) = orch_with(Arc::new(exec), api);

    orch.run_turn_streaming("say hi")
        .await
        .expect("turn must succeed");

    assert!(
        log.lock().unwrap().is_empty(),
        "no MessageDisplay hook registered ⇒ firing must be a strict no-op"
    );
    // Live streaming is unchanged: the token is emitted as a raw delta.
    assert_eq!(
        out.text_events().await,
        vec!["hi".to_string()],
        "no display hook ⇒ live per-token text emission is byte-identical"
    );
}

/// (3) A `MessageDisplay` hook returning `displayContent` substitutes the
/// ON-SCREEN text for the completed message: the live per-token deltas are
/// suppressed and the hook's value is rendered instead, while the hook still
/// SEES the original joined text as the completed-pass delta (proving the
/// stored/source text is unchanged — "Display-only").
#[tokio::test]
async fn message_display_display_content_substitutes_on_screen_text() {
    let log: FiredLog = Arc::new(Mutex::new(Vec::new()));
    let exec = exec_with(RecordingHandler {
        log: log.clone(),
        display: Some("[redacted]".into()),
        fail: false,
    })
    .await;

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![single_text_turn()]));
    let (orch, out) = orch_with(exec, api);

    orch.run_turn_streaming("say hi")
        .await
        .expect("turn must succeed");

    // Only the substituted text reaches the screen — the raw "hi" delta was
    // suppressed because a display hook is active.
    assert_eq!(
        out.text_events().await,
        vec!["[redacted]".to_string()],
        "displayContent replaces the on-screen text; the raw delta is suppressed"
    );

    // The hook still observed the ORIGINAL text on the completed-message pass,
    // confirming the stored/source content is untouched.
    let seen = log.lock().unwrap().clone();
    let done = seen.last().expect("completed pass fired");
    assert!(done.is_final, "last fire is the completed-message pass");
    assert_eq!(
        done.delta, "hi",
        "the hook receives the ORIGINAL joined text, not the override"
    );
}

/// (4) A FAILING `MessageDisplay` hook falls back to the original text: no
/// override is applied and the original joined text is rendered on-screen.
#[tokio::test]
async fn message_display_failing_hook_falls_back_to_original_text() {
    let log: FiredLog = Arc::new(Mutex::new(Vec::new()));
    let exec = exec_with(RecordingHandler {
        log: log.clone(),
        display: None,
        fail: true,
    })
    .await;

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![single_text_turn()]));
    let (orch, out) = orch_with(exec, api);

    orch.run_turn_streaming("say hi")
        .await
        .expect("turn must succeed");

    assert_eq!(
        out.text_events().await,
        vec!["hi".to_string()],
        "a failing display hook falls back to the original joined text"
    );
}
