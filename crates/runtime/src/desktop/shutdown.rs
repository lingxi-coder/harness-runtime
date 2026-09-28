use async_trait::async_trait;
use command_api::CommandRegistry;
use orchestrator::ConversationOrchestrator;
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::{file_changed_watch, fusion_recorder, session_state, settings_watch};

/// Total wall-clock budget for one host shutdown barrier.
///
/// Every desktop exit path awaits `shutdown_and_drain`, and the electron host
/// hard-kills the engine after its own grace period. A barrier that outlives
/// that grace turns an orderly drain into a kill in the middle of settlement,
/// which is the outcome the barrier exists to prevent. Eight seconds leaves
/// headroom under a ten-second grace.
pub const DESKTOP_SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// Ordered owner of host-shutdown persistence barriers. Clones keep the exact
/// task registry, cost ledger, per-session coordinators, and outbox delivery
/// locks alive until accepted work is settled.
pub(super) struct ProcessSessionActivationObserver;

#[async_trait]
impl orchestrator::conversation::SessionActivationObserver for ProcessSessionActivationObserver {
    async fn session_activated(
        &self,
        previous: protocol::SessionId,
        current: protocol::SessionId,
    ) -> Result<(), String> {
        refresh_process_session_presence(previous, current).await
    }
}

/// Rebind this host process's UDS generation and live discovery record after
/// the owned orchestrator commit. The conversation switch is already durable;
/// failures are returned as post-commit warnings and stale socket fields are
/// removed rather than advertising an endpoint under the wrong session.
pub async fn refresh_process_session_presence(
    previous: protocol::SessionId,
    current: protocol::SessionId,
) -> Result<(), String> {
    if previous == current {
        return Ok(());
    }
    let Some(dir) = platform_api::live_sessions::process_dir() else {
        return Ok(());
    };
    let pid = std::process::id();
    let current_text = current.as_uuid().to_string();
    let observed = platform_api::live_sessions::process_session_id()
        .as_deref()
        .and_then(protocol::SessionId::parse_prefixed)
        .ok_or_else(|| "live process has no valid scoped session identity".to_string())?;
    if observed != previous && observed != current {
        return Err(format!(
            "stale session activation {previous} -> {current} cannot replace live session {observed}"
        ));
    }
    let already_current = observed == current;
    let socket = if already_current {
        platform_api::uds_inbox::process_socket_path()
    } else {
        let inbox_session = current_text.clone();
        match tokio::task::spawn_blocking(move || {
            platform_api::uds_inbox::retarget_process_inbox(&inbox_session)
        })
        .await
        {
            Ok(Ok(path)) => Some(path),
            Ok(Err(error)) => {
                platform_api::live_sessions::set_process_session_id(&current_text);
                let _ = dir.upsert_identity(
                    pid,
                    &current_text,
                    platform_api::live_sessions::process_name().as_deref(),
                    None,
                    None,
                    platform_api::live_sessions::process_permission_class().as_deref(),
                );
                let _ = dir.clear_messaging_socket_if_session(pid, &current_text);
                let _ = dir.clear_messaging_socket_if_session(pid, &previous.to_string());
                return Err(format!("cross-session inbox is unavailable: {error}"));
            }
            Err(error) => {
                platform_api::live_sessions::set_process_session_id(&current_text);
                let _ = dir.upsert_identity(
                    pid,
                    &current_text,
                    platform_api::live_sessions::process_name().as_deref(),
                    None,
                    None,
                    platform_api::live_sessions::process_permission_class().as_deref(),
                );
                let _ = dir.clear_messaging_socket_if_session(pid, &current_text);
                let _ = dir.clear_messaging_socket_if_session(pid, &previous.to_string());
                return Err(format!("cross-session inbox task failed: {error}"));
            }
        }
    };
    platform_api::live_sessions::set_process_session_id(&current_text);
    if let Err(error) = dir.upsert_identity(
        pid,
        &current_text,
        platform_api::live_sessions::process_name().as_deref(),
        None,
        socket.as_deref(),
        platform_api::live_sessions::process_permission_class().as_deref(),
    ) {
        let _ = dir.clear_messaging_socket_if_session(pid, &previous.to_string());
        return Err(format!(
            "live-session identity could not be updated: {error}"
        ));
    }
    if socket.is_none() {
        dir.clear_messaging_socket_if_session(pid, &current_text)
            .map_err(|error| format!("stale messaging presence could not be cleared: {error}"))?;
    }
    Ok(())
}

