//! orchestrator's `fire_instructions_loaded` helper, the seam the host
//! composition root (`engine-desktop`) calls once at session startup, right
//! after `fire_session_start`.
//!
//! Byte-faithful to claude-code: the eager session-start `getMemoryFiles` pass
//! fires `executeInstructionsLoadedHooks` once per LINGXI.md / `LINGXI.local.md`
//! spliced into context (`utils/claudemd.ts:1054-1071`, `utils/hooks.ts:4335-4369`),
//! each carrying that file's `file_path` / `memory_type` / `load_reason`. Every
//! top-level (parent-less) file reports `load_reason: 'session_start'`. The
//! orchestrator loads the full Managed/User/Project/Local hierarchy, tagging
//! each file with its tier, so each file fired here is `session_start` with
//! `memory_type` taken directly from that tier.
//!
//! Scenarios:
//! 1. One fire per loaded instruction file, carrying the correct
//!    `(file_path, memory_type, load_reason)` triple — `Project` for a repo
//!    LINGXI.md, `Local` for a `LINGXI.local.md`.
//! 2. A hook that itself returns an error outcome does NOT panic / break the
//!    (best-effort) fire helper.
//! 3. No `InstructionsLoaded` hook registered ⇒ firing is a strict no-op.
//! 4. No instruction files present ⇒ firing is a strict no-op (nothing fires).
//! 5. A `Managed`-tier file is reported with `memory_type: Managed`.

use async_trait::async_trait;
use hooks::definition::{HookDefinition, HookExecutor as DefHookExecutor, HookSource};
use hooks::events::{HookEvent, HookEventType, InstructionsLoadReason, InstructionsMemoryType};
use hooks::executor::BuiltinHookHandler;
use hooks::registry::{HookContext, HookRegistry};
use hooks::response::{HookOutcome, HookResult};
use hooks::HookExecutorImpl;
use lingxi_core::host::{HttpError, HttpTransport, RuntimeError, RuntimeSpawner};
use lingxi_core::types::{HookId, HttpRequest, HttpResponse};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, MemoryFile, OrchestratorConfig};
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

/// Records the `(file_path, memory_type, load_reason)` of every
/// `InstructionsLoaded` event it sees so the test can assert exactly which
/// fired (a pass-through observer — no decision).
struct RecordingHandler {
    log: Arc<Mutex<Vec<(PathBuf, InstructionsMemoryType, InstructionsLoadReason)>>>,
}
#[async_trait]
impl BuiltinHookHandler for RecordingHandler {
    fn id(&self) -> &str {
        "record-instructions-loaded"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::InstructionsLoaded {
            file_path,
            memory_type,
            load_reason,
            ..
        } = event
        {
            self.log
                .lock()
                .unwrap()
                .push((file_path.clone(), *memory_type, *load_reason));
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

/// An `InstructionsLoaded` hook that itself FAILS (non-success outcome). Proves
/// the fire helper is best-effort — a broken hook must not break the call.
struct FailingHandler;
#[async_trait]
impl BuiltinHookHandler for FailingHandler {
    fn id(&self) -> &str {
        "broken-instructions-loaded"
    }
    async fn handle(&self, _event: &HookEvent, _ctx: &HookContext) -> HookResult {
        HookResult {
            outcome: HookOutcome::Error,
            stdout: String::new(),
            stderr: "the instructions-loaded hook itself blew up".into(),
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

/// Build an orchestrator over a fixed memory fixture + cwd. The hierarchy
/// provider is the only input `fire_instructions_loaded` reads, so a static
/// fixture exercises the helper end-to-end without touching the filesystem.
fn orch_with(
    hooks: Arc<HookExecutorImpl>,
    files: Vec<MemoryFile>,
    cwd: PathBuf,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        hooks,
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(files)),
        cwd,
    )
}

fn exec_with_recorder(
    registry: Arc<RwLock<HookRegistry>>,
    handler: Arc<RecordingHandler>,
) -> Arc<HookExecutorImpl> {
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(handler);
    Arc::new(exec)
}

#[tokio::test]
async fn fire_instructions_loaded_dispatches_one_event_per_file() {
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-instructions-loaded",
        HookEventType::InstructionsLoaded,
    ));
    let exec = exec_with_recorder(registry, Arc::new(RecordingHandler { log: log.clone() }));

    // A repo LINGXI.md (Project) and a repo LINGXI.local.md (Local). The
    // `memory_type` is now taken straight from each file's tier.
    let cwd = PathBuf::from("/work/repo");
    let project = MemoryFile {
        path: cwd.join("LINGXI.md"),
        body: "project rules".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "project rules".into(),
        content_differs_from_disk: false,
    };
    let local = MemoryFile {
        path: cwd.join("LINGXI.local.md"),
        body: "local override".into(),
        is_local_override: true,
        tier: memory::lingxi_md::LingxiMdTier::Local,
        globs: None,
        raw_content: "local override".into(),
        content_differs_from_disk: false,
    };
    let orch = orch_with(exec, vec![project.clone(), local.clone()], cwd);

    orch.fire_instructions_loaded().await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            (
                project.path,
                InstructionsMemoryType::Project,
                InstructionsLoadReason::SessionStart,
            ),
            (
                local.path,
                InstructionsMemoryType::Local,
                InstructionsLoadReason::SessionStart,
            ),
        ],
        "fire_instructions_loaded must dispatch one session_start event per loaded file, \
         with memory_type derived from the file: {seen:?}"
    );
}

