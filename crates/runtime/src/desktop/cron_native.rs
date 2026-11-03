//! CLI session-aware scheduler adapter. Sessions retain their ordinary writer lease.
use crate::desktop::{ConversationOrchestrator, DesktopConfig};
use async_trait::async_trait;
use futures::FutureExt as _;
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::OrchestratorHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

tokio::task_local! { pub(crate) static CHILD_RUNTIME: bool; pub(crate) static HOST_AUTOMATION: bool; }
pub(crate) fn host_owns_automation() -> bool {
    HOST_AUTOMATION.try_with(|value| *value).unwrap_or(false)
}
pub(crate) fn is_child_runtime() -> bool {
    CHILD_RUNTIME.try_with(|value| *value).unwrap_or(false)
}
pub(crate) fn should_start_native_scheduler(config: &DesktopConfig) -> bool {
    config.enable_automation_scheduler
        && config.host_workspace_trusted.is_none()
        && !is_child_runtime()
        && !host_owns_automation()
}
#[derive(Clone)]
pub(crate) struct NativeCronFirer {
    pub config: DesktopConfig,
    pub permissions: Arc<dyn client::adapter::PermissionRequestSink>,
    pub current: std::sync::Weak<ConversationOrchestrator>,
    supervisor: Arc<NativeRunSupervisor>,
}
impl NativeCronFirer {
    pub(crate) fn new(
        config: DesktopConfig,
        permissions: Arc<dyn client::adapter::PermissionRequestSink>,
        current: std::sync::Weak<ConversationOrchestrator>,
    ) -> Self {
        Self {
            config,
            permissions,
            current,
            supervisor: Arc::new(NativeRunSupervisor::new()),
        }
    }
}
type NativeRunReply = Result<cron::AutomationRunResult, String>;
const NATIVE_CLEANUP_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The scheduler owns the waiter; this supervisor owns runtime destruction.
/// Dropping a waiter requests cancellation without aborting child cleanup.
struct NativeRunSupervisor {
    runtime: Arc<dyn lingxi_core::host::RuntimeSpawner>,
    runs: std::sync::Mutex<std::collections::HashMap<String, Arc<NativeRunControl>>>,
}
struct NativeRunControl {
    cancel: CancellationToken,
    finished: tokio::sync::watch::Receiver<bool>,
    handle: std::sync::Mutex<Option<lingxi_core::host::BackgroundTaskHandle>>,
    cleanup_error: std::sync::Mutex<Option<String>>,
}
struct CancelNativeWaiter(CancellationToken);
impl Drop for CancelNativeWaiter {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
struct NativeCompletion(tokio::sync::watch::Sender<bool>);
impl Drop for NativeCompletion {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}
struct NativeSupervisedFuture {
    // Drop execution resources before signalling completion.
    future: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
    _completion: NativeCompletion,
}
impl std::future::Future for NativeSupervisedFuture {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.get_mut().future.as_mut().poll(cx)
    }
}
impl NativeRunSupervisor {
    fn new() -> Self {
        Self {
            runtime: Arc::new(platform_posix::PosixRuntime::new()),
            runs: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
    async fn run<F, Fut>(&self, id: &str, execute: F) -> NativeRunReply
    where
        F: FnOnce(CancellationToken, Arc<NativeRunControl>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = NativeRunReply> + Send + 'static,
    {
        let (completed, finished) = tokio::sync::watch::channel(false);
        let run = Arc::new(NativeRunControl {
            cancel: CancellationToken::new(),
            finished,
            handle: std::sync::Mutex::new(None),
            cleanup_error: std::sync::Mutex::new(None),
        });
        {
            let mut runs = self
                .runs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if runs.contains_key(id) {
                return Err("busy:Previous scheduled runtime is still stopping".into());
            }
            runs.insert(id.into(), run.clone());
        }
        let _cancel_on_drop = CancelNativeWaiter(run.cancel.clone());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let control = run.clone();
        let cancel = run.cancel.clone();
        let owned = NativeSupervisedFuture {
            future: Box::pin(async move {
                let result = execute(cancel, control).await;
                let _ = result_tx.send(result);
            }),
            _completion: NativeCompletion(completed),
        };
        match self
            .runtime
            .spawn("cron-native-owner", Box::pin(owned))
            .await
        {
            Ok(handle) => {
                *run.handle
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle)
            }
            Err(error) => {
                self.remove(id, &run);
                return Err(format!(
                    "interrupted:Cannot start scheduled runtime owner: {error}"
                ));
            }
        }
        let result = result_rx.await.unwrap_or_else(|_| {
            Err("interrupted:Scheduled runtime owner ended without a result".into())
        });
        Self::wait_finished(&run).await;
        self.reap(id, &run).await?;
        result
    }
    async fn wait_finished(run: &NativeRunControl) {
        let mut finished = run.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
    async fn cancel_run(&self, id: &str) -> Result<(), String> {
        let run = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned();
        let Some(run) = run else {
            return Ok(());
        };
        run.cancel.cancel();
        if tokio::time::timeout(NATIVE_CLEANUP_WAIT, Self::wait_finished(&run))
            .await
            .is_err()
        {
            let reason = run
                .cleanup_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_else(|| "execution is still stopping".into());
            return Err(format!(
                "Scheduled runtime cleanup is still in progress: {reason}"
            ));
        }
        self.reap(id, &run).await
    }
    async fn reap(&self, id: &str, run: &Arc<NativeRunControl>) -> Result<(), String> {
        let handle = run
            .handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            // Resource destruction is proven; remove the completed handle.
            self.runtime
                .cancel(&handle)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.remove(id, run);
        Ok(())
    }
    fn remove(&self, id: &str, run: &Arc<NativeRunControl>) {
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if runs
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, run))
        {
            runs.remove(id);
        }
    }
}

async fn drain_supervised_runtime<F, Fut>(status: &NativeRunControl, mut drain: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = crate::desktop::DesktopSessionShutdownReport>,
{
    let mut retry_delay = std::time::Duration::from_millis(100);
    loop {
        let report = drain().await;
        if report.complete {
            *status
                .cleanup_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            return;
        }
        let error = if report.errors.is_empty() {
            "runtime did not confirm shutdown".into()
        } else {
            report.errors.join("; ")
        };
        let changed = {
            let mut previous = status
                .cleanup_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed = previous.as_ref() != Some(&error);
            *previous = Some(error.clone());
            changed
        };
        if changed {
            tracing::warn!(%error, "scheduled child runtime cleanup incomplete; retaining its owner for retry");
        }
        tokio::time::sleep(retry_delay).await;
        retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(2));
    }
}

struct QuietOutput;
#[async_trait]
impl lingxi_core::host::OutputStream for QuietOutput {
    async fn emit_text(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) {}
    async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    async fn emit_tool_call(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) {
    }
    async fn emit_tool_result(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _mcp_meta: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>,
    ) {
    }
}
#[async_trait]
impl cron::CronJobFirer for NativeCronFirer {
    async fn fire(
        &self,
        _: &str,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
    ) -> Result<String, String> {
        Err("Versioned execution settings required".into())
    }
    async fn fire_automation(
        &self,
        request: &cron::automation::AutomationRunRequest,
    ) -> Result<cron::automation::AutomationRunResult, String> {
        // The firer is attached before the trust decision is made (and trust can
        // be revoked or granted mid-session), so the gate has to live on the
        // execution edge, not on the wiring. Without it, a project-local
        // `scheduled_tasks.json` in a repo the user declined to trust still runs
        // its prompt with tool use on the first tick. Same verdict and same
        // `paused:` prefix as the bridge's `run_scheduled_turn`, so the run is
        // retried rather than retired.
        if let Some(orchestrator) = self.current.upgrade() {
            if !orchestrator.workspace_trusted().await {
                return Err("paused:Trust this workspace before running scheduled tasks".into());
            }
        }
        let owned = self.clone();
        let request = request.clone();
        self.supervisor
            .run(&request.run_id.clone(), move |cancel, status| async move {
                owned.fire_owned(&request, cancel, status).await
            })
            .await
    }
    async fn cancel_run(&self, run_id: &str) -> Result<(), String> {
        self.supervisor.cancel_run(run_id).await
    }
}

#[derive(Debug, Clone)]
struct ScheduledSource {
    id: lingxi_core::types::SessionId,
    path: PathBuf,
    mode: ConversationMode,
    scope: String,
    existing: bool,
}

fn scheduled_source(
    config: &DesktopConfig,
    automation: &cron::CronAutomation,
) -> Result<ScheduledSource, String> {
    use cron::automation::RunMode;
    cron::validate_automation_source(automation).map_err(|error| format!("paused:{error}"))?;
    let (id, path, mode, scope, existing) = match automation.run_mode {
        RunMode::SelectedSession => (
            automation.target_session_id.as_deref(),
            automation.target_session_transcript_path.as_deref(),
            automation.target_session_mode.as_deref(),
            automation.target_session_scope_key.as_deref(),
            true,
        ),
        RunMode::TaskSession if automation.owned_session_id.is_some() => {
            if automation.owned_session_mode != automation.session_mode
                || automation.owned_session_scope_key != automation.session_scope_key
            {
                return Err("paused:Task session owner or mode changed".into());
            }
            (
                automation.owned_session_id.as_deref(),
                automation.owned_session_transcript_path.as_deref(),
                automation.owned_session_mode.as_deref(),
                automation.owned_session_scope_key.as_deref(),
                true,
            )
        }
        RunMode::NewSession | RunMode::TaskSession => {
            if automation.run_mode == RunMode::TaskSession
                && (automation.owned_session_transcript_path.is_some()
                    || automation.owned_session_mode.is_some()
                    || automation.owned_session_scope_key.is_some())
            {
                return Err("paused:Task session source is incomplete".into());
            }
            (
                None,
                None,
                automation.session_mode.as_deref(),
                automation.session_scope_key.as_deref(),
                false,
            )
        }
    };
    let mode = mode
        .and_then(ConversationMode::from_str)
        .ok_or("paused:Scheduled session mode is missing or invalid")?;
    let scope = scope
        .filter(|value| !value.trim().is_empty() && value.len() <= 8192)
        .ok_or("paused:Scheduled session owner is missing or invalid")?
        .to_owned();
    let id = match id {
        Some(id) => lingxi_core::types::SessionId::parse_prefixed(id)
            .ok_or("paused:Invalid scheduled session ID")?,
        None if !existing => lingxi_core::types::SessionId::new(),
        None => return Err("paused:Scheduled session ID is missing".into()),
    };
    let path = match path {
        Some(path) => PathBuf::from(path),
        None if !existing => session::jsonl::session_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &id.as_uuid().to_string(),
        ),
        None => return Err("paused:Scheduled transcript path is missing".into()),
    };
    if !path.is_absolute() || path.to_str().is_none() {
        return Err("paused:Scheduled transcript path is not absolute UTF-8".into());
    }
    Ok(ScheduledSource {
        id,
        path,
        mode,
        scope,
        existing,
    })
}

