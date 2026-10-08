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
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
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

struct TestModSettingsReader;

#[async_trait]
impl hooks::mods::ModSettingsReader for TestModSettingsReader {
    async fn read(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, hooks::mods::ModError> {
        assert_eq!(input, serde_json::json!({"source":"policy"}));
        Ok(serde_json::json!({"custom":"managed"}))
    }
}

struct ListedTool {
    schema: serde_json::Value,
}

#[async_trait]
impl Tool for ListedTool {
    fn name(&self) -> &str {
        "Inspector"
    }

    fn input_schema(&self) -> &serde_json::Value {
        &self.schema
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
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
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
        "Inspect the current repository.".into()
    }

    async fn prompt(&self, _opts: &PromptOptions) -> String {
        "Inspect the current repository.".into()
    }

    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Ok(ToolCallResult { mcp_meta_projection: None, model_content_projection: None, data_projection: None,
            data: serde_json::json!({}),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
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
async fn mod_session_start_runs_at_the_host_startup_seam() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("start.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('session.start', async ($, e, next) => {
            if (typeof e.cwd !== 'string' || e.surface !== null || e.isInteractive !== false) {
              throw new Error('unexpected session.start shape');
            }
            $.ui.log('mod started', { to: 'transcript' });
            return next(e);
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("start-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = HookRegistry::new();
    registry.set_mod_host(host);
    let registry = Arc::new(RwLock::new(registry));
    let exec = Arc::new(HookExecutorImpl::new(
        registry.clone(),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        exec,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_hook_registry(registry);
    orch.fire_session_start("startup").await;
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { plugin, text }
        if plugin == "start-mod" && text == "mod started"
    )));
}

#[tokio::test]
async fn mod_settings_and_tool_list_reach_the_host_during_startup() {
    let dir = tempfile::tempdir().unwrap();
    let module = dir.path().join("settings.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('session.start', async ($, e, next) => {
            const settings = await $.settings.read({ source: 'policy' });
            const tools = await $.tool.list();
            $.ui.log(`${settings.custom}:${tools[0]?.name}:${tools[0]?.description}:${tools[0]?.mcp}`, { to: 'transcript' });
            return next(e);
          });
        }
        "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("settings-mod", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = HookRegistry::new();
    registry.set_mod_host(host);
    let registry = Arc::new(RwLock::new(registry));
    let exec = Arc::new(HookExecutorImpl::new(
        registry.clone(),
        Arc::new(UnusedHttp),
        Arc::new(UnusedRuntime),
    ));
    let output = Arc::new(MockOutputStream::new());
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(ListedTool {
        schema: serde_json::json!({"type":"object"}),
    }));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tools),
        exec,
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_hook_registry(registry)
    .with_mod_settings_reader(Arc::new(TestModSettingsReader));
    orch.fire_session_start("startup").await;
    assert!(output.snapshot().await.iter().any(|event| matches!(
        event,
        lingxi_core::host::OutputEvent::ModLog { plugin, text }
        if plugin == "settings-mod" && text == "managed:Inspector:Inspect the current repository.:false"
    )));
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
