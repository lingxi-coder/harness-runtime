//! CLI session-aware scheduler adapter. Sessions retain their ordinary writer lease.
use crate::desktop::{ConversationOrchestrator, DesktopConfig};
use async_trait::async_trait;
use futures::FutureExt as _;
use lingxi_core::host::OrchestratorHandle;
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
    async fn emit_text(&self, _: &str) {}
    async fn emit_end_turn(&self, _: &str, _: &lingxi_core::host::CostSnapshot) {}
    async fn emit_tool_call(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &serde_json::Value,
    ) {
    }
    async fn emit_tool_result(
        &self,
        _: &lingxi_core::types::ToolUseId,
        _: &str,
        _: &str,
        _: &serde_json::Value,
    ) {
    }
}
#[async_trait]
impl cron::CronJobFirer for NativeCronFirer {
    async fn fire(&self, _: &str, _: &str) -> Result<String, String> {
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
        use cron::automation::RunMode;
        let automation = request
            .task
            .automation
            .as_ref()
            .ok_or("Missing automation settings")?;
        let target = match automation.run_mode {
            RunMode::NewSession => None,
            RunMode::SelectedSession => Some(
                automation
                    .target_session_id
                    .as_deref()
                    .ok_or("paused:Choose a target session")?,
            ),
            RunMode::TaskSession => automation.owned_session_id.as_deref(),
        };
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
        if let Some(target) = target {
            if lingxi_core::types::SessionId::parse_prefixed(target)
                == Some(owner.current_session_id().await)
            {
                let fs = super::platform::DesktopFileSystem::new(self.config.cwd.clone());
                let expected = lingxi_core::types::SessionId::parse_prefixed(target)
                    .ok_or("paused:Invalid session ID")?;
                let (outcome, captured_session, summary) = owner
                    .run_scheduled_turn_in_session(
                        expected,
                        &request.task.prompt,
                        &automation.model,
                        reasoning,
                        cancel.clone(),
                        cron::automation::bind_automation_run_session(
                            &fs,
                            &self.config.cwd,
                            request,
                            target,
                        ),
                    )
                    .await?;
                validate_outcome(outcome)?;
                debug_assert_eq!(captured_session, expected);
                return Ok(cron::automation::AutomationRunResult {
                    session_id: target.to_string(),
                    summary,
                });
            }
        }
        let id = match target {
            Some(id) => lingxi_core::types::SessionId::parse_prefixed(id)
                .ok_or("paused:Invalid session ID")?,
            None => lingxi_core::types::SessionId::new(),
        };
        let (lease, replayed) = claim_and_replay_target(&self.config, id, target.is_some()).await?;
        let mut cfg = self.config.clone();
        cfg.session_id_override = Some(id.as_uuid().to_string());
        cfg.session_writer_lease = Some(lease);
        cfg.session_persistence = true;
        cfg.session_agent_observer = None;
        cfg.parent_session_id = None;
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
            let fs = super::platform::DesktopFileSystem::new(self.config.cwd.clone());
            cron::automation::bind_automation_run_session(
                &fs,
                &self.config.cwd,
                request,
                &id.as_uuid().to_string(),
            )
            .await?;
            if let Some(replayed) = replayed {
                let snapshot = replayed.handle_runtime_snapshot();
                runtime
                    .orchestrator
                    .resume_session(
                        id,
                        replayed.state.history,
                        replayed.last_message_uuid.map(|id| id.to_string()),
                        None,
                        snapshot,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
            }
            let outcome = runtime
                .orchestrator
                .run_scheduled_turn(
                    &request.task.prompt,
                    &automation.model,
                    reasoning,
                    cancel.clone(),
                )
                .await?;
            finish(&runtime.orchestrator, outcome).await
        })
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err("Scheduled execution panicked".into()));
        drain_supervised_runtime(&status, || runtime.session_lifecycle.shutdown_and_drain()).await;
        execution
    }
}
fn child_build_error(error: crate::desktop::BuildError) -> String {
    // Writer contention is classified by claim_and_replay_target before build.
    // A failed child configuration requires repair, not an unbounded busy retry.
    format!("paused:Cannot initialize scheduled runtime: {error}")
}

async fn claim_and_replay_target(
    config: &DesktopConfig,
    id: lingxi_core::types::SessionId,
    resume: bool,
) -> Result<
    (
        lingxi_core::host::live_sessions::SharedSessionWriterLease,
        Option<orchestrator::resume::ReplayedSession>,
    ),
    String,
> {
    // The same lease protects both the history snapshot and the subsequent
    // runtime. Reading before claiming would let a foreground writer append
    // between replay and construction, leaving the scheduled turn on an old parent.
    let lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
        config.lingxi_home.join("sessions"),
    )
    .claim_session_id(&id.as_uuid().to_string(), std::process::id())
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
    let replayed = if resume {
        let fs = Arc::new(super::platform::DesktopFileSystem::new(config.cwd.clone()));
        Some(
            orchestrator::replay_session_state(
                &config.lingxi_home,
                &config.cwd.to_string_lossy(),
                id.as_uuid(),
                fs,
            )
            .await
            .map_err(|error| format!("paused:Target session is unavailable: {error}"))?,
        )
    } else {
        None
    };
    Ok((lease, replayed))
}

async fn finish(
    orch: &ConversationOrchestrator,
    outcome: orchestrator::conversation::TurnOutcome,
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
        session_id: orch.current_session_id().await.as_uuid().to_string(),
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
    #[tokio::test]
    async fn target_replay_waits_for_writer_ownership_and_keeps_the_latest_parent() {
        let temp = tempfile::tempdir().unwrap();
        let config = DesktopConfig {
            cwd: temp.path().join("project"),
            lingxi_home: temp.path().join("home"),
            ..DesktopConfig::default()
        };
        std::fs::create_dir_all(&config.cwd).unwrap();
        let id = lingxi_core::types::SessionId::new();
        let writers = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
            config.lingxi_home.join("sessions"),
        );
        let foreground = writers
            .claim_session_id(&id.as_uuid().to_string(), std::process::id())
            .unwrap();
        // No transcript exists yet: a premature read would report unavailable
        // instead of busy while the foreground owns this session.
        let error = claim_and_replay_target(&config, id, true)
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("busy:"), "{error}");
        let message_id = lingxi_core::types::SessionId::new().as_uuid();
        let path = orchestrator::transcript_paths::main_transcript_path(
            &config.lingxi_home,
            &config.cwd.to_string_lossy(),
            &id.as_uuid().to_string(),
        );
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{}\n",
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
        let (lease, replayed) = claim_and_replay_target(&config, id, true).await.unwrap();
        let replayed = replayed.unwrap();
        assert_eq!(replayed.last_message_uuid, Some(message_id));
        assert_eq!(replayed.state.history.len(), 1);
        assert_eq!(
            replayed.state.history[0].text_content(),
            "latest foreground message"
        );
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
            lingxi_home: home,
            cwd: temp.path().to_path_buf(),
            ..Default::default()
        };
        let error = claim_and_replay_target(&config, lingxi_core::types::SessionId::new(), false)
            .await
            .err()
            .unwrap();
        assert!(error.starts_with("paused:"), "{error}");
    }
}