impl NativeCronFirer {
    async fn fire_owned(
        &self,
        request: &cron::AutomationRunRequest,
        cancel: CancellationToken,
        status: Arc<NativeRunControl>,
    ) -> Result<cron::AutomationRunResult, String> {
        if cancel.is_cancelled() {
            return Err("cancelled:Scheduled run cancelled".into());
        }
        let automation = request
            .task
            .automation
            .as_ref()
            .ok_or("Missing automation settings")?;
        let mut scheduler_config = self.config.clone();
        if !scheduler_config.cwd.is_absolute() || !scheduler_config.lingxi_home.is_absolute() {
            let process_cwd = std::env::current_dir()
                .map_err(|error| format!("paused:Cannot resolve scheduled workspace: {error}"))?;
            if !scheduler_config.cwd.is_absolute() {
                scheduler_config.cwd = process_cwd.join(&scheduler_config.cwd);
            }
            if !scheduler_config.lingxi_home.is_absolute() {
                scheduler_config.lingxi_home = process_cwd.join(&scheduler_config.lingxi_home);
            }
        }
        let source = scheduled_source(&scheduler_config, automation)?;
        let reasoning: client::protocol::controls::ReasoningSelectionDto =
            serde_json::from_value(automation.reasoning.clone())
                .map_err(|e| format!("paused:Invalid reasoning: {e}"))?;
        let reasoning = match reasoning {
            client::protocol::controls::ReasoningSelectionDto::Automatic => {
                lingxi_core::host::ReasoningSelection::Automatic
            }
            client::protocol::controls::ReasoningSelectionDto::Disabled => {
                lingxi_core::host::ReasoningSelection::Disabled
            }
            client::protocol::controls::ReasoningSelectionDto::Enabled => {
                lingxi_core::host::ReasoningSelection::Enabled
            }
            client::protocol::controls::ReasoningSelectionDto::Level { id } => {
                lingxi_core::host::ReasoningSelection::Level { id }
            }
            client::protocol::controls::ReasoningSelectionDto::TokenBudget { tokens } => {
                lingxi_core::host::ReasoningSelection::TokenBudget { tokens }
            }
            _ => return Err("paused:Unsupported reasoning".into()),
        };
        let owner = self.current.upgrade().ok_or("Scheduler session closed")?;
        if !owner.workspace_trusted().await {
            return Err("paused:Workspace is not trusted".into());
        }
        if source.existing && source.id == owner.current_session_id().await {
            let fs = super::platform::DesktopFileSystem::new(scheduler_config.cwd.clone());
            let expected = source.id;
            let binding_owner = owner.clone();
            let binding_source = source.clone();
            let home = scheduler_config.lingxi_home.clone();
            let (outcome, captured_session, summary) = owner
                .run_scheduled_turn_in_session(
                    expected,
                    request.task.prompt.clone(),
                    &automation.model,
                    reasoning,
                    cancel.clone(),
                    async {
                        validate_live_source(&home, &binding_source, &binding_owner).await?;
                        bind_source(&fs, &scheduler_config.cwd, request, &binding_source).await
                    },
                )
                .await?;
            validate_outcome(outcome)?;
            debug_assert_eq!(captured_session, expected);
            return Ok(cron::automation::AutomationRunResult {
                session_id: source.id.as_uuid().to_string(),
                session_transcript_path: Some(source.path.to_string_lossy().into_owned()),
                session_mode: Some(source.mode.as_str().into()),
                session_scope_key: Some(source.scope.clone()),
                summary,
            });
        }
        let (lease, captured) = claim_and_capture_target(&scheduler_config, &source).await?;
        let mut cfg = scheduler_config.clone();
        cfg.session_id_override = Some(source.id.as_uuid().to_string());
        cfg.session_writer_lease = Some(lease.clone());
        cfg.session_persistence = true;
        cfg.session_agent_observer = None;
        cfg.parent_session_id = None;
        cfg.initial_conversation_mode = Some(source.mode);
        cfg.session_transcript_path = Some(source.path.clone());
        cfg.session_transcript_authority = None;
        cfg.session_resume_snapshot = None;
        cfg.session_resume_target_snapshot = None;
        cfg.session_resume_cost = None;
        cfg.defer_session_start = false;
        cfg.restore_draft = !source.existing;
        cfg.durable_draft_title = (!source.existing).then(|| {
            automation
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("Scheduled task")
                .to_owned()
        });
        let fs = super::platform::DesktopFileSystem::new(scheduler_config.cwd.clone());
        if let Some(captured) = captured {
            cfg.session_transcript_path = Some(captured.path.clone());
            if let Some(cwd) = captured.cwd.filter(|cwd| cwd.is_dir()) {
                cfg.cwd = cwd;
            }
            cfg.session_resume_cost = super::session_state::capture_resume_cost(
                &cfg.lingxi_home,
                source.id.as_uuid(),
                lease.clone(),
                &captured.content,
            )
            .await
            .map_err(|error| format!("paused:Cannot capture scheduled session: {error}"))?;
            bind_source(&fs, &scheduler_config.cwd, request, &source).await?;
            let verify_home = scheduler_config.lingxi_home.clone();
            let verify_source = source.clone();
            let verified = tokio::task::spawn_blocking(move || {
                capture_target_source(&verify_home, &verify_source)
            })
            .await
            .map_err(|error| format!("paused:Target capture failed: {error}"))??;
            if verified.path != captured.path
                || verified.root_identity != captured.root_identity
                || verified.file_identity != captured.file_identity
                || verified.content != captured.content
            {
                return Err("paused:Target transcript changed before runtime assembly".into());
            }
            cfg.session_transcript_authority = Some(captured.authority.clone());
            cfg.session_resume_snapshot = Some(super::DesktopSessionResumeSnapshot {
                source_session_id: source.id.as_uuid().to_string(),
                content: captured.content.into(),
            });
        }
        let runtime = CHILD_RUNTIME
            .scope(
                true,
                Box::pin(crate::desktop::build(
                    cfg,
                    Arc::new(QuietOutput),
                    self.permissions.clone(),
                )),
            )
            .await
            .map_err(child_build_error)?;
        let execution = std::panic::AssertUnwindSafe(async {
            if cancel.is_cancelled() {
                return Err("cancelled:Scheduled run cancelled".into());
            }
            if !source.existing {
                validate_live_source(
                    &scheduler_config.lingxi_home,
                    &source,
                    &runtime.orchestrator,
                )
                .await?;
                bind_source(&fs, &scheduler_config.cwd, request, &source).await?;
            }
            let outcome = runtime
                .orchestrator
                .run_scheduled_turn(
                    request.task.prompt.clone(),
                    &automation.model,
                    reasoning,
                    cancel.clone(),
                )
                .await?;
            finish(&runtime.orchestrator, outcome, &source).await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err("Scheduled execution panicked".into()));
        drain_supervised_runtime(&status, || runtime.session_lifecycle.shutdown_and_drain()).await;
        execution
    }
}
fn child_build_error(error: crate::desktop::BuildError) -> String {
    // Writer contention is classified before building the child runtime.
    // A failed child configuration requires repair, not an unbounded busy retry.
    format!("paused:Cannot initialize scheduled runtime: {error}")
}

struct CapturedSource {
    path: PathBuf,
    content: String,
    cwd: Option<PathBuf>,
    root_identity: lingxi_core::host::rooted_fs::RootIdentity,
    file_identity: lingxi_core::host::rooted_fs::RootIdentity,
    authority: session::jsonl::writer::TranscriptFileAuthority,
}

async fn bind_source(
    fs: &dyn lingxi_core::host::FileSystem,
    root: &Path,
    request: &cron::AutomationRunRequest,
    source: &ScheduledSource,
) -> Result<(), String> {
    cron::automation::bind_automation_run_session_with_source(
        fs,
        root,
        request,
        &source.id.as_uuid().to_string(),
        source
            .path
            .to_str()
            .ok_or("paused:Invalid scheduled transcript path")?,
        source.mode.as_str(),
        Some(&source.scope),
    )
    .await
}

async fn validate_live_source(
    home: &Path,
    source: &ScheduledSource,
    owner: &ConversationOrchestrator,
) -> Result<(), String> {
    let mode = owner.conversation_mode().await;
    if mode.session_id != source.id.as_uuid().to_string() || mode.mode != source.mode {
        return Err("paused:Live scheduled session mode or identity changed".into());
    }
    let writer = owner
        .session_transcript_writer()
        .ok_or("paused:Live session has no transcript writer")?;
    let active = std::fs::canonicalize(writer.active_path())
        .map_err(|error| format!("paused:Live transcript is unavailable: {error}"))?;
    let home = home.to_path_buf();
    let source = source.clone();
    let captured = tokio::task::spawn_blocking(move || capture_target_source(&home, &source))
        .await
        .map_err(|error| format!("paused:Target capture failed: {error}"))??;
    if active != captured.path {
        return Err("paused:Selected session belongs to another transcript".into());
    }
    writer
        .verify_active_transcript_source(&captured.authority)
        .map_err(|error| format!("paused:Live transcript source changed: {error}"))?;
    Ok(())
}

/// Read an exact file under the projects root with the same pinned root and
/// inode across the entire capture. Callers hold the session writer lease or
/// the live owner's turn gate; neither can authorize a different source.
fn target_source_error(error: impl std::fmt::Display) -> String {
    format!("paused:Target session is unavailable: {error}")
}

fn capture_target_source(home: &Path, source: &ScheduledSource) -> Result<CapturedSource, String> {
    capture_target_source_with_hook(home, source, || {})
}

fn capture_target_source_with_hook(
    home: &Path,
    source: &ScheduledSource,
    before_recheck: impl FnOnce(),
) -> Result<CapturedSource, String> {
    use lingxi_core::host::rooted_fs::{
        open_read_file_pinned, opened_file_identity, root_identity, validate_open_file_single_link,
    };
    use std::io::Read;
    let logical_home = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(target_source_error)?
            .join(home)
    };
    // Only the configured home is canonicalized. Keep `projects`, the bucket,
    // and the selected filename as original relative components for the
    // no-follow open, so a replaced project directory cannot redirect a run.
    let root = std::fs::canonicalize(&logical_home).map_err(target_source_error)?;
    let logical_projects = logical_home.join("projects");
    let canonical_projects = root.join("projects");
    let selected = source
        .path
        .strip_prefix(&logical_projects)
        .or_else(|_| source.path.strip_prefix(&canonical_projects))
        .map_err(|_| "paused:Target transcript is outside the projects root")?;
    if selected.components().count() != 2
        || !selected
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        || selected.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || selected
            .file_stem()
            .and_then(|value| value.to_str())
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            != Some(source.id.as_uuid())
    {
        return Err("paused:Target transcript does not match the session identity".into());
    }
    let relative = Path::new("projects").join(selected);
    let path = root.join(&relative);
    let root_id = root_identity(&root).map_err(target_source_error)?;
    let mut file =
        open_read_file_pinned(&root, &relative, Some(&root_id)).map_err(target_source_error)?;
    validate_open_file_single_link(&file).map_err(target_source_error)?;
    let identity = opened_file_identity(&file).map_err(target_source_error)?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(target_source_error)?;
    let loaded =
        session::jsonl::loader::route_captured_session_source(&path, source.id.as_uuid(), &content)
            .map_err(target_source_error)?;
    if loaded.malformed_line_count != 0 {
        return Err("paused:Target transcript is malformed".into());
    }
    let actual = session::JsonlWriter::fixed_session_mode_from_source(
        &content,
        &source.id.as_uuid().to_string(),
    )
    .map_err(target_source_error)?;
    if actual != source.mode {
        return Err("paused:Target session mode does not match the selected source".into());
    }
    before_recheck();
    let mut current =
        open_read_file_pinned(&root, &relative, Some(&root_id)).map_err(target_source_error)?;
    validate_open_file_single_link(&current).map_err(target_source_error)?;
    if opened_file_identity(&current).map_err(target_source_error)? != identity {
        return Err("paused:Target transcript changed during capture".into());
    }
    let mut current_content = String::new();
    current
        .read_to_string(&mut current_content)
        .map_err(target_source_error)?;
    if current_content != content
        || std::fs::canonicalize(&source.path).map_err(target_source_error)? != path
    {
        return Err("paused:Target transcript changed during capture".into());
    }
    let cwd = session::jsonl::loader::transcript_workspace_path(&path, &loaded).map(PathBuf::from);
    let authority = session::jsonl::writer::TranscriptFileAuthority::from_capture(
        root, relative, root_id, identity,
    )
    .map_err(target_source_error)?;
    Ok(CapturedSource {
        path,
        content,
        cwd,
        root_identity: root_id,
        file_identity: identity,
        authority,
    })
}

