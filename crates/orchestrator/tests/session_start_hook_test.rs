//! `fire_session_start` helper, the seam the host composition root
//! (`engine-desktop` / `engine-mobile`) calls once at session startup.
//!
//! Byte-faithful to claude-code: a fresh session fires the `SessionStart` hook
//! event at startup (`utils/hooks.ts:3876-3881`, the `SessionStart` path) with
//! `source` = one of `startup` / `resume` / `clear` / `compact`. The desktop
//! composition root assembles exactly one fresh session per `build()` and so
//! fires `source = "startup"`. Like the other lifecycle arms, firing is
//! best-effort: a `SessionStart` hook that itself fails must NOT break the call.
//!
//! Scenarios:
//! 1. `fire_session_start("startup")` dispatches the `SessionStart` event to a
//!    registered hook, carrying `source = "startup"`.
//! 2. A `SessionStart` hook that itself returns an error outcome does NOT panic
//!    / break the (best-effort) fire helper.
//! 3. No `SessionStart` hook registered ⇒ firing is a strict no-op (nothing
//!    observed), so a session with no session-lifecycle hooks is unaffected.

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
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

/// Records the `source` of every `SessionStart` event it sees so the test can
/// assert exactly which one fired (a pass-through observer — no decision).
struct RecordingHandler {
    log: Arc<Mutex<Vec<String>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-session-start"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::SessionStart { source, .. } = event {
            self.log.lock().unwrap().push(source.clone());
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

/// A `SessionStart` hook that itself FAILS (non-success outcome). Proves the
/// fire helper is best-effort — a broken session-lifecycle hook must not break
/// the call.
struct FailingHandler;
#[async_trait]
impl BuiltinHookHandler for FailingHandler {
    fn id(&self) -> &str {
        "broken-session-start"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the session-start hook itself blew up".into(),
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
async fn fire_session_start_dispatches_session_start_with_source_startup() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-session-start",
        HookEventType::SessionStart,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    // The host composition root's startup seam: one fresh session ⇒ `"startup"`.
    orch.fire_session_start("startup").await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec!["startup".to_string()],
        "fire_session_start must dispatch exactly one SessionStart with source=startup: {seen:?}"
    );
}

#[tokio::test]
async fn failing_session_start_hook_does_not_break_fire() {
    // The registered SessionStart hook itself returns a non-success outcome.
    // `fire_session_start` discards the aggregate, so the call must STILL
    // return cleanly (best-effort, identical to the other lifecycle arms).
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-session-start",
        HookEventType::SessionStart,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingHandler));
    let orch = orch_with(Arc::new(exec));

    // Must not panic / hang — a broken session-lifecycle hook is swallowed.
    orch.fire_session_start("startup").await;
}

#[tokio::test]
async fn fire_session_start_is_noop_without_a_registered_hook() {
    // No SessionStart hook registered: firing observes nothing (strict no-op),
    // so a session with no session-lifecycle hooks is wholly unaffected.
    let log = Arc::new(Mutex::new(Vec::<String>::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    orch.fire_session_start("startup").await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no SessionStart hook registered ⇒ firing must be a strict no-op"
    );
}
