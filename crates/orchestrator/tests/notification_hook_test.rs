//! `fire_notification` helper, the seam the CLI repl's idle-prompt timer calls
//! once the REPL has been idle for `messageIdleNotifThresholdMs` after the last
//! response.
//!
//! Byte-faithful to claude-code: the idle watcher fires
//! `sendNotification({ message: "Claude is waiting for your input",
//! notificationType: "idle_prompt" })` (`screens/REPL.tsx:3934-3937`). The
//! helper feeds `message` → `NotificationPayload.message` and `notification_type`
//! → `HookEvent::Notification { kind }` → `NotificationPayload.notification_type`
//! (`hooks/executor.rs` Notification arm). Like the other lifecycle arms, firing
//! is best-effort: a `Notification` hook that itself fails (or blocks) must NOT
//! affect the caller (the repl input loop).
//!
//! Scenarios:
//! 1. `fire_notification(message, notification_type)` dispatches the
//!    `Notification` event to a registered hook carrying the exact byte-faithful
//!    message + `notification_type`.
//! 2. A `Notification` hook that returns an error outcome does NOT panic / break
//!    the (best-effort) fire helper.
//! 3. No `Notification` hook registered ⇒ firing is a strict no-op, AND
//!    `has_notification_hook()` reports `false` (the gate); registering one
//!    flips the gate to `true`.

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

/// Captured `(message, notification_type)` pairs from every observed
/// `Notification`.
type FiredLog = Arc<Mutex<Vec<(String, String)>>>;

/// Records the `(message, kind)` of every `Notification` event it sees so the
/// test can assert exactly which one fired (a pass-through observer).
struct RecordingHandler {
    log: FiredLog,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-notification"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::Notification { message, kind } = event {
            self.log
                .lock()
                .unwrap()
                .push((message.clone(), kind.clone()));
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

/// A `Notification` hook that itself FAILS. Proves the fire helper is
/// best-effort — a broken runtime-lifecycle hook must not break the call (and so
/// can never affect the repl input loop).
struct FailingHandler;
#[async_trait]
impl BuiltinHookHandler for FailingHandler {
    fn id(&self) -> &str {
        "broken-notification"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the notification hook itself blew up".into(),
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

/// (a) `fire_notification` dispatches a single `Notification` carrying the
/// byte-faithful idle-prompt message + `notification_type` ("idle_prompt").
#[tokio::test]
async fn fire_notification_dispatches_byte_faithful_idle_prompt() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-notification",
        HookEventType::Notification,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    orch.fire_notification("Claude is waiting for your input", "idle_prompt")
        .await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(
            "Claude is waiting for your input".to_string(),
            "idle_prompt".to_string()
        )],
        "fire_notification must dispatch exactly one Notification carrying the byte-faithful message + notification_type: {seen:?}"
    );
}

/// (b) A failing `Notification` hook is swallowed — best-effort, never breaks
/// the caller.
#[tokio::test]
async fn failing_notification_hook_does_not_break_fire() {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-notification",
        HookEventType::Notification,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingHandler));
    let orch = orch_with(Arc::new(exec));

    // Must not panic / hang — a broken runtime-lifecycle hook is swallowed.
    orch.fire_notification("Claude is waiting for your input", "idle_prompt")
        .await;
}

/// (c) No `Notification` hook registered ⇒ firing is a strict no-op AND the
/// `has_notification_hook` gate reports `false`; registering one flips it.
#[tokio::test]
async fn fire_notification_noop_and_gate_false_without_a_registered_hook() {
    // No Notification hook registered.
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    assert!(
        !orch.has_notification_hook().await,
        "gate must report false when no Notification hook is registered"
    );

    orch.fire_notification("Claude is waiting for your input", "idle_prompt")
        .await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no Notification hook registered ⇒ firing must be a strict no-op"
    );
}

/// The gate flips to `true` once a `Notification` hook is registered, but stays
/// `false` for an unrelated event type (proves it is event-type-specific).
#[tokio::test]
async fn has_notification_hook_gate_is_event_type_specific() {
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    // Register a non-Notification hook first: gate must stay false.
    registry
        .write()
        .await
        .register(builtin_hook("some-stop-hook", HookEventType::Stop));
    let exec = HookExecutorImpl::new(
        registry.clone(),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    );
    let orch = orch_with(Arc::new(exec));
    assert!(
        !orch.has_notification_hook().await,
        "a Stop hook must not satisfy the Notification gate"
    );

    // Now register a Notification hook: gate flips true.
    registry.write().await.register(builtin_hook(
        "record-notification",
        HookEventType::Notification,
    ));
    assert!(
        orch.has_notification_hook().await,
        "gate must report true once a Notification hook is registered"
    );
}