async fn claim_and_capture_target(
    config: &DesktopConfig,
    source: &ScheduledSource,
) -> Result<
    (
        lingxi_core::host::live_sessions::SharedSessionWriterLease,
        Option<CapturedSource>,
    ),
    String,
> {
    // The same lease protects both the history snapshot and the subsequent
    // runtime. Reading before claiming would let a foreground writer append
    // between replay and construction, leaving the scheduled turn on an old parent.
    let lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
        config.lingxi_home.join("sessions"),
    )
    .claim_session_id(&source.id.as_uuid().to_string(), std::process::id())
    .map_err(|error| {
        let prefix = if matches!(
            error.kind(),
            std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::WouldBlock
        ) {
            "busy"
        } else {
            "paused"
        };
        format!("{prefix}:Cannot acquire scheduled session: {error}")
    })?
    .into_shared();
    let captured = if source.existing {
        let home = config.lingxi_home.clone();
        let source = source.clone();
        Some(
            tokio::task::spawn_blocking(move || capture_target_source(&home, &source))
                .await
                .map_err(|error| format!("paused:Target capture failed: {error}"))??,
        )
    } else {
        None
    };
    Ok((lease, captured))
}

async fn finish(
    orch: &ConversationOrchestrator,
    outcome: orchestrator::conversation::TurnOutcome,
    source: &ScheduledSource,
) -> Result<cron::automation::AutomationRunResult, String> {
    validate_outcome(outcome)?;
    let summary = orch
        .snapshot_history()
        .await
        .iter()
        .rev()
        .find(|message| {
            matches!(
                message,
                lingxi_core::types::ConversationMessage::Assistant { .. }
            )
        })
        .map(lingxi_core::types::ConversationMessage::text_content)
        .unwrap_or_default();
    Ok(cron::automation::AutomationRunResult {
        session_id: source.id.as_uuid().to_string(),
        session_transcript_path: Some(source.path.to_string_lossy().into_owned()),
        session_mode: Some(source.mode.as_str().into()),
        session_scope_key: Some(source.scope.clone()),
        summary,
    })
}

