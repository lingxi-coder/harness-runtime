//! `fire_config_change` helper, the seam the desktop composition root's
//! settings watcher (`harness_runtime::desktop::settings_watch`) calls on every detected
//! settings-file change.
//!
//! Byte-faithful to claude-code: the settings watcher
//! (`utils/settings/changeDetector.ts:285-297`) fires the `ConfigChange` hook
//! (`executeConfigChangeHooks`, `utils/hooks.ts:4214`) with the layer `source`
//! (`user_settings` / `project_settings` / `local_settings` / `policy_settings`
//! / `skills`) and the changed `file_path` BEFORE applying the change. Like the
//! other lifecycle arms, firing is best-effort: a `ConfigChange` hook that
//! itself fails (or blocks) must NOT break the watch loop.
//!
//! Scenarios:
//! 1. `fire_config_change(source, file_path)` dispatches the `ConfigChange`
//!    event to a registered hook, carrying the exact `source` + `file_path`.
//! 2. A `ConfigChange` hook that returns an error outcome does NOT panic / break
//!    the (best-effort) fire helper.
//! 3. No `ConfigChange` hook registered ⇒ firing is a strict no-op.
use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{ConfigChangeSource, HookEvent, HookEventType};
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

/// Captured `(source, file_path)` pairs from every observed `ConfigChange`.
type FiredLog = Arc<Mutex<Vec<(ConfigChangeSource, Option<PathBuf>)>>>;

/// Records the `(source, file_path)` of every `ConfigChange` event it sees so
/// the test can assert exactly which one fired (a pass-through observer).
struct RecordingHandler {
    log: FiredLog,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-config-change"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::ConfigChange { source, file_path } = event {
            self.log.lock().unwrap().push((*source, file_path.clone()));
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

/// A `ConfigChange` hook that itself FAILS. Proves the fire helper is
/// best-effort — a broken settings-lifecycle hook must not break the call.
struct FailingHandler;
#[async_trait]
impl BuiltinHookHandler for FailingHandler {
    fn id(&self) -> &str {
        "broken-config-change"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the config-change hook itself blew up".into(),
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
async fn fire_config_change_dispatches_with_source_and_file_path() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-config-change",
        HookEventType::ConfigChange,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    let path = PathBuf::from("/work/.lingxi/settings.local.json");
    orch.fire_config_change(ConfigChangeSource::LocalSettings, Some(path.clone()))
        .await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(ConfigChangeSource::LocalSettings, Some(path))],
        "fire_config_change must dispatch exactly one ConfigChange carrying the source + path: {seen:?}"
    );
}

#[tokio::test]
async fn fire_config_change_maps_each_source() {
    // Each settings layer fires with its own `source` discriminator.
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-config-change",
        HookEventType::ConfigChange,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    for src in [
        ConfigChangeSource::UserSettings,
        ConfigChangeSource::ProjectSettings,
        ConfigChangeSource::LocalSettings,
        ConfigChangeSource::PolicySettings,
        ConfigChangeSource::Skills,
    ] {
        orch.fire_config_change(src, None).await;
    }

    let seen: Vec<ConfigChangeSource> = log.lock().unwrap().iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seen,
        vec![
            ConfigChangeSource::UserSettings,
            ConfigChangeSource::ProjectSettings,
            ConfigChangeSource::LocalSettings,
            ConfigChangeSource::PolicySettings,
            ConfigChangeSource::Skills,
        ]
    );
}

#[tokio::test]
async fn failing_config_change_hook_does_not_break_fire() {
    // The registered ConfigChange hook itself returns a non-success outcome.
    // `fire_config_change` discards the aggregate, so the call must STILL return
    // cleanly (best-effort, identical to the other lifecycle arms).
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-config-change",
        HookEventType::ConfigChange,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingHandler));
    let orch = orch_with(Arc::new(exec));

    // Must not panic / hang — a broken settings-lifecycle hook is swallowed.
    orch.fire_config_change(ConfigChangeSource::UserSettings, None)
        .await;
}

#[tokio::test]
async fn fire_config_change_is_noop_without_a_registered_hook() {
    // No ConfigChange hook registered: firing observes nothing (strict no-op).
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(Arc::new(exec));

    orch.fire_config_change(
        ConfigChangeSource::ProjectSettings,
        Some(PathBuf::from("/work/.lingxi/settings.json")),
    )
    .await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no ConfigChange hook registered ⇒ firing must be a strict no-op"
    );
}