pub struct DesktopSessionLifecycle {
    pub(super) settings_watcher: settings_watch::SettingsWatcherHandle,
    pub(super) file_changed_watcher: file_changed_watch::FileChangedWatcherHandle,
    pub cron_scheduler: Option<Arc<cron::CronScheduler>>,
    pub(super) orchestrator: Arc<ConversationOrchestrator>,
    pub(super) mcp_reconnect_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) mcp_catalog_refresh_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(super) task_registry: Arc<tasks::registry::TaskRegistry>,
    pub(super) command_registry: Arc<RwLock<CommandRegistry>>,
    pub(super) subagent_spawner: Arc<agent::handle::PoolSubagentSpawner>,
    pub(super) fusion_api_service: Arc<llm_runtime::ApiService>,
    pub(super) cost_tracker: Arc<cost::CostTracker>,
    pub(super) session_state_manager: Arc<session_state::SessionStateManager>,
    pub(super) fusion_recorder_factory: Arc<fusion_recorder::DesktopFusionRecorderFactory>,
    pub(super) fusion_recovery_task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Disposable ledger root created for a `--no-session-persistence` host.
    /// Removed once every queue is closed, so the promise of leaving nothing
    /// behind holds for the spend ledger as well as the transcript.
    pub(super) ephemeral_home: Option<std::path::PathBuf>,
}

/// Truthful result of a host shutdown drain. Independent producer barriers are
/// attempted on failure, but live-worker dependencies remain owned for retry.
#[derive(Debug, Default)]
pub struct DesktopSessionShutdownReport {
    /// True only after every barrier succeeded. An incomplete shutdown must
    /// not be followed by a whole-runtime session remount.
    pub complete: bool,
    /// Persistence/producer failures observed after all possible drains ran.
    pub errors: Vec<String>,
    /// Final durable publication states for every known Slash outbox.
    pub publications: Vec<platform_api::FusionPublicationReceipt>,
}