fn validate_outcome(outcome: orchestrator::conversation::TurnOutcome) -> Result<(), String> {
    if outcome == orchestrator::conversation::TurnOutcome::Cancelled {
        return Err("cancelled:Scheduled run cancelled".into());
    }
    if outcome != orchestrator::conversation::TurnOutcome::EndTurn {
        return Err("Scheduled run did not complete".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn automation(value: serde_json::Value) -> cron::CronAutomation {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn scheduled_source_requires_complete_immutable_owner_and_mode() {
        let root = tempfile::tempdir().unwrap();
        let config = DesktopConfig {
            cwd: root.path().join("cwd"),
            lingxi_home: root.path().join("home"),
            ..Default::default()
        };
        let id = uuid::Uuid::new_v4();
        let path = root.path().join("selected.jsonl");
        let mut task = automation(serde_json::json!({
            "version":2, "model":"test/model", "runMode":"selected_session",
            "targetSessionId":id, "targetSessionTranscriptPath":path,
            "targetSessionMode":"chat", "targetSessionScopeKey":"global"
        }));
        let selected = scheduled_source(&config, &task).unwrap();
        assert_eq!(selected.id.as_uuid(), id);
        assert_eq!(selected.mode, ConversationMode::Chat);
        assert_eq!(selected.scope, "global");
        task.target_session_transcript_path = None;
        assert!(scheduled_source(&config, &task).is_err());

        let mut owned = automation(serde_json::json!({
            "version":2, "model":"test/model", "runMode":"task_session",
            "sessionMode":"chat", "sessionScopeKey":"global", "ownedSessionId":id,
            "ownedSessionTranscriptPath":path, "ownedSessionMode":"chat", "ownedSessionScopeKey":"global"
        }));
        assert_eq!(
            scheduled_source(&config, &owned).unwrap().mode,
            ConversationMode::Chat
        );
        owned.session_mode = Some("code".into());
        assert!(scheduled_source(&config, &owned).is_err());
        owned.session_mode = Some("chat".into());
        owned.owned_session_transcript_path = None;
        assert!(scheduled_source(&config, &owned).is_err());

        let fresh = automation(serde_json::json!({
            "version":2, "model":"test/model", "runMode":"new_session",
            "sessionMode":"chat", "sessionScopeKey":"global"
        }));
        let source = scheduled_source(&config, &fresh).unwrap();
        assert!(!source.existing);
        assert_eq!(source.mode, ConversationMode::Chat);
        assert_eq!(source.scope, "global");
    }

    #[tokio::test]
    async fn selected_source_never_switches_to_a_same_uuid_sibling() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let a = temp.path().join("a");
        let b = temp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let id = lingxi_core::types::SessionId::new();
        let path = |cwd: &Path| {
            session::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.as_uuid().to_string())
        };
        let first = path(&a);
        let other = path(&b);
        for (file, cwd, text, mode) in [
            (&first, &a, "selected A", "chat"),
            (&other, &b, "sibling B", "code"),
        ] {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, format!("{}\n{}\n",
                serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":mode, "instructionEpoch":0}),
                serde_json::json!({"type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
                    "sessionId":id.as_uuid(), "cwd":cwd, "message":{"role":"user", "content":text}}),
            )).unwrap();
        }
        let config = DesktopConfig {
            cwd: a,
            lingxi_home: home.clone(),
            ..Default::default()
        };
        let source = ScheduledSource {
            id,
            path: first.clone(),
            mode: ConversationMode::Chat,
            scope: "global".into(),
            existing: true,
        };
        let other_before = std::fs::read(&other).unwrap();
        let (lease, selected) = claim_and_capture_target(&config, &source).await.unwrap();
        let selected = selected.unwrap();
        assert_eq!(selected.path, first.canonicalize().unwrap());
        assert!(selected.content.contains("selected A"));
        assert!(!selected.content.contains("sibling B"));
        drop(lease);
        let mut changed_mode = source.clone();
        changed_mode.mode = ConversationMode::Code;
        assert!(claim_and_capture_target(&config, &changed_mode)
            .await
            .err()
            .unwrap()
            .contains("mode"));
        std::fs::remove_file(first).unwrap();
        assert!(claim_and_capture_target(&config, &source).await.is_err());
        assert_eq!(std::fs::read(other).unwrap(), other_before);
    }

    #[test]
    fn target_capture_rejects_inode_or_profile_changes_before_recheck() {
        for change in ["inode", "profile"] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let cwd = temp.path().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let id = lingxi_core::types::SessionId::new();
            let path = session::jsonl::session_path(
                &home,
                &cwd.to_string_lossy(),
                &id.as_uuid().to_string(),
            );
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original = format!(
                "{}\n{}\n",
                serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":"chat", "instructionEpoch":4}),
                serde_json::json!({"type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
                    "sessionId":id.as_uuid(), "cwd":cwd, "message":{"role":"user", "content":"selected"}}),
            );
            std::fs::write(&path, &original).unwrap();
            let source = ScheduledSource {
                id,
                path: path.clone(),
                mode: ConversationMode::Chat,
                scope: "global".into(),
                existing: true,
            };
            let error = capture_target_source_with_hook(&home, &source, || {
                if change == "inode" {
                    let replacement = path.with_extension("replacement");
                    std::fs::write(&replacement, &original).unwrap();
                    std::fs::rename(replacement, &path).unwrap();
                } else {
                    let changed =
                        original.replace("\"instructionEpoch\":4", "\"instructionEpoch\":5");
                    assert_ne!(changed, original);
                    std::fs::write(&path, changed).unwrap();
                }
            })
            .err()
            .expect("changed authority must not be captured");
            assert!(error.contains("changed"), "{error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn selected_target_rejects_symlinked_projects_root_or_bucket() {
        use std::os::unix::fs::symlink;
        for changed in ["root", "bucket"] {
            let temp = tempfile::tempdir().unwrap();
            let home = temp.path().join("home");
            let a = temp.path().join("a");
            let b = temp.path().join("b");
            std::fs::create_dir_all(&a).unwrap();
            std::fs::create_dir_all(&b).unwrap();
            let id = lingxi_core::types::SessionId::new();
            let path_for = |cwd: &Path| {
                session::jsonl::session_path(
                    &home,
                    &cwd.to_string_lossy(),
                    &id.as_uuid().to_string(),
                )
            };
            let selected = path_for(&a);
            let sibling = path_for(&b);
            for (path, text) in [(&selected, "selected"), (&sibling, "sibling")] {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, format!("{}\n{}\n",
                    serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":"code", "instructionEpoch":0}),
                    serde_json::json!({"type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
                        "sessionId":id.as_uuid(), "message":{"role":"user", "content":text}}),
                )).unwrap();
            }
            let sibling_before = std::fs::read(&sibling).unwrap();
            let source = ScheduledSource {
                id,
                path: selected.clone(),
                mode: ConversationMode::Code,
                scope: "project".into(),
                existing: true,
            };
            if changed == "bucket" {
                let bucket = selected.parent().unwrap();
                std::fs::rename(bucket, bucket.with_extension("saved")).unwrap();
                symlink(sibling.parent().unwrap(), bucket).unwrap();
            } else {
                let projects = home.join("projects");
                let saved = home.join("projects-saved");
                std::fs::rename(&projects, &saved).unwrap();
                symlink(saved, projects).unwrap();
            }
            assert!(capture_target_source(&home, &source).is_err(), "{changed}");
            assert_eq!(std::fs::read(&sibling).unwrap(), sibling_before);
        }
    }

    #[test]
    fn selected_target_rejects_malformed_history_without_writes() {
        use std::io::Write;
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let id = lingxi_core::types::SessionId::new();
        let path =
            session::jsonl::session_path(&home, &cwd.to_string_lossy(), &id.as_uuid().to_string());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(&path).unwrap();
        writeln!(file, "{}\n{}\n{{broken",
            serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":"code", "instructionEpoch":0}),
            serde_json::json!({"type":"user", "uuid":uuid::Uuid::new_v4(), "parentUuid":null,
                "sessionId":id.as_uuid(), "message":{"role":"user", "content":"keep"}}),
        ).unwrap();
        drop(file);
        let before = std::fs::read(&path).unwrap();
        let source = ScheduledSource {
            id,
            path: path.clone(),
            mode: ConversationMode::Code,
            scope: "project".into(),
            existing: true,
        };
        assert!(capture_target_source(&home, &source)
            .err()
            .unwrap()
            .contains("malformed"));
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[tokio::test]
    async fn target_replay_waits_for_writer_ownership_and_keeps_the_latest_parent() {
        let temp = tempfile::tempdir().unwrap();
        let config = DesktopConfig {
            idle_compaction_enabled: false,
            cwd: temp.path().join("project"),
            lingxi_home: temp.path().join("home"),
            ..DesktopConfig::default()
        };
        std::fs::create_dir_all(&config.cwd).unwrap();
        let id = lingxi_core::types::SessionId::new();
        let path = orchestrator::transcript_paths::main_transcript_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &id.as_uuid().to_string(),
        );
        let source = ScheduledSource {
            id,
            path: path.clone(),
            mode: ConversationMode::Code,
            scope: "project".into(),
            existing: true,
        };
        let writers = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
            config.lingxi_home.join("sessions"),
        );
        let foreground = writers
            .claim_session_id(&id.as_uuid().to_string(), std::process::id())
            .unwrap();
        // No transcript exists yet: a premature read would report unavailable
        // instead of busy while the foreground owns this session.
        let error = claim_and_capture_target(&config, &source)
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("busy:"), "{error}");
        let message_id = lingxi_core::types::SessionId::new().as_uuid();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(),
                    "sessionMode":"code", "instructionEpoch":0}),
                serde_json::json!({
                    "type": "user", "uuid": message_id, "parentUuid": null,
                    "sessionId": id.as_uuid(), "timestamp": "2026-09-12T12:00:00Z",
                    "cwd": config.cwd, "version": "0.6.0", "isSidechain": false,
                    "message": {"role": "user", "content": "latest foreground message"}
                })
            ),
        )
        .unwrap();
        drop(foreground);
        let (lease, captured) = claim_and_capture_target(&config, &source).await.unwrap();
        let captured = captured.unwrap();
        let loaded = session::jsonl::route_lines(&captured.content);
        let (messages, _) =
            session::jsonl::build_conversation_chain(&loaded, &id.as_uuid().to_string());
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].uuid, message_id.to_string());
        assert!(captured.content.contains("latest foreground message"));
        assert!(writers
            .claim_session_id(&id.as_uuid().to_string(), std::process::id())
            .is_err());
        // This is the construction field consumed by build_with_credential_stack.
        let mut construction = config;
        construction.session_writer_lease = Some(lease);
        assert!(writers
            .claim_session_id(&id.as_uuid().to_string(), std::process::id())
            .is_err());
        drop(construction);
        assert!(writers
            .claim_session_id(&id.as_uuid().to_string(), std::process::id())
            .is_ok());
    }

    #[tokio::test]
    async fn controller_ownership_never_temporarily_attaches_a_native_firer() {
        let mut config = DesktopConfig::default();
        assert!(should_start_native_scheduler(&config));
        HOST_AUTOMATION
            .scope(true, async {
                assert!(!should_start_native_scheduler(&config));
            })
            .await;
        assert!(should_start_native_scheduler(&config));
        CHILD_RUNTIME
            .scope(true, async {
                assert!(!should_start_native_scheduler(&config));
            })
            .await;
        config.enable_automation_scheduler = false;
        assert!(!should_start_native_scheduler(&config));
    }
}