#[tokio::test]
async fn managed_tier_file_reports_memory_type_managed() {
    // GAP 1: an enterprise-`Managed` file fires with `memory_type: Managed`
    // (taken from the file's tier, not a path heuristic).
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-instructions-loaded",
        HookEventType::InstructionsLoaded,
    ));
    let exec = exec_with_recorder(registry, Arc::new(RecordingHandler { log: log.clone() }));

    let cwd = PathBuf::from("/work/repo");
    let managed = MemoryFile {
        path: PathBuf::from("/Library/Application Support/LingXi/LINGXI.md"),
        body: "enterprise policy".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Managed,
        globs: None,
        raw_content: "enterprise policy".into(),
        content_differs_from_disk: false,
    };
    let orch = orch_with(exec, vec![managed.clone()], cwd);

    orch.fire_instructions_loaded().await;

    let seen = log.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![(
            managed.path,
            InstructionsMemoryType::Managed,
            InstructionsLoadReason::SessionStart,
        )],
        "a Managed-tier file must fire memory_type=Managed: {seen:?}"
    );
}

#[tokio::test]
async fn failing_instructions_loaded_hook_does_not_break_fire() {
    // The registered hook itself returns a non-success outcome.
    // `fire_instructions_loaded` discards each aggregate, so the call must STILL
    // return cleanly (best-effort, identical to the other lifecycle arms).
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "broken-instructions-loaded",
        HookEventType::InstructionsLoaded,
    ));
    let mut exec = HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
    exec.register_builtin(Arc::new(FailingHandler));
    let cwd = PathBuf::from("/work/repo");
    let file = MemoryFile {
        path: cwd.join("LINGXI.md"),
        body: "x".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "x".into(),
        content_differs_from_disk: false,
    };
    let orch = orch_with(Arc::new(exec), vec![file], cwd);

    // Must not panic / hang — a broken hook is swallowed.
    orch.fire_instructions_loaded().await;
}

#[tokio::test]
async fn fire_instructions_loaded_is_noop_without_a_registered_hook() {
    // No InstructionsLoaded hook registered: firing observes nothing (strict
    // no-op), so a session with no instruction-load hooks is wholly unaffected —
    // even when instruction files ARE present.
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    let exec = exec_with_recorder(registry, Arc::new(RecordingHandler { log: log.clone() }));
    let cwd = PathBuf::from("/work/repo");
    let file = MemoryFile {
        path: cwd.join("LINGXI.md"),
        body: "x".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "x".into(),
        content_differs_from_disk: false,
    };
    let orch = orch_with(exec, vec![file], cwd);

    orch.fire_instructions_loaded().await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no InstructionsLoaded hook registered ⇒ firing must be a strict no-op"
    );
}

#[tokio::test]
async fn fire_instructions_loaded_is_noop_with_no_memory_files() {
    // A registered hook but NO instruction files: firing observes nothing, so a
    // memory-less project never spuriously fires the hook.
    let log = Arc::new(Mutex::new(Vec::new()));
    let registry = Arc::new(RwLock::new(HookRegistry::new()));
    registry.write().await.register(builtin_hook(
        "record-instructions-loaded",
        HookEventType::InstructionsLoaded,
    ));
    let exec = exec_with_recorder(registry, Arc::new(RecordingHandler { log: log.clone() }));
    let orch = orch_with(exec, Vec::new(), PathBuf::from("/work/repo"));

    orch.fire_instructions_loaded().await;

    assert!(
        log.lock().unwrap().is_empty(),
        "no instruction files ⇒ firing must be a strict no-op"
    );
}
