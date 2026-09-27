//! `fire_session_end` helper, the seam the host composition root
//! (`apps/cli` repl loop) calls once at session teardown.
//!
//! Byte-faithful to claude-code: a session that ends fires the `SessionEnd`
//! hook event at shutdown (`utils/hooks.ts:4097` `executeSessionEndHooks`,
//! driven from `gracefulShutdown`) with `reason` = one of the `ExitReason`
//! values (`coreTypes.ts:55` `EXIT_REASONS`: `clear` / `resume` / `logout` /
//! `prompt_input_exit` / `other` / `bypass_permissions_disabled`). The CLI's
//! clean exit paths (Ctrl+D / `/exit` / double-Ctrl+C) all funnel through
//! claude-code's `handleExit` → `gracefulShutdown(0, "prompt_input_exit")`, so
//! the CLI fires `reason = "prompt_input_exit"`. Like the other lifecycle
//! arms, firing is best-effort: a `SessionEnd` hook that itself fails must NOT
//! break the call (a failing shutdown hook never breaks shutdown).
//!
//! Scenarios:
//! 1. `fire_session_end("prompt_input_exit")` dispatches the `SessionEnd` event
//!    to a registered hook, carrying the exact `reason` for the exit path.
//! 2. Each clean-exit reason (`prompt_input_exit`, ...) round-trips verbatim.
//! 3. A `SessionEnd` hook that itself returns an error outcome does NOT panic /
//!    break the (best-effort) fire helper.
//! 4. No `SessionEnd` hook registered ⇒ firing is a strict no-op (nothing
//!    observed), so a session with no session-lifecycle hooks is unaffected.
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use hooks::HookExecutorImpl;
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use protocol::{HookId, HttpRequest, HttpResponse};
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

/// Records the `reason` of every `SessionEnd` event it sees so the test can
/// assert exactly which one fired (a pass-through observer — no decision).
struct RecordingHandler {
    log: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-session-end"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::SessionEnd { reason, .. } = event {
            self.log.lock().unwrap().push(reason.clone());
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

/// A `SessionEnd` hook that itself FAILS (non-success outcome). Proves the fire
/// helper is best-effort — a broken shutdown hook must not break teardown.
struct FailingHandler;
#[async_trait]
impl BuiltinHookHandler for FailingHandler {
    fn id(&self) -> &str {
        "broken-session-end"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the session-end hook itself blew up".into(),
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

fn orch_with(hooks: Arc<HookExecutorImpl>) -> ConversationOrchestrator {
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

#[tokio::test]
async fn fire_session_end_dispatches_session_end_with_reason() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-session-end",
        HookEventType::SessionEnd,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    // The CLI session-end seam: a clean user-initiated exit ⇒ `prompt_input_exit`.
    orch.fire_session_end("prompt_input_exit").await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["prompt_input_exit".to_string()],
        "fire_session_end must dispatch exactly one SessionEnd with the exit reason: {seen:?}"
    );
}

#[tokio::test]
async fn fire_session_end_round_trips_each_clean_exit_reason() {
    // The three clean REPL exit paths (Ctrl+D, /exit, double-Ctrl+C) all map to
    // `prompt_input_exit`; the defensive default maps to `other`. Each reason
    // string must reach the hook verbatim (the `reason` field is a free String).
    for reason in ["prompt_input_exit", "other"] {
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        registry.write().await.register(builtin_hook(
            "record-session-end",
            HookEventType::SessionEnd,
        ));
        let mut exec =
            HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
        let orch = orch_with(Arc::new(exec));

        orch.fire_session_end(reason).await;

        let seen = log.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![reason.to_string()],
            "fire_session_end must carry reason `{reason}` verbatim: {seen:?}"
        );
    }
}

#[tokio::test]
async fn failing_session_end_hook_does_not_break_fire() {
    // The registered SessionEnd hook itself returns a non-success outcome.
    // `fire_session_end` discards the aggregate, so the call must STILL return
    // cleanly (best-effort: a failing SessionEnd hook never breaks shutdown).
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-session-end",
        HookEventType::SessionEnd,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingHandler));
    let orch = orch_with(Arc::new(exec));

    // Must not panic / hang — a broken session-lifecycle hook is swallowed.
    orch.fire_session_end("prompt_input_exit").await;
}

#[tokio::test]
async fn fire_session_end_is_noop_without_a_registered_hook() {
    // No SessionEnd hook registered: firing observes nothing (strict no-op),
    // so a session with no session-lifecycle hooks is wholly unaffected.
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    orch.fire_session_end("prompt_input_exit").await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no SessionEnd hook registered ⇒ firing must be a strict no-op"
    );
}