impl DesktopSessionLifecycle {
    /// Stop producers, settle known provider usage, fence session queues, drain
    /// retained outbox I/O, then fence late acknowledgements. Filesystem work
    /// already in progress is never cancelled merely because a UI waiter left.
    /// The host must await this barrier to completion without racing another
    /// shutdown call; an incomplete report may then be retried sequentially.
    pub async fn shutdown_and_drain(&self) -> DesktopSessionShutdownReport {
        let mut report = DesktopSessionShutdownReport::default();
        let mut producers_drained = true;
        let started = tokio::time::Instant::now();
        let deadline = started + DESKTOP_SHUTDOWN_BUDGET;
        // Per-stage allowance, further clipped by the overall deadline. A
        // stage that overruns is abandoned, not aborted: its detached owner
        // still holds its own guard and settles on its own.
        let stage =
            |budget: std::time::Duration| deadline.min(tokio::time::Instant::now() + budget);
        // Both watcher families can fire hooks that retain the orchestrator or
        // create children. Join their actual jobs before draining consumers.
        if tokio::time::timeout_at(stage(std::time::Duration::from_millis(500)), async {
            tokio::join!(
                self.settings_watcher.shutdown_and_drain(),
                self.file_changed_watcher.shutdown_and_drain(),
            );
        })
        .await
        .is_err()
        {
            producers_drained = false;
            report
                .errors
                .push("watchers did not drain within the shutdown budget".into());
        }
        if let Some(scheduler) = self.cron_scheduler.as_ref() {
            match tokio::time::timeout_at(
                stage(std::time::Duration::from_secs(2)),
                scheduler.stop(),
            )
            .await
            {
                Ok(Err(error)) => {
                    producers_drained = false;
                    report
                        .errors
                        .push(format!("cron scheduler shutdown failed: {error}"));
                }
                Err(_) => {
                    producers_drained = false;
                    report
                        .errors
                        .push("cron scheduler did not stop within the shutdown budget".into());
                }
                Ok(Ok(())) => {}
            }
        }
        if let Some(reconnect) = self.mcp_reconnect_task.lock().await.take() {
            reconnect.abort();
            let _ = reconnect.await;
        }
        if let Some(refresh) = self.mcp_catalog_refresh_task.lock().await.take() {
            refresh.abort();
            let _ = refresh.await;
        }
        match tokio::time::timeout_at(
            stage(std::time::Duration::from_secs(2)),
            self.task_registry.shutdown_background_tasks(),
        )
        .await
        {
            Ok(Err(error)) => {
                producers_drained = false;
                report.errors.push(format!("task shutdown failed: {error}"));
            }
            Err(_) => {
                producers_drained = false;
                report
                    .errors
                    .push("background tasks did not drain within the shutdown budget".into());
            }
            Ok(Ok(())) => {}
        }
        report.errors.extend(
            self.orchestrator
                .close_and_drain_session_switches()
                .await
                .into_iter()
                .map(|error| format!("session switch failed: {error}")),
        );
        if !producers_drained {
            // A failed handler drain cannot prove its worker stopped. Preserve
            // commands, pool backedges and open coordinators so it can finish
            // and this lifecycle can be retried. Already-observed charges may
            // still be settled safely; no queue or writer claim is closed here.
            if let Err(error) = self.cost_tracker.drain_owned_settlements().await {
                report
                    .errors
                    .push(format!("cost settlement failed: {error}"));
            }
            return report;
        }
        let retired_command_handlers =
            { self.command_registry.write().await.take_builtin_handlers() };
        // Handler destructors can release the orchestrator, whose skill-listing
        // provider owns this same registry. Run that graph teardown only after
        // the write guard above has been released.
        drop(retired_command_handlers);
        // The task registry has drained every handler-owned child and the
        // session-switch supervisor has joined every accepted mount. Nothing
        // can legitimately start another child now, so sever the pool's four
        // construction-time backedges (tools/hooks/skills/MCP builder). The
        // set-once latches remain closed and cannot be resurrected by a late
        // producer.
        self.subagent_spawner.release_runtime_links();
        if let Err(error) = self.cost_tracker.drain_owned_settlements().await {
            report
                .errors
                .push(format!("cost settlement failed: {error}"));
        }
        if let Err(error) = self.session_state_manager.flush_all().await {
            report
                .errors
                .push(format!("session queue flush failed: {error}"));
        }
        if let Some(recovery) = self.fusion_recovery_task.lock().await.take() {
            if let Err(error) = recovery.await {
                report
                    .errors
                    .push(format!("Fusion startup recovery task failed: {error}"));
            }
        }
        report.publications = self
            .fusion_recorder_factory
            .drain_pending_all(stage(std::time::Duration::from_secs(2)))
            .await;
        for receipt in &report.publications {
            if !matches!(
                receipt.status,
                platform_api::FusionPublicationStatus::Published
                    | platform_api::FusionPublicationStatus::Queued
            ) {
                report.errors.push(
                    receipt
                        .error
                        .clone()
                        .unwrap_or_else(|| "Fusion publication remains queued".into()),
                );
            }
        }
        if let Err(error) = self.session_state_manager.flush_all().await {
            report
                .errors
                .push(format!("late session queue flush failed: {error}"));
        }
        if let Err(error) = self.session_state_manager.close_and_drain().await {
            report
                .errors
                .push(format!("session state shutdown failed: {error}"));
        }
        if let Some(home) = self.ephemeral_home.as_ref() {
            // Every queue and claim above is closed by now. A failure here is
            // reported rather than swallowed: leftover state under the OS temp
            // root is exactly what this host promised not to leave.
            if let Err(error) = std::fs::remove_dir_all(home) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    report
                        .errors
                        .push(format!("ephemeral ledger cleanup failed: {error}"));
                }
            }
        }
        report.complete = report.errors.is_empty();
        if report.complete {
            // A service may outlive this session through another API consumer.
            // Its registered-attempt host must not keep the closed writer claim
            // alive. Failed producer/receipt drains retain authority for retry.
            self.fusion_api_service.clear_model_attempt_hooks();
        }
        report
    }
}

/// Shared CLI/bridge factory for the independent shell supervisor. Its writer
/// uses the same rooted spool, framing, caps and flush boundary as live tasks.
#[cfg(any(unix, windows))]
pub fn supervisor_exit_sink(
    path: &std::path::Path,
) -> std::sync::Arc<dyn platform_api::BackgroundExitSink> {
    let root = path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("/"))
        .to_path_buf();
    #[cfg(unix)]
    let fs = std::sync::Arc::new(platform_posix::PosixFileSystem::new(root.clone()));
    #[cfg(windows)]
    let fs = std::sync::Arc::new(platform_windows::WindowsFileSystem::new(root.clone()));
    let manager = std::sync::Arc::new(tasks::output_manager::TaskOutputManager::new(root, fs));
    std::sync::Arc::new(tasks::output_manager::TaskOutputSink::new(
        manager,
        path.to_path_buf(),
    ))
}