#[cfg(test)]
mod supervision_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn cancelled_caller_retains_supervisor_until_child_cleanup_finishes() {
        let supervisor = Arc::new(NativeRunSupervisor::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let cleaning = Arc::new(tokio::sync::Notify::new());
        let cleaned = Arc::new(AtomicBool::new(false));
        let (release, cleanup_released) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn({
            let supervisor = supervisor.clone();
            let started = started.clone();
            let cleaning = cleaning.clone();
            let cleaned = cleaned.clone();
            async move {
                supervisor
                    .run("cancelled-run", move |cancel, _status| async move {
                        started.notify_one();
                        cancel.cancelled().await;
                        cleaning.notify_one();
                        cleanup_released.await.unwrap();
                        cleaned.store(true, Ordering::SeqCst);
                        Err("cancelled:Scheduled run cancelled".into())
                    })
                    .await
            }
        });
        started.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        cleaning.notified().await;
        assert!(supervisor
            .cancel_run("cancelled-run")
            .await
            .unwrap_err()
            .contains("cleanup"));
        assert!(!cleaned.load(Ordering::SeqCst));
        assert!(supervisor
            .runs
            .lock()
            .unwrap()
            .contains_key("cancelled-run"));
        release.send(()).unwrap();
        supervisor.cancel_run("cancelled-run").await.unwrap();
        assert!(cleaned.load(Ordering::SeqCst));
        assert!(supervisor.runs.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_shutdown_is_retried_before_a_success_result_is_released() {
        let supervisor = Arc::new(NativeRunSupervisor::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let retrying = Arc::new(tokio::sync::Notify::new());
        let allowed = Arc::new(AtomicBool::new(false));
        let caller = tokio::spawn({
            let supervisor = supervisor.clone();
            let attempts = attempts.clone();
            let retrying = retrying.clone();
            let allowed = allowed.clone();
            async move {
                supervisor
                    .run("cleanup-retry", move |_cancel, status| async move {
                        drain_supervised_runtime(&status, || async {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            if allowed.load(Ordering::SeqCst) {
                                crate::desktop::DesktopSessionShutdownReport {
                                    complete: true,
                                    ..Default::default()
                                }
                            } else {
                                retrying.notify_one();
                                crate::desktop::DesktopSessionShutdownReport {
                                    complete: false,
                                    errors: vec!["worker is still shutting down".into()],
                                    ..Default::default()
                                }
                            }
                        })
                        .await;
                        Ok(cron::AutomationRunResult {
                            session_id: "session".into(),
                            session_transcript_path: None,
                            session_mode: None,
                            session_scope_key: None,
                            summary: "done".into(),
                        })
                    })
                    .await
            }
        });
        retrying.notified().await;
        assert!(!caller.is_finished());
        let status = supervisor
            .runs
            .lock()
            .unwrap()
            .get("cleanup-retry")
            .unwrap()
            .clone();
        assert!(status
            .cleanup_error
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("worker"));
        allowed.store(true, Ordering::SeqCst);
        assert_eq!(caller.await.unwrap().unwrap().summary, "done");
        assert!(attempts.load(Ordering::SeqCst) >= 2);
        assert!(supervisor.runs.lock().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_supervisor_reaps_its_owned_child_process_before_joining() {
        let supervisor = Arc::new(NativeRunSupervisor::new());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (cleanup_tx, cleanup_rx) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn({
            let supervisor = supervisor.clone();
            async move {
                supervisor
                    .run("process-cleanup", move |cancel, _status| async move {
                        let mut child = tokio::process::Command::new("sh")
                            .args(["-c", "exec sleep 60"])
                            .kill_on_drop(true)
                            .spawn()
                            .unwrap();
                        started_tx.send(child.id().unwrap()).unwrap();
                        cancel.cancelled().await;
                        cleanup_rx.await.unwrap();
                        child.kill().await.unwrap();
                        child.wait().await.unwrap();
                        Err("cancelled:Scheduled run cancelled".into())
                    })
                    .await
            }
        });
        let pid = started_rx.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(
            cron::scheduler::pid_alive_check(pid),
            "supervisor must retain the child until cleanup owns its termination"
        );
        cleanup_tx.send(()).unwrap();
        supervisor.cancel_run("process-cleanup").await.unwrap();
        assert!(
            !cron::scheduler::pid_alive_check(pid),
            "successful cleanup join must prove the owned process exited"
        );
    }
    #[test]
    fn permanent_child_build_failures_pause_instead_of_retrying_as_busy() {
        for error in [
            crate::desktop::BuildError::SandboxUnavailable("required sandbox is missing".into()),
            crate::desktop::BuildError::ApiBase("invalid provider URL".into()),
            crate::desktop::BuildError::InvalidCustomBetas,
        ] {
            let message = child_build_error(error);
            assert!(message.starts_with("paused:"), "{message}");
            assert!(!message.starts_with("busy:"));
        }
    }
    #[tokio::test]
    async fn invalid_session_storage_is_not_misclassified_as_writer_contention() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("not-a-directory");
        std::fs::write(&home, "file").unwrap();
        let config = DesktopConfig {
            idle_compaction_enabled: false,
            lingxi_home: home,
            cwd: temp.path().to_path_buf(),
            ..Default::default()
        };
        let source = ScheduledSource {
            id: lingxi_core::types::SessionId::new(),
            path: temp.path().join("new.jsonl"),
            mode: ConversationMode::Code,
            scope: "project".into(),
            existing: false,
        };
        let error = claim_and_capture_target(&config, &source)
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("paused:"), "{error}");
    }
}
