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

/// Capture the complete current native eager-hook payload. Acquisition and
/// rendering are replayed over real files by memory_block's paired golden test.
struct DetailedRecordingHandler {
    log: Arc<Mutex<Vec<serde_json::Value>>>,
}
#[async_trait]
impl BuiltinHookHandler for DetailedRecordingHandler {
    fn id(&self) -> &str {
        "record-current-eager-payload"
    }
    async fn handle(&self, event: &HookEvent, _ctx: &HookContext) -> HookResult {
        if let HookEvent::InstructionsLoaded {
            file_path,
            memory_type,
            load_reason,
            globs,
            trigger_file_path,
            parent_file_path,
        } = event
        {
            // The eager native producer supplies no lazy trigger.
            assert!(trigger_file_path.is_none());
            let mut payload = serde_json::json!({
                "file_path": file_path,
                "memory_type": memory_type,
                "load_reason": load_reason,
            });
            if let Some(globs) = globs {
                payload["globs"] = serde_json::json!(globs);
            }
            if let Some(parent) = parent_file_path {
                payload["parent_file_path"] = serde_json::json!(parent);
            }
            self.log.lock().unwrap().push(payload);
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

#[tokio::test]
async fn current_native_eager_hook_goldens_preserve_globs_and_import_parents() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/instruction_eager_2_1_287.json")).unwrap();
    let cases = oracle["hookCases"].as_array().unwrap();
    assert_eq!(cases.len(), 11);
    for case in cases {
        let cwd = PathBuf::from("/work/current-eager-fixture");
        let replace = |text: &str| {
            text.replace("/repo", &cwd.display().to_string())
                .replace("CLAUDE.md", branding::MEMORY_FILE)
                .replace(".claude", branding::DOT_DIR)
        };
        let files = case["expected"]["acquired"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| MemoryFile {
                source_content: None,
                path: replace(file["path"].as_str().unwrap()).into(),
                parent: file
                    .get("parent")
                    .and_then(serde_json::Value::as_str)
                    .map(|path| PathBuf::from(replace(path))),
                body: file["content"].as_str().unwrap().into(),
                is_local_override: false,
                tier: serde_json::from_value(file["type"].clone()).unwrap(),
                globs: file
                    .get("globs")
                    .map(|value| serde_json::from_value(value.clone()).unwrap()),
                raw_content: file
                    .get("rawContent")
                    .unwrap_or(&file["content"])
                    .as_str()
                    .unwrap()
                    .into(),
                content_differs_from_disk: file["contentDiffersFromDisk"].as_bool().unwrap(),
            })
            .collect();
        let log = Arc::new(Mutex::new(Vec::new()));
        let registry = Arc::new(RwLock::new(HookRegistry::new()));
        registry.write().await.register(builtin_hook(
            "record-current-eager-payload",
            HookEventType::InstructionsLoaded,
        ));
        let mut exec =
            HookExecutorImpl::new(registry, Arc::new(UnusedHttp), Arc::new(UnusedRuntime));
        exec.register_builtin(Arc::new(DetailedRecordingHandler { log: log.clone() }));
        let orch = orch_with(Arc::new(exec), files, cwd.clone());
        orch.fire_instructions_loaded().await;
        let mut expected = case["expected"]["hooks"].clone();
        for event in expected.as_array_mut().unwrap() {
            for key in ["file_path", "parent_file_path"] {
                if let Some(path) = event.get(key).and_then(serde_json::Value::as_str) {
                    event[key] = serde_json::Value::String(replace(path));
                }
            }
        }
        assert_eq!(
            serde_json::json!(log.lock().unwrap().clone()),
            expected,
            "{}",
            case["name"]
        );
        // The same cached acquisition never fires a second eager batch.
        orch.fire_instructions_loaded().await;
        assert_eq!(
            serde_json::json!(log.lock().unwrap().clone()),
            expected,
            "{}",
            case["name"]
        );
    }
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
        parent: None,
        source_content: None,
        path: cwd.join("LINGXI.md"),
        body: "project rules".into(),
        is_local_override: false,
        tier: memory::lingxi_md::LingxiMdTier::Project,
        globs: None,
        raw_content: "project rules".into(),
        content_differs_from_disk: false,
    };
    let local = MemoryFile {
        parent: None,
        source_content: None,
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
        parent: None,
        source_content: None,
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
        parent: None,
        source_content: None,
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
        parent: None,
        source_content: None,
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
