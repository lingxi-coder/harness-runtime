use super::restoration::canonical_cwd_string;
use async_trait::async_trait;
use client::adapter::{ClientEventListener, PermissionRequestSink};
use client::protocol::events::ClientEvent;
use client::protocol::permission::{
    PermissionKindDto, PermissionRequest as PermissionRequestDto, PermissionResponseDto,
};
use lingxi_core::host::{Clock, FileSystem, OrchestratorHandle, Platform};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use super::{build_mobile_inner, MobileConfig, MobileEngineError, MobileEngineHandle};

/// Per-job wall-clock budget for a fired cron turn. This remains a second
/// safety limit beneath WorkManager's outer lifecycle budget.
pub(super) const CRON_TURN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Terminal status of one fired cron job, lowered for the foreign host.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Debug, Clone)]
pub enum CronFireStatusDto {
    /// The job's turn completed.
    Ok,
    /// The job's turn failed (or timed out); carries a log-safe message.
    Failed {
        /// Human-readable failure detail.
        message: String,
    },
}

/// One fired-job record the Android service turns into a result notification.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone)]
pub struct FiredCronJobDto {
    /// Persisted result conversation, when execution created one.
    pub session_id: Option<String>,
    /// The cron job id that fired.
    pub id: String,
    /// The prompt that was run.
    pub prompt: String,
    /// The final assistant text, if the turn produced any.
    pub result_text: Option<String>,
    /// Terminal status.
    pub status: CronFireStatusDto,
    /// Whether the failure is safe to retry automatically (HTTP 429/5xx and
    /// transport failures, or a durable `busy:` queued occurrence). Successful
    /// runs always report `false`.
    pub retryable: bool,
}

/// A persisted cron job lowered for the Android management UI.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone)]
pub struct CronTaskDto {
    /// Versioned automation settings, including retained run history.
    pub automation_json: Option<String>,
    /// Stable 9-char job id.
    pub id: String,
    /// 5-field cron expression (local time).
    pub cron: String,
    /// Prompt run at each fire.
    pub prompt: String,
    /// Creation time, epoch milliseconds.
    pub created_at_ms: u64,
    /// Last fire time, epoch milliseconds (absent until the job first fires).
    pub last_fired_at_ms: Option<u64>,
    /// `true` = recurring; `false` = one-shot.
    pub recurring: bool,
    /// Next fire, epoch milliseconds (absent for an impossible expression).
    pub next_fire_ms: Option<u64>,
    /// Human-readable schedule (e.g. "every day at 9:00am").
    pub human: String,
    /// Whether this device may schedule this task (iOS and Android). Recurring schedules must have a
    /// minimum interval of 15 minutes; one-shot schedules are exempt.
    pub mobile_supported: bool,
    /// Stable explanation when [`Self::mobile_supported`] is false.
    pub unsupported_reason: Option<String>,
}

/// One due task occurrence lowered for WorkManager dispatch.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronDueOccurrenceDto {
    /// Stable persisted task id.
    pub task_id: String,
    /// The task's computed (possibly missed) fire instant in epoch milliseconds.
    pub scheduled_at_ms: u64,
}

fn source_display(prompt: &Utf16JsonProjection) -> String {
    String::from_utf16_lossy(&prompt.string_units("").unwrap_or_default())
}

/// A discarding [`ClientEventListener`] that captures only assistant text, so a
/// headless cron turn's final message can be surfaced in a notification without
/// streaming anything to the user's live UI.
pub(super) struct CapturingListener {
    pub(super) text: Arc<Mutex<String>>,
}

#[async_trait]
impl ClientEventListener for CapturingListener {
    async fn on_workflow_progress(
        &self,
        _: String,
        _: String,
        _: String,
        _: client::protocol::listings::WorkflowProgressDto,
    ) {
    }
    async fn on_event(&self, event: ClientEvent) {
        if let ClientEvent::TextDelta { text } = event {
            self.text.lock().await.push_str(&text.display());
        }
    }
}

/// A headless cron turn has no foreground answerer. Requests are forwarded to a
/// local channel and resolved `Deny` immediately, so a background job never
/// parks for the normal five-minute interactive timeout. Existing durable
/// allow-rules still short-circuit before a request is emitted.
pub(super) struct ImmediateDenyPermissionSink {
    pub(super) sender: mpsc::UnboundedSender<PermissionRequestDto>,
}

#[async_trait]
impl PermissionRequestSink for ImmediateDenyPermissionSink {
    async fn emit_request(&self, request: PermissionRequestDto) {
        let _ = self.sender.send(request);
    }
}

pub(super) static MOBILE_CRON_HANDLES: std::sync::OnceLock<
    std::sync::Mutex<Vec<std::sync::Weak<MobileEngineHandle>>>,
> = std::sync::OnceLock::new();

pub(super) type MobileCronSessionGates = std::collections::HashMap<String, Arc<Mutex<()>>>;

pub(super) static MOBILE_CRON_SESSION_GATES: std::sync::OnceLock<
    std::sync::Mutex<MobileCronSessionGates>,
> = std::sync::OnceLock::new();

pub(super) fn mobile_cron_session_gate(cwd: &str, id: &str) -> Arc<Mutex<()>> {
    MOBILE_CRON_SESSION_GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(format!("{cwd}\0{id}"))
        .or_default()
        .clone()
}

/// The foreign caller can drop its future on a platform timeout. Keep result
/// persistence in an owned task, while cancellation drops the actual execution
/// before marking that same occurrence terminal.
pub(super) fn mobile_automation_runtime() -> &'static tokio::runtime::Runtime {
    try_mobile_automation_runtime().unwrap_or_else(|error| panic!("{error}"))
}

/// Process-lifetime executor for teardown/recovery tasks that must survive an
/// engine runtime's destruction. The construction error is cached so hosts can
/// refuse to create an attachment before exposing an engine that could not be
/// torn down safely.
pub(super) fn try_mobile_automation_runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    static RUNTIME: std::sync::OnceLock<Result<tokio::runtime::Runtime, String>> =
        std::sync::OnceLock::new();
    match RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("mobile-cron")
            .enable_all()
            .build()
            .map_err(|error| format!("build mobile scheduled execution runtime: {error}"))
    }) {
        Ok(runtime) => Ok(runtime),
        Err(error) => Err(error.clone()),
    }
}

pub(super) async fn supervise_mobile_automation<F>(
    fs: Arc<dyn FileSystem>,
    cwd: std::path::PathBuf,
    clock: Arc<dyn Clock>,
    claim: impl std::future::Future<Output = Option<cron::AutomationRunRequest>> + Send + 'static,
    execute: impl FnOnce(cron::AutomationRunRequest) -> F + Send + 'static,
) -> Option<FiredCronJobDto>
where
    F: std::future::Future<Output = Result<cron::AutomationRunResult, String>> + Send + 'static,
{
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    mobile_automation_runtime().spawn(async move {
        // A filesystem implementation may commit after an awaiting caller is
        // cancelled. Never drop the claim future: settle it, then cancel the
        // returned occurrence before any model execution begins.
        let request = claim.await?;
        let execution = execute(request.clone());
        let result = {
            // The losing execution future is destroyed before durable completion.
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err("cancelled: Scheduled execution was cancelled by the host".into()),
                result = execution => result,
            }
        };
        let now = clock.now().duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default().as_millis() as u64;
        let mut persistence_backoff = std::time::Duration::from_millis(100);
        // Bounded: `finish_automation_run_checked` has deterministic failures
        // (an unparseable tasks file, a read-only volume) that no amount of
        // retrying repairs, and the FFI entry points await this task — an
        // unbounded loop hangs `runCronTaskNow` on the client forever.
        const PERSISTENCE_ATTEMPTS: usize = 8;
        let mut disposition = None;
        for attempt in 0..PERSISTENCE_ATTEMPTS {
            match cron::finish_automation_run_checked(fs.as_ref(), &cwd, &request, &result, now).await {
                Ok(value) => {
                    disposition = Some(value);
                    break;
                }
                Err(error) => {
                    // Retain the captured outcome on this independent runtime.
                    // Replaying the model to repair an I/O failure would repeat
                    // its tool side effects; retry only the durable merge.
                    tracing::warn!(run_id = %request.run_id, %error, attempt, "mobile cron result persistence failed; retrying saved outcome");
                    tokio::select! {
                        biased;
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(persistence_backoff) => {}
                    }
                    persistence_backoff = (persistence_backoff * 2).min(std::time::Duration::from_secs(5));
                }
            }
        }
        let Some(disposition) = disposition else {
            tracing::error!(run_id = %request.run_id, "mobile cron result persistence gave up; the run stays claimed for recovery");
            return None;
        };
        let finished = matches!(disposition, cron::AutomationFinishDisposition::Terminal);
        let result_session_id = result.as_ref().ok().map(|value| value.session_id.clone());
        let persisted = read_cron_tasks(fs.as_ref(), &cwd).await.tasks.into_iter()
            .find(|task| task.id == request.task.id)
            .and_then(|task| task.automation)
            .and_then(|automation| automation.runs.into_iter().find(|run| run.id == request.run_id));
        let same_claim = persisted.as_ref().is_some_and(|run| run.claim_generation == Some(request.claim_generation));
        let queued = matches!(disposition, cron::AutomationFinishDisposition::Queued)
            && persisted.as_ref().is_none_or(|run| run.claim_generation == Some(request.claim_generation) && run.status == cron::AutomationRunStatus::Queued);
        // Binding may atomically cancel this exact claim before execution (for
        // example, pause or expiry). Completion then correctly returns false
        // because it must not overwrite that terminal record. Still surface
        // the durable cancellation instead of turning it into a skipped run.
        let committed_cancellation = !finished && same_claim
            && result.as_ref().err().is_some_and(|error| error.starts_with(cron::AUTOMATION_CANCELLED_PREFIX))
            && persisted.as_ref().is_some_and(|run| run.status == cron::AutomationRunStatus::Cancelled);
        if !finished && !queued && !committed_cancellation {
            return None;
        }
        let outcome = if committed_cancellation {
            Err(format!("{}{}", cron::AUTOMATION_CANCELLED_PREFIX, persisted.as_ref().and_then(|run| run.error.as_deref()).unwrap_or("Scheduled execution was cancelled")))
        } else { result.map(|value| value.summary) };
        let mut dto = fired_cron_dto(&request.task, outcome);
        dto.session_id = persisted.and_then(|run| run.session_id).or(result_session_id);
        // Busy is a durable pending occurrence, not a terminal failure to skip.
        // OR rather than assign: `fired_cron_dto` already set `retryable` from
        // `cron_failure_is_retryable` (HTTP 429/5xx, transport failures, turn
        // timeouts), and the client retry budgets read that bit. Overwriting it
        // with `queued` alone reported every transient terminal failure as
        // non-retryable, so Android's `runAttemptCount` retry never rescheduled.
        dto.retryable = dto.retryable || queued;
        Some(dto)
    }).await.ok().flatten()
}

/// Upgraded foreground engines own a Tokio Runtime. The last reference may
/// belong to this background reader after the UI releases its FFI object, so
/// it must never be destroyed on the supervisor's async worker. Wrap every
/// upgrade immediately, including nonmatching handles and early returns.
pub(super) struct MobileCronBorrow<T: Send + Sync + 'static>(pub(super) Option<Arc<T>>);

impl<T: Send + Sync + 'static> std::ops::Deref for MobileCronBorrow<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.0.as_deref().expect("live scheduled foreground borrow")
    }
}

impl<T: Send + Sync + 'static> Drop for MobileCronBorrow<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            drop(mobile_automation_runtime().spawn_blocking(move || drop(value)));
        }
    }
}

/// Invalidation must also happen when a caller drops an in-flight turn. This
/// guard is declared after the session lease, so readers are invalidated before
/// another foreground/background writer can acquire that lease.
pub(super) struct MobileCronTurnCleanup {
    pub(super) readers: Vec<MobileCronBorrow<MobileEngineHandle>>,
    pub(super) deny_requests: Option<tokio::task::JoinHandle<()>>,
    media_attachment: Option<ScheduledChildMedia>,
}

/// Keep host-side provider cleanup fenced behind the exact child's media drain.
pub(super) struct ScheduledChildMedia {
    state: Option<ScheduledChildMediaState>,
}

struct ScheduledChildMediaState {
    attachment: media_runtime::MediaAttachmentHandle,
    route: Option<Arc<dyn orchestrator::ScheduledMediaChildRoute>>,
    fence: Option<media_runtime::MediaAttachmentCleanupFence>,
}

impl ScheduledChildMediaState {
    async fn finish(&mut self, budget: std::time::Duration) -> Result<(), String> {
        self.attachment.begin_revoke();
        match self.attachment.drain_media(budget).await {
            media_runtime::MediaAttachmentRevocation::Confirmed => {}
            media_runtime::MediaAttachmentRevocation::Unknown { reason } => {
                return Err(format!(
                    "scheduled media child drain is unconfirmed: {reason}"
                ));
            }
        }
        if let Some(route) = &self.route {
            route.release_after_media_drain()?;
        }
        if let Some(fence) = &mut self.fence {
            fence.confirm().map_err(|error| error.to_string())?;
        }
        match self.attachment.revoke(budget).await {
            media_runtime::MediaAttachmentRevocation::Confirmed => Ok(()),
            media_runtime::MediaAttachmentRevocation::Unknown { reason } => Err(format!(
                "scheduled media child cleanup is unconfirmed: {reason}"
            )),
        }
    }
}

impl ScheduledChildMedia {
    fn new(
        attachment: media_runtime::MediaAttachmentHandle,
        route: Option<Arc<dyn orchestrator::ScheduledMediaChildRoute>>,
    ) -> Result<Self, String> {
        let fence = if route.is_some() {
            match attachment.retain_for_host_cleanup() {
                Ok(fence) => Some(fence),
                Err(error) => {
                    attachment.begin_revoke();
                    let retained = attachment.clone();
                    if let Err(spawn_error) = super::spawn_mobile_lifecycle_task(async move {
                        super::revoke_mobile_attachment(retained, None).await;
                    }) {
                        tracing::warn!(%spawn_error, "scheduled child cleanup could not start");
                    }
                    return Err(format!(
                        "scheduled media cleanup fence unavailable: {error}"
                    ));
                }
            }
        } else {
            None
        };
        Ok(Self {
            state: Some(ScheduledChildMediaState {
                attachment,
                route,
                fence,
            }),
        })
    }

    async fn finish(&mut self) -> Result<(), String> {
        if let Some(state) = &mut self.state {
            state.finish(crate::media_handoff::CLEANUP_BUDGET).await?;
        }
        self.state = None;
        Ok(())
    }
}

impl Drop for ScheduledChildMedia {
    fn drop(&mut self) {
        if let Some(mut state) = self.state.take() {
            state.attachment.begin_revoke();
            if let Err(error) = super::spawn_mobile_lifecycle_task(async move {
                // The shared drain has its own 30s deadline. A caller timeout
                // must not discard a late receipt or release an Unknown route.
                if let Err(error) = state.finish(std::time::Duration::from_secs(35)).await {
                    tracing::warn!(%error, "scheduled child media cleanup remains unconfirmed");
                }
            }) {
                tracing::warn!(%error, "scheduled child media cleanup could not start");
            }
        }
    }
}

impl Drop for MobileCronTurnCleanup {
    fn drop(&mut self) {
        if let Some(task) = &self.deny_requests {
            task.abort();
        }
        for reader in &self.readers {
            reader
                .scheduled_reload
                .store(true, std::sync::atomic::Ordering::Release);
        }
        // Dropping the child guard closes admission immediately and retains
        // its exact route until a confirmed drain receipt is observed.
        drop(self.media_attachment.take());
    }
}

pub(super) struct MobileTurnFirer {
    pub(super) cfg: MobileConfig,
    pub(super) platform: Arc<dyn Platform>,
}

#[async_trait]
impl cron::CronJobFirer for MobileTurnFirer {
    async fn fire(&self, _id: &str, prompt: &Utf16JsonProjection) -> Result<String, String> {
        self.fire_session(prompt.clone(), None, None)
            .await
            .map(|result| result.summary)
    }

    async fn fire_automation(
        &self,
        request: &cron::AutomationRunRequest,
    ) -> Result<cron::AutomationRunResult, String> {
        self.fire_session(
            request.task.prompt.clone(),
            request.task.automation.as_ref(),
            Some(request),
        )
        .await
    }
}

fn scheduled_parent_context_check(
    parent: &MobileCronBorrow<MobileEngineHandle>,
    parent_id: media_api::MediaAttachmentId,
    expected_session: String,
    expected_profile: String,
) -> orchestrator::ScheduledMediaParentCheck {
    let parent_weak = Arc::downgrade(parent.0.as_ref().expect("live parent"));
    Arc::new(move || {
        let Some(parent) = parent_weak.upgrade() else {
            return false;
        };
        // Dropping the final engine handle can block on its runtime. Keep that
        // destruction on the host's existing blocking lifecycle executor.
        let parent = MobileCronBorrow(Some(parent));
        let active = parent
            .inner
            .media_attachment_handle
            .as_ref()
            .is_some_and(|attachment| {
                attachment.id() == &parent_id && attachment.admission_is_open()
            });
        active
            && parent.inner.orchestrator.try_current_audio_binding().ok()
                == Some((expected_session.clone(), expected_profile.clone()))
    })
}

impl MobileTurnFirer {
    /// Build a transient scheduled runtime with its own host-issued media
    /// attachment. The child cannot borrow the parent's owner namespace or
    /// conversation adapter, and parent revocation cascades through the
    /// attachment lifecycle.
    pub(super) fn scheduled_runtime_config(
        &self,
    ) -> Result<(MobileConfig, Option<ScheduledChildMedia>), String> {
        let mut config = self.cfg.clone();
        config.media_attachment_confirmed_callback = None;
        // Realtime IO is engine-specific; only the separate host route factory
        // participates in scheduled child assembly.
        config.media_conversation_attachment = None;
        config.media_service = None;
        let mut cleanup = None;
        config.media_attachment_handle = match self.cfg.media_attachment_handle.as_ref() {
            Some(parent) => {
                let scheduled = self
                    .cfg
                    .media_conversation_attachment
                    .as_ref()
                    .map(|adapter| adapter.fork_scheduled_child(parent))
                    .transpose()?;
                let (child, route) = match scheduled.flatten() {
                    Some(scheduled) => (scheduled.attachment, Some(scheduled.route)),
                    None => (
                        parent.fork_child().map_err(|error| {
                            format!("scheduled media attachment unavailable: {error}")
                        })?,
                        None,
                    ),
                };
                cleanup = Some(ScheduledChildMedia::new(child.clone(), route)?);
                config.media_service = Some(child.service());
                Some(child)
            }
            // A raw facade has no trusted issuer/lifecycle. Keep legacy and
            // non-native scheduled runtimes fail-closed rather than sharing it.
            None => None,
        };
        Ok((config, cleanup))
    }

    /// Find the live engine issuing this exact parent attachment. Do not guess
    /// from a workspace, model name or a different engine's selected session.
    fn parent_engine(&self) -> Option<MobileCronBorrow<MobileEngineHandle>> {
        let parent_id = self.cfg.media_attachment_handle.as_ref()?.id();
        let handles: Vec<_> = MOBILE_CRON_HANDLES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .map(|handle| MobileCronBorrow(Some(handle)))
            .collect();
        let mut matching = handles.into_iter().filter(|handle| {
            handle
                .inner
                .media_attachment_handle
                .as_ref()
                .is_some_and(|attachment| attachment.id() == parent_id)
        });
        let parent = matching.next()?;
        if matching.next().is_some() {
            return None;
        }
        drop(matching);
        Some(parent)
    }

    pub(super) async fn fire_session(
        &self,
        prompt: Utf16JsonProjection,
        automation: Option<&cron::CronAutomation>,
        request: Option<&cron::AutomationRunRequest>,
    ) -> Result<cron::AutomationRunResult, String> {
        llm_runtime::catalog_task::scope(self.fire_session_scoped(prompt, automation, request))
            .await
    }

    async fn fire_session_scoped(
        &self,
        mut prompt: Utf16JsonProjection,
        automation: Option<&cron::CronAutomation>,
        request: Option<&cron::AutomationRunRequest>,
    ) -> Result<cron::AutomationRunResult, String> {
        let target = automation.and_then(|a| match a.run_mode {
            cron::RunMode::SelectedSession => a.target_session_id.as_deref(),
            cron::RunMode::TaskSession => a.owned_session_id.as_deref(),
            cron::RunMode::NewSession => None,
        });
        if automation.is_some_and(|a| a.run_mode == cron::RunMode::SelectedSession)
            && target.is_none()
        {
            return Err("paused: Select a conversation".into());
        }
        let store_cwd = self
            .cfg
            .logical_cwd
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|| canonical_cwd_string(&self.cfg.cwd));
        let target_uuid = target
            .map(|id| uuid::Uuid::parse_str(id.strip_prefix("sess:").unwrap_or(id)))
            .transpose()
            .map_err(|e| format!("paused: Invalid session: {e}"))?;
        let workspace = if automation.is_some() {
            super::scheduled_workspaces::ScheduledWorkspace::prepare(
                &self.cfg,
                request.map(|r| r.task.id.as_str()).unwrap_or("scheduled"),
                automation.and_then(|a| a.name.as_deref()).unwrap_or("task"),
                target_uuid,
            )
            .map_err(|error| format!("paused: {error}"))?
        } else {
            None
        };
        let cwd = workspace
            .as_ref()
            .map(|w| w.logical.to_string_lossy().into_owned())
            .unwrap_or_else(|| store_cwd.clone());
        if let Some(workspace) = &workspace {
            let mut units = prompt
                .string_units("")
                .ok_or_else(|| "invalid task prompt".to_string())?;
            units.extend(workspace.instructions().encode_utf16());
            prompt
                .set_string_units("", &units)
                .map_err(|error| error.to_string())?;
        }
        let gate = target_uuid.map(|id| mobile_cron_session_gate(&cwd, &id.to_string()));
        let _session_guard = match &gate {
            Some(gate) => Some(
                gate.try_lock()
                    .map_err(|_| "busy: Conversation is changing".to_string())?,
            ),
            None => None,
        };
        let mut readers = Vec::new();
        if let Some(uuid) = target_uuid {
            // Fresh sessions cannot have foreground readers. Do not retain
            // unrelated engines merely to create a new scheduled conversation.
            let handles: Vec<_> = MOBILE_CRON_HANDLES
                .get_or_init(Default::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .map(|reader| MobileCronBorrow(Some(reader)))
                .collect();
            for reader in handles {
                let handle: Arc<dyn OrchestratorHandle> = reader.inner.orchestrator.clone();
                if reader.session_cwd() == cwd
                    && handle.current_session_id().await.as_uuid() == uuid
                {
                    if reader.active_cancel.lock().await.is_some() {
                        return Err("busy: Conversation has an active turn".into());
                    }
                    readers.push(reader);
                }
            }
        }
        let mut cleanup = MobileCronTurnCleanup {
            readers,
            deny_requests: None,
            media_attachment: None,
        };
        let captured = Arc::new(Mutex::new(String::new()));
        let listener: Arc<dyn ClientEventListener> = Arc::new(CapturingListener {
            text: captured.clone(),
        });
        let (permission_tx, mut permission_rx) = mpsc::unbounded_channel();
        let sink: Arc<dyn PermissionRequestSink> = Arc::new(ImmediateDenyPermissionSink {
            sender: permission_tx,
        });
        let (mut config, child_media) = self.scheduled_runtime_config()?;
        cleanup.media_attachment = child_media;
        if let Some(workspace) = &workspace {
            config.cwd = workspace.host.clone();
            config.logical_cwd = self
                .cfg
                .logical_cwd
                .as_ref()
                .map(|_| workspace.logical.clone());
            config.scheduled_memory_directory = workspace
                .memory_host
                .parent()
                .map(std::path::Path::to_path_buf);
        }
        // Session transcripts and the scheduler store are host-owned paths,
        // not model-supplied guest paths, even if their spelling overlaps a
        // guest root such as /tmp.
        let model_fs = self.platform.filesystem();
        let host_fs = model_fs
            .host_filesystem()
            .unwrap_or_else(|| model_fs.clone());
        if let Some(uuid) = target_uuid {
            config.session_mode =
                cron_target_session_mode(&config.lingxi_home, &cwd, uuid, host_fs.clone()).await?;
        }
        if let Some(automation) = automation {
            config.default_model = automation.model.clone();
        }
        let mut rt = build_mobile_inner(config, self.platform.clone(), listener, sink, None)
            .await
            .map_err(|e| {
                if automation.is_some() {
                    format!("paused: Scheduled runtime configuration is unavailable: {e}")
                } else {
                    e.to_string()
                }
            })?;
        // The guard was installed before assembly to cover build failures and
        // is the sole cleanup owner for this short-lived scheduled runtime.
        debug_assert!(rt.media_attachment_confirmed_callback.is_none());
        drop(rt.media_attachment_handle.take());

        let handle: Arc<dyn OrchestratorHandle> = rt.orchestrator.clone();
        if let Some(uuid) = target_uuid {
            let replayed =
                cron_replay_session(&self.cfg.lingxi_home, &cwd, uuid, host_fs.clone()).await?;
            let (history, last_message, runtime) = match replayed {
                Some(replayed) => (
                    replayed.state.history.clone(),
                    replayed.last_message_uuid.map(|id| id.to_string()),
                    replayed.handle_runtime_snapshot(),
                ),
                None => (
                    Vec::new(),
                    None,
                    lingxi_core::host::ResumeRuntimeSnapshot::default(),
                ),
            };
            handle
                .resume_session(
                    lingxi_core::types::SessionId::from_uuid(uuid),
                    history,
                    last_message,
                    None,
                    runtime,
                )
                .await
                .map_err(|error| format!("paused: Cannot restore conversation: {error}"))?;
            rt.retarget_session_context(
                &self.cfg.lingxi_home,
                lingxi_core::types::SessionId::from_uuid(uuid),
                &cwd,
            )
            .await;
        }
        let session_id = handle.current_session_id().await.as_uuid().to_string();
        if target_uuid.is_none() && automation.is_some() {
            // A configured turn can fail validation before appending a prompt.
            // Anchor newly allocated identities before publishing the binding,
            // so a repaired task can resume the same durable conversation.
            rt.session_writer
                .append_mobile_empty_session(
                    &session_id,
                    automation
                        .and_then(|a| a.name.as_deref())
                        .unwrap_or("Scheduled task"),
                    &cwd,
                )
                .await
                .map_err(|error| format!("persist scheduled session anchor: {error}"))?;
            rt.session_writer
                .append_session_mode(self.cfg.session_mode.as_str())
                .await
                .map_err(|error| format!("persist scheduled session mode: {error}"))?;
        }
        if let Some(workspace) = &workspace {
            workspace.bind(&self.cfg.lingxi_home, &session_id)?;
        }
        if let Some(request) = request {
            cron::bind_automation_run_session(
                model_fs.as_ref(),
                std::path::Path::new(&store_cwd),
                request,
                &session_id,
            )
            .await?;
        }
        if let Some(route) = cleanup
            .media_attachment
            .as_ref()
            .and_then(|child| child.state.as_ref())
            .and_then(|child| child.route.as_ref())
        {
            // Use the actual parent engine's current binding and the restored
            // child's current session/profile. A missing or mismatched context
            // leaves media unavailable without blocking a text-only task.
            let parent = self.parent_engine();
            let child_binding = if let Some(automation) = automation {
                let reasoning = if automation.reasoning.is_null() {
                    Some(lingxi_core::host::ReasoningSelection::Automatic)
                } else {
                    serde_json::from_value(automation.reasoning.clone()).ok()
                };
                match reasoning {
                    Some(reasoning) => rt
                        .orchestrator
                        .scheduled_audio_binding(&automation.model, reasoning)
                        .await
                        .ok(),
                    None => None,
                }
            } else {
                rt.orchestrator.current_audio_binding().await.ok()
            };
            if let (Some(parent), Some((child_session, child_profile))) = (parent, child_binding) {
                if let Ok((parent_session, parent_profile)) =
                    parent.inner.orchestrator.current_audio_binding().await
                {
                    let same_context =
                        child_session == session_id && parent_profile == child_profile;
                    let same_snapshot = same_context
                        && parent
                            .inner
                            .audio_services
                            .profile_matches_current_of(&rt.audio_services, &child_profile);
                    if same_context && !same_snapshot {
                        tracing::warn!(
                            profile = %child_profile,
                            "scheduled media provider changed; rebuild or restart the parent engine before using cloud audio"
                        );
                    }
                    if same_snapshot {
                        let parent_id = self
                            .cfg
                            .media_attachment_handle
                            .as_ref()
                            .expect("child route requires parent attachment")
                            .id()
                            .clone();
                        let parent_is_current = scheduled_parent_context_check(
                            &parent,
                            parent_id,
                            parent_session.clone(),
                            parent_profile.clone(),
                        );
                        let account_scope = format!("profile:{child_profile}");
                        if let Err(error) = route.bind_session(
                            &parent_session,
                            &child_session,
                            &child_profile,
                            &account_scope,
                            parent_is_current,
                        ) {
                            tracing::warn!(%error, "scheduled media provider binding unavailable");
                        }
                    }
                }
            }
        }
        let gate = rt.permission_gate.clone();
        cleanup.deny_requests = Some(tokio::spawn(async move {
            while let Some(request) = permission_rx.recv().await {
                let tool_name = match &request.kind {
                    PermissionKindDto::ToolUseConfirm { tool_name, .. } => tool_name.as_str(),
                    _ => "",
                };
                let _ = gate
                    .resolve(request.request_id, PermissionResponseDto::Deny, tool_name)
                    .await;
            }
        }));
        let turn_cancel = CancellationToken::new();
        let _cancel_turn_on_drop = turn_cancel.clone().drop_guard();
        let run = async {
            if let Some(automation) = automation {
                let reasoning = if automation.reasoning.is_null() {
                    lingxi_core::host::ReasoningSelection::Automatic
                } else {
                    serde_json::from_value(automation.reasoning.clone())
                        .map_err(|e| format!("paused: {e}"))?
                };
                rt.orchestrator
                    .run_scheduled_turn(
                        prompt.clone(),
                        &automation.model,
                        reasoning,
                        turn_cancel.clone(),
                    )
                    .await
                    .and_then(cron_scheduled_turn_outcome)
            } else {
                rt.orchestrator
                    .run_turn_streaming_source_with_origin(
                        prompt.clone(),
                        Vec::new(),
                        turn_cancel.clone(),
                        None,
                        true,
                    )
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(cron_scheduled_turn_outcome)
            }
        };

        let result = match tokio::time::timeout(CRON_TURN_TIMEOUT, run).await {
            Ok(Ok(_outcome)) => Ok(cron::AutomationRunResult {
                session_id,
                summary: captured.lock().await.clone(),
            }),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("cron turn timed out".to_string()),
        };
        cleanup.revoke_media_attachment().await?;
        result
    }
}

impl MobileCronTurnCleanup {
    async fn revoke_media_attachment(&mut self) -> Result<(), String> {
        if let Some(child) = &mut self.media_attachment {
            child.finish().await?;
        }
        self.media_attachment = None;
        Ok(())
    }
}

pub(super) async fn cron_replay_session(
    home: &std::path::Path,
    cwd: &str,
    session_id: uuid::Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<Option<orchestrator::resume::ReplayedSession>, String> {
    match orchestrator::replay_session_state(home, cwd, session_id, fs.clone()).await {
        Ok(replayed) => Ok(Some(replayed)),
        Err(error) => {
            if matches!(
                &error,
                orchestrator::resume::ResumeError::Loader(
                    session::jsonl::LoaderError::EmptyDirectory
                        | session::jsonl::LoaderError::SessionNotFound { .. }
                )
            ) {
                let path = session::jsonl::session_path(home, cwd, &session_id.to_string());
                if let Ok(routed) = session::jsonl::JsonlReader::new(path, fs)
                    .read_routed()
                    .await
                {
                    // Match the native resume-empty contract: an existing,
                    // explicitly versioned anchor is required. A missing or
                    // deleted selected/owned transcript is never recreated.
                    if routed
                        .mobile_empty_sessions
                        .contains(&session_id.to_string())
                        && routed.messages_in_order.is_empty()
                    {
                        return Ok(None);
                    }
                }
            }
            Err(format!("paused: Cannot restore conversation: {error}"))
        }
    }
}

pub(super) async fn cron_target_session_mode(
    home: &std::path::Path,
    cwd: &str,
    session_id: uuid::Uuid,
    fs: Arc<dyn FileSystem>,
) -> Result<session::jsonl::SessionMode, String> {
    let path = session::jsonl::session_path(home, cwd, &session_id.to_string());
    let routed = session::jsonl::JsonlReader::new(path, fs)
        .read_routed()
        .await
        .map_err(|error| format!("paused: Cannot read scheduled conversation mode: {error}"))?;
    match routed.session_modes.get(&session_id.to_string()) {
        Some(value) => session::jsonl::SessionMode::from_str(value)
            .ok_or_else(|| "paused: Scheduled conversation mode is invalid".to_string()),
        // Historical transcripts without explicit mode were Code sessions.
        None => Ok(session::jsonl::SessionMode::Code),
    }
}

pub(super) fn cron_scheduled_turn_outcome(
    outcome: orchestrator::conversation::TurnOutcome,
) -> Result<(), String> {
    match outcome {
        orchestrator::conversation::TurnOutcome::EndTurn => Ok(()),
        orchestrator::conversation::TurnOutcome::Cancelled => Err(format!(
            "{}Scheduled run cancelled",
            cron::AUTOMATION_CANCELLED_PREFIX
        )),
        orchestrator::conversation::TurnOutcome::MaxTurns => {
            Err("Scheduled run did not complete: maximum turns reached".into())
        }
    }
}

#[cfg(test)]
mod media_attachment_tests {
    use super::*;
    use async_trait::async_trait;
    use media_api::{
        MediaCapabilities, MediaError, MediaErrorKind, MediaOperation, MediaOperationContext,
        MediaOperationId, MediaOwner, MediaResourceStatus, MediaResult, MediaService,
    };
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RecordingMediaService {
        owners: StdMutex<Vec<MediaOwner>>,
        fail_status: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl MediaService for RecordingMediaService {
        fn capabilities(&self) -> MediaCapabilities {
            MediaCapabilities {
                service_epoch: 17,
                support_revision: 1,
                supported_operations: vec![media_api::MediaOperationKind::Status],
                supported_media_kinds: Vec::new(),
                readiness: Vec::new(),
                max_payload_bytes: 1024,
            }
        }

        async fn execute(
            &self,
            context: MediaOperationContext,
            operation: MediaOperation,
        ) -> Result<MediaResult, MediaError> {
            self.owners.lock().unwrap().push(context.owner);
            match operation {
                MediaOperation::Status { .. }
                    if self.fail_status.load(std::sync::atomic::Ordering::SeqCst) =>
                {
                    Err(MediaError::new(
                        MediaErrorKind::Unavailable,
                        "status unavailable",
                    ))
                }
                MediaOperation::Status { .. } => Ok(MediaResult::Status {
                    resources: Vec::<MediaResourceStatus>::new(),
                }),
                _ => Err(MediaError::new(
                    MediaErrorKind::Unsupported,
                    "recording fixture only supports status",
                )),
            }
        }

        async fn read_media(
            &self,
            _context: MediaOperationContext,
            _media: media_api::MediaRef,
        ) -> Result<media_api::MediaContent, MediaError> {
            Err(MediaError::new(
                MediaErrorKind::NotFound,
                "recording fixture has no media",
            ))
        }

        async fn cancel(&self, _context: MediaOperationContext) -> Result<(), MediaError> {
            Ok(())
        }
    }

    struct TestScheduledRoute {
        child: media_runtime::MediaAttachmentHandle,
        released: Arc<std::sync::atomic::AtomicUsize>,
        released_notify: Arc<tokio::sync::Notify>,
    }

    impl orchestrator::ScheduledMediaChildRoute for TestScheduledRoute {
        fn bind_session(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &str,
            _: orchestrator::ScheduledMediaParentCheck,
        ) -> Result<(), String> {
            Ok(())
        }
        fn release_after_media_drain(&self) -> Result<(), String> {
            if !self.child.media_drain_confirmed() {
                return Err("media drain not confirmed".into());
            }
            self.released
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.released_notify.notify_one();
            Ok(())
        }
    }

    struct TestScheduledFactory {
        released: Arc<std::sync::atomic::AtomicUsize>,
        released_notify: Arc<tokio::sync::Notify>,
    }

    #[async_trait]
    impl orchestrator::MediaConversationAttachment for TestScheduledFactory {
        fn fork_scheduled_child(
            &self,
            parent: &media_runtime::MediaAttachmentHandle,
        ) -> Result<Option<orchestrator::ScheduledMediaChild>, String> {
            let child = parent.fork_child().map_err(|error| error.to_string())?;
            Ok(Some(orchestrator::ScheduledMediaChild {
                attachment: child.clone(),
                route: Arc::new(TestScheduledRoute {
                    child,
                    released: self.released.clone(),
                    released_notify: self.released_notify.clone(),
                }),
            }))
        }
        async fn open(
            &self,
            _: orchestrator::MediaAgentHandoffRequest,
            _: orchestrator::RealtimeAgentContext,
            _: Arc<dyn media_api::MediaAudioChannel>,
        ) -> Result<orchestrator::RealtimeAgentSessionIo, String> {
            Err("scheduled tests do not open realtime".into())
        }
    }

    #[test]
    fn scheduled_parent_check_tracks_exact_live_engine_identity() {
        use crate::mobile::test_support::{
            new_engine_with_streaming, CollectingPermissionSink, FakeListener, HostFakePlatform,
        };
        use orchestrator::test_support_stream::MockStreamingApiClient;
        let root = tempfile::tempdir().unwrap();
        let host = media_runtime::MediaAttachmentHost::new();
        let attachment = host
            .attach(Arc::new(RecordingMediaService::default()))
            .unwrap();
        let mut config = crate::mobile::test_support::test_config(root.path());
        config.media_service = Some(attachment.service());
        config.media_attachment_handle = Some(attachment.clone());
        let platform = Arc::new(HostFakePlatform::new(root.path().to_path_buf()));
        let engine = new_engine_with_streaming(
            config.clone(),
            platform.clone(),
            Arc::new(FakeListener::default()),
            Arc::new(CollectingPermissionSink::default()),
            Some(Arc::new(MockStreamingApiClient::empty())),
        )
        .unwrap();
        let firer = MobileTurnFirer {
            cfg: config,
            platform,
        };
        let parent = firer
            .parent_engine()
            .expect("find exact host-issued parent");
        let (session, profile) = engine
            .runtime()
            .block_on(parent.inner.orchestrator.current_audio_binding())
            .expect("parent has an authoritative binding");
        let check = scheduled_parent_context_check(
            &parent,
            attachment.id().clone(),
            session.clone(),
            profile.clone(),
        );
        let wrong_session = scheduled_parent_context_check(
            &parent,
            attachment.id().clone(),
            "another-session".into(),
            profile.clone(),
        );
        let wrong_profile = scheduled_parent_context_check(
            &parent,
            attachment.id().clone(),
            session,
            "another-profile".into(),
        );
        assert!(check());
        assert!(!wrong_session());
        assert!(!wrong_profile());
        attachment.begin_revoke();
        assert!(!check());
    }

    #[tokio::test]
    async fn scheduled_child_cleanup_waits_for_media_receipt_before_host_route() {
        use media_runtime::{MediaAttachmentPhase, MediaAttachmentRevocation};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let host = media_runtime::MediaAttachmentHost::new();
        let parent = host
            .attach(Arc::new(RecordingMediaService::default()))
            .unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut config = crate::mobile::test_support::test_config(root.path());
        config.media_attachment_handle = Some(parent.clone());
        config.media_conversation_attachment = Some(Arc::new(TestScheduledFactory {
            released: released.clone(),
            released_notify: notify,
        }));
        let firer = MobileTurnFirer {
            cfg: config,
            platform: Arc::new(crate::mobile::test_support::HostFakePlatform::new(
                root.path().to_path_buf(),
            )),
        };
        let (config, cleanup) = firer.scheduled_runtime_config().unwrap();
        let child = config.media_attachment_handle.unwrap();
        let mut cleanup = cleanup.unwrap();
        assert!(cleanup.state.as_ref().unwrap().fence.is_some());
        let permit = child.authorize_owner(&logical_owner()).unwrap();
        assert!(cleanup
            .state
            .as_mut()
            .unwrap()
            .finish(std::time::Duration::from_millis(20))
            .await
            .is_err());
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(parent.phase(), MediaAttachmentPhase::Active);
        drop(permit);
        cleanup.finish().await.unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoked);
        assert_eq!(
            parent.revoke(std::time::Duration::from_secs(2)).await,
            MediaAttachmentRevocation::Confirmed
        );
    }

    #[tokio::test]
    async fn scheduled_child_unknown_drain_never_releases_host_route() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let host = media_runtime::MediaAttachmentHost::new();
        let shared = Arc::new(RecordingMediaService::default());
        shared.fail_status.store(true, Ordering::SeqCst);
        let parent = host.attach(shared).unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let mut config = crate::mobile::test_support::test_config(root.path());
        config.media_attachment_handle = Some(parent.clone());
        config.media_conversation_attachment = Some(Arc::new(TestScheduledFactory {
            released: released.clone(),
            released_notify: Arc::new(tokio::sync::Notify::new()),
        }));
        let firer = MobileTurnFirer {
            cfg: config,
            platform: Arc::new(crate::mobile::test_support::HostFakePlatform::new(
                root.path().to_path_buf(),
            )),
        };
        let (config, cleanup) = firer.scheduled_runtime_config().unwrap();
        let child = config.media_attachment_handle.unwrap();
        let mut cleanup = cleanup.unwrap();
        assert!(child
            .service()
            .execute(
                MediaOperationContext::new(MediaOperationId::new(1, 17), logical_owner()),
                MediaOperation::Status { target: None },
            )
            .await
            .is_err());
        assert!(cleanup.finish().await.is_err());
        assert_eq!(child.phase(), media_runtime::MediaAttachmentPhase::Unknown);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        assert!(cleanup.state.as_ref().unwrap().fence.is_some());
        drop(cleanup);
        assert_eq!(released.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn dropped_scheduled_child_guard_releases_only_after_late_receipt() {
        use media_runtime::MediaAttachmentPhase;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let host = media_runtime::MediaAttachmentHost::new();
        let parent = host
            .attach(Arc::new(RecordingMediaService::default()))
            .unwrap();
        let released = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(tokio::sync::Notify::new());
        let mut config = crate::mobile::test_support::test_config(root.path());
        config.media_attachment_handle = Some(parent.clone());
        config.media_conversation_attachment = Some(Arc::new(TestScheduledFactory {
            released: released.clone(),
            released_notify: notify.clone(),
        }));
        let firer = MobileTurnFirer {
            cfg: config,
            platform: Arc::new(crate::mobile::test_support::HostFakePlatform::new(
                root.path().to_path_buf(),
            )),
        };
        let (config, cleanup) = firer.scheduled_runtime_config().unwrap();
        let child = config.media_attachment_handle.unwrap();
        let permit = child.authorize_owner(&logical_owner()).unwrap();
        drop(cleanup);
        assert_eq!(child.phase(), MediaAttachmentPhase::Revoking);
        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(permit);
        tokio::time::timeout(std::time::Duration::from_secs(2), notify.notified())
            .await
            .unwrap();
        // The route is released before confirming the fence; wait until both finish.
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while child.phase() != MediaAttachmentPhase::Revoked {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(released.load(Ordering::SeqCst), 1);
        assert_eq!(parent.phase(), MediaAttachmentPhase::Active);
    }

    fn logical_owner() -> MediaOwner {
        MediaOwner::Session {
            session_id: "scheduled-session".into(),
        }
    }

    #[tokio::test]
    async fn scheduled_runtime_uses_child_scope_and_is_fenced_by_parent() {
        let root = tempfile::tempdir().unwrap();
        let platform = Arc::new(crate::mobile::test_support::HostFakePlatform::new(
            root.path().to_path_buf(),
        ));
        let shared = Arc::new(RecordingMediaService::default());
        let host = media_runtime::MediaAttachmentHost::new();
        let parent = host.attach(shared.clone()).unwrap();
        let mut parent_config = crate::mobile::test_support::test_config(root.path());
        parent_config.media_service = Some(parent.service());
        parent_config.media_attachment_handle = Some(parent.clone());
        parent_config.media_attachment_confirmed_callback = Some(Arc::new(|| {}));

        let firer = MobileTurnFirer {
            cfg: parent_config,
            platform: platform.clone(),
        };
        let (child_config, _child_cleanup) = firer.scheduled_runtime_config().unwrap();
        let child = child_config
            .media_attachment_handle
            .as_ref()
            .expect("scheduled runtime receives its own host-issued attachment")
            .clone();
        assert_ne!(parent.id(), child.id());
        assert_eq!(
            child_config
                .media_service
                .as_ref()
                .and_then(|service| service.attachment_id()),
            Some(child.id().clone())
        );
        assert!(child_config.media_conversation_attachment.is_none());
        assert!(child_config.media_attachment_confirmed_callback.is_none());

        let owner = logical_owner();
        child_config
            .media_service
            .as_ref()
            .unwrap()
            .execute(
                MediaOperationContext::new(MediaOperationId::new(1, 17), owner.clone()),
                MediaOperation::Status { target: None },
            )
            .await
            .unwrap();
        assert_eq!(
            *shared.owners.lock().unwrap(),
            vec![owner.clone().in_attachment(child.id().clone())]
        );

        // Parent admission closes synchronously, before its async drain has
        // necessarily scheduled the child drain.
        parent.begin_revoke();
        assert!(matches!(firer.scheduled_runtime_config(), Err(_)));
        assert_eq!(
            child_config
                .media_service
                .as_ref()
                .unwrap()
                .execute(
                    MediaOperationContext::new(MediaOperationId::new(2, 17), owner),
                    MediaOperation::Status { target: None },
                )
                .await
                .unwrap_err()
                .kind,
            MediaErrorKind::Cancelled
        );
        assert_eq!(
            parent.revoke(std::time::Duration::from_secs(1)).await,
            media_runtime::MediaAttachmentRevocation::Confirmed
        );
        assert_eq!(child.phase(), media_runtime::MediaAttachmentPhase::Revoked);
    }

    #[test]
    fn scheduled_runtime_does_not_share_an_unscoped_media_facade() {
        let root = tempfile::tempdir().unwrap();
        let platform = Arc::new(crate::mobile::test_support::HostFakePlatform::new(
            root.path().to_path_buf(),
        ));
        let shared = Arc::new(RecordingMediaService::default());
        let mut config = crate::mobile::test_support::test_config(root.path());
        config.media_service = Some(shared);
        let firer = MobileTurnFirer {
            cfg: config,
            platform,
        };

        let (scheduled, _child_cleanup) = firer.scheduled_runtime_config().unwrap();
        assert!(scheduled.media_service.is_none());
        assert!(scheduled.media_attachment_handle.is_none());
    }
}

pub(super) fn cron_legacy_turn_outcome(
    outcome: orchestrator::ConversationOutcome,
) -> Result<(), String> {
    match outcome {
        orchestrator::ConversationOutcome::EndTurn { .. } => Ok(()),
        orchestrator::ConversationOutcome::StopHookPrevented { .. } => Err(format!(
            "{}Scheduled run stopped by a hook",
            cron::AUTOMATION_CANCELLED_PREFIX
        )),
        _ => Err("Scheduled run did not complete".into()),
    }
}

pub(super) const MOBILE_MIN_RECURRING_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

pub(super) fn cron_field_values(field: &cron::CronField, min: u32, max: u32) -> Option<Vec<u32>> {
    // `cron::parse_cron` already expands and range-checks every field the way
    // claude-code's `expandField` does; this only guards the domain it was
    // handed against the one the caller expects.
    let values = field.values().to_vec();
    if !values.is_empty() && values.iter().all(|value| (min..=max).contains(value)) {
        Some(values)
    } else {
        None
    }
}

/// Android's recurring-work contract: a valid five-field cron expression whose
/// closest two wall-clock occurrences are at least fifteen minutes apart.
/// One-shot tasks still validate field ranges but are not interval-limited.
pub(super) fn mobile_cron_schedule_error(cron_expr: &str, recurring: bool) -> Option<String> {
    let expression = match cron::parse_cron(cron_expr) {
        Ok(expression) => expression,
        Err(error) => return Some(format!("invalid cron expression: {error}")),
    };
    let invalid_field = || Some("cron expression contains an out-of-range field".to_string());
    let Some(minutes) = cron_field_values(&expression.minute, 0, 59) else {
        return invalid_field();
    };
    let Some(hours) = cron_field_values(&expression.hour, 0, 23) else {
        return invalid_field();
    };
    if cron_field_values(&expression.dom, 1, 31).is_none()
        || cron_field_values(&expression.month, 1, 12).is_none()
        || cron_field_values(&expression.dow, 0, 6).is_none()
    {
        return invalid_field();
    }
    if !recurring {
        return None;
    }

    let mut minute_of_day = Vec::with_capacity(minutes.len() * hours.len());
    for hour in hours {
        for minute in &minutes {
            minute_of_day.push(hour * 60 + minute);
        }
    }
    minute_of_day.sort_unstable();
    minute_of_day.dedup();
    if minute_of_day.is_empty() {
        return Some("cron expression has no valid fire time".to_string());
    }
    if minute_of_day.len() > 1 {
        let min_gap = minute_of_day
            .windows(2)
            .map(|pair| pair[1] - pair[0])
            .chain(std::iter::once(
                24 * 60 - minute_of_day[minute_of_day.len() - 1] + minute_of_day[0],
            ))
            .min()
            .unwrap_or(24 * 60);
        if std::time::Duration::from_secs(u64::from(min_gap) * 60) < MOBILE_MIN_RECURRING_INTERVAL {
            return Some(
                "Recurring tasks on this device must be at least 15 minutes apart".to_string(),
            );
        }
    }
    None
}

pub(super) fn cron_task_dto(task: cron::CronTask, now: std::time::SystemTime) -> CronTaskDto {
    let recurring = task.recurring.unwrap_or(false);
    let unsupported_reason = mobile_cron_schedule_error(&task.cron, recurring);
    CronTaskDto {
        automation_json: task
            .automation
            .as_ref()
            .and_then(|value| serde_json::to_string(value).ok()),
        human: tool_cron::schedule_cron::cron_to_human(&task.cron),
        next_fire_ms: if cron_task_active(&task) {
            task.automation
                .as_ref()
                .and_then(|a| {
                    a.runs
                        .iter()
                        .find(|r| r.status == cron::AutomationRunStatus::Queued)
                        .map(|r| r.scheduled_at)
                })
                .or_else(|| cron::next_fire_epoch_ms_for_persisted_task(&task, now))
        } else {
            None
        },
        id: task.id,
        cron: task.cron,
        prompt: source_display(&task.prompt),
        created_at_ms: task.created_at,
        last_fired_at_ms: task.last_fired_at,
        recurring,
        mobile_supported: unsupported_reason.is_none(),
        unsupported_reason,
    }
}

pub(super) fn cron_failure_is_retryable(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    [
        "429",
        "rate limit",
        "transport error",
        "connection failed",
        "temporarily unavailable",
        "timed out",
        "timeout",
        "dns",
        "http 5",
        "status 5",
        "server error",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(super) fn fired_cron_dto(
    task: &cron::CronTask,
    result: Result<String, String>,
) -> FiredCronJobDto {
    match result {
        Ok(text) => FiredCronJobDto {
            session_id: None,
            id: task.id.clone(),
            prompt: source_display(&task.prompt),
            result_text: Some(text),
            status: CronFireStatusDto::Ok,
            retryable: false,
        },
        Err(message) => FiredCronJobDto {
            session_id: None,
            id: task.id.clone(),
            prompt: source_display(&task.prompt),
            result_text: None,
            retryable: cron_failure_is_retryable(&message),
            status: CronFireStatusDto::Failed { message },
        },
    }
}

pub(super) async fn read_cron_tasks(
    fs: &dyn FileSystem,
    cwd: &std::path::Path,
) -> cron::ScheduledTasks {
    cron::tasks_file::read_automation_tasks_body(fs, cwd)
        .await
        .map(|body| cron::tasks_file::parse_automation_tasks(&body))
        .unwrap_or_default()
}

/// Lightweight, credential-free scheduled-task store used by Android UI and
/// reconciliation workers. It owns only the validated workspace root plus the
/// platform filesystem/clock; constructing it never builds an LLM client.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MobileCronStoreHandle {
    pub(super) cwd: std::path::PathBuf,
    pub(super) fs: Arc<dyn FileSystem>,
    pub(super) clock: Arc<dyn Clock>,
}

impl MobileCronStoreHandle {
    #[must_use]
    pub fn new(cwd: std::path::PathBuf, fs: Arc<dyn FileSystem>, clock: Arc<dyn Clock>) -> Self {
        Self { cwd, fs, clock }
    }
    pub(super) async fn migrate_legacy_scope(&self) -> Result<(), MobileEngineError> {
        if !self.cwd.ends_with("scheduled/workspace") {
            return Ok(());
        }
        let Some(root) = self.cwd.parent().and_then(std::path::Path::parent) else {
            return Ok(());
        };
        let _guard = cron::lock_cron_file().await;
        let _legacy_lock = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), root)
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let _new_lock = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let marker_path = std::path::Path::new(".lingxi/cron-v2-migration.json");
        let marker: Option<serde_json::Value> =
            match self.fs.read_file_rooted_no_follow(root, marker_path).await {
                Ok(file) => Some(serde_json::from_str(&file.content).map_err(|e| {
                    MobileEngineError::Internal(format!("invalid cron migration marker: {e}"))
                })?),
                Err(lingxi_core::host::FsError::NotFound(_)) => None,
                Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
            };
        let old_body =
            match cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), root).await {
                Ok(body) => body,
                Err(lingxi_core::host::FsError::NotFound(_)) => {
                    cron::serialize_tasks(&cron::ScheduledTasks::default())
                }
                Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
            };
        let mut legacy = cron::tasks_file::parse_automation_tasks_strict(&old_body)
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let snapshot = if let Some(marker) = marker
            .as_ref()
            .filter(|marker| marker.get("completed") == Some(&serde_json::Value::Bool(false)))
        {
            marker
                .get("source")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    MobileEngineError::Internal(
                        "cron migration marker is missing its source snapshot".into(),
                    )
                })?
                .to_string()
        } else {
            if legacy.tasks.is_empty() {
                return Ok(());
            }
            let pending = serde_json::json!({"version":2,"completed":false,"source":old_body});
            self.fs
                .write_file_rooted_atomic(root, marker_path, &pending.to_string())
                .await
                .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
            old_body
        };
        let mut source = cron::tasks_file::parse_automation_tasks_strict(&snapshot)
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        // Old /loop records belong to their original session, not task-center
        // automations. Exclude before computing removal IDs so they stay intact
        // in the old document without being revived in the destination.
        source
            .tasks
            .retain(|task| !cron::is_loop_default_sentinel(&task.prompt));
        // Suppress the old scheduler before publishing any destination tasks.
        // The durable source snapshot recovers a crash after this write.
        let migrated_ids: std::collections::HashSet<_> =
            source.tasks.iter().map(|task| task.id.clone()).collect();
        legacy.tasks.retain(|task| !migrated_ids.contains(&task.id));
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            root,
            &cron::serialize_tasks(&legacy),
        )
        .await
        .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let mut destination =
            match cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), &self.cwd).await {
                Ok(body) => cron::tasks_file::parse_automation_tasks_strict(&body)
                    .map_err(|e| MobileEngineError::Internal(e.to_string()))?,
                Err(lingxi_core::host::FsError::NotFound(_)) => cron::ScheduledTasks::default(),
                Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
            };
        for mut task in source.tasks {
            if destination
                .tasks
                .iter()
                .any(|existing| existing.id == task.id)
            {
                continue;
            }
            if task.automation.is_none() {
                task.automation = Some(cron::CronAutomation {
                    version: 2,
                    name: None,
                    status: cron::AutomationStatus::Paused,
                    status_reason: Some("Choose a model to enable this migrated task".into()),
                    model: String::new(),
                    reasoning: serde_json::json!({"type":"automatic"}),
                    run_mode: cron::RunMode::NewSession,
                    target_session_id: None,
                    owned_session_id: None,
                    notification_policy: cron::NotificationPolicy::All,
                    runs: Vec::new(),
                });
            }
            destination.tasks.push(task);
        }
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            &self.cwd,
            &cron::serialize_tasks(&destination),
        )
        .await
        .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        self.fs
            .write_file_rooted_atomic(
                root,
                marker_path,
                &serde_json::json!({"version":2,"completed":true}).to_string(),
            )
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        Ok(())
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileCronStoreHandle {
    /// Capture host-selected defaults while upgrading legacy tasks atomically.
    pub async fn set_migration_defaults(
        &self,
        model: String,
        reasoning_json: String,
    ) -> Result<(), MobileEngineError> {
        self.migrate_legacy_scope().await?;
        if model.trim().is_empty() {
            return Ok(());
        }
        let reasoning: lingxi_core::host::ReasoningSelection =
            serde_json::from_str(&reasoning_json)
                .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let reasoning = serde_json::to_value(reasoning)
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let _guard = cron::lock_cron_file().await;
        let _file = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let body =
            match cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), &self.cwd).await {
                Ok(body) => body,
                Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
            };
        let mut document = cron::tasks_file::parse_automation_tasks_strict(&body)
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let now = self
            .clock
            .now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let mut changed = false;
        for task in &mut document.tasks {
            let needs_defaults = task.automation.as_ref().is_none_or(|a| {
                a.model.is_empty()
                    && a.status_reason.as_deref()
                        == Some("Choose a model to enable this migrated task")
            });
            if !needs_defaults {
                continue;
            }
            task.automation = Some(cron::CronAutomation {
                version: 2,
                name: None,
                status: if task.expires_at.is_some_and(|expiry| expiry <= now) {
                    cron::AutomationStatus::Completed
                } else {
                    cron::AutomationStatus::Active
                },
                status_reason: None,
                model: model.clone(),
                reasoning: reasoning.clone(),
                run_mode: cron::RunMode::NewSession,
                target_session_id: None,
                owned_session_id: None,
                notification_policy: cron::NotificationPolicy::All,
                runs: Vec::new(),
            });
            changed = true;
        }
        if changed {
            cron::tasks_file::write_automation_tasks_body(
                self.fs.as_ref(),
                &self.cwd,
                &cron::serialize_tasks(&document),
            )
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn list(&self) -> Vec<CronTaskDto> {
        if let Err(error) = self.migrate_legacy_scope().await {
            tracing::warn!(%error, "cron migration failed");
            return Vec::new();
        }
        if !read_cron_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .tasks
            .is_empty()
        {
            let now_ms = self
                .clock
                .now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            if let Err(error) =
                cron::recover_orphaned_automation_runs(self.fs.as_ref(), &self.cwd, now_ms).await
            {
                tracing::warn!(%error, "cron recovery failed");
            }
        }
        let now = self.clock.now();
        read_cron_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .tasks
            .into_iter()
            .map(|task| cron_task_dto(task, now))
            .collect()
    }

    pub async fn create(
        &self,
        cron_expr: String,
        prompt: String,
        recurring: bool,
    ) -> Result<CronTaskDto, MobileEngineError> {
        self.create_configured(cron_expr, prompt, recurring, String::new())
            .await
    }

    pub async fn create_configured(
        &self,
        cron_expr: String,
        prompt: String,
        recurring: bool,
        automation_json: String,
    ) -> Result<CronTaskDto, MobileEngineError> {
        self.migrate_legacy_scope().await?;
        let automation = decode_cron_automation(&automation_json)?;
        if let Some(error) = mobile_cron_schedule_error(&cron_expr, recurring) {
            return Err(MobileEngineError::Internal(error));
        }
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|error| {
                MobileEngineError::Internal(format!("lock scheduled_tasks.json: {error}"))
            })?;
        // `create` is the one mutation that rewrites the file from whatever it
        // read: `update`/`delete` bail when the id is absent, so an empty
        // document makes them no-ops. Only a genuinely ABSENT file may start a
        // fresh document here — any other read error (EIO, EACCES, the rooted-fs
        // symlink rejection) is not evidence that there are no tasks, and
        // starting from `default()` would write the new task over every
        // existing one.
        let mut document =
            match cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), &self.cwd).await {
                Ok(body) => cron::tasks_file::parse_automation_tasks(&body),
                Err(lingxi_core::host::FsError::NotFound(_)) => cron::ScheduledTasks::default(),
                Err(error) => {
                    return Err(MobileEngineError::Internal(format!(
                        "read scheduled_tasks.json: {error}"
                    )));
                }
            };
        let now = self.clock.now();
        let now_ms = now
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let task = cron::CronTask {
            creator: Default::default(),
            id: tool_cron::schedule_cron::generate_cron_task_id(),
            cron: cron_expr,
            prompt: prompt.into(),
            created_at: now_ms,
            last_fired_at: None,
            recurring: Some(recurring),
            permanent: None,
            expires_at: None,
            session_id: None,
            automation,
        };
        document.tasks.push(task.clone());
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            &self.cwd,
            &cron::serialize_tasks(&document),
        )
        .await
        .map_err(|error| {
            MobileEngineError::Internal(format!("write scheduled_tasks.json: {error}"))
        })?;
        Ok(cron_task_dto(task, now))
    }

    pub async fn update(
        &self,
        id: String,
        cron_expr: String,
        prompt: String,
        recurring: bool,
    ) -> Result<CronTaskDto, MobileEngineError> {
        self.update_configured(id, cron_expr, prompt, recurring, String::new())
            .await
    }

    pub async fn update_configured(
        &self,
        id: String,
        cron_expr: String,
        prompt: String,
        recurring: bool,
        automation_json: String,
    ) -> Result<CronTaskDto, MobileEngineError> {
        let automation = decode_cron_automation(&automation_json)?;
        if let Some(error) = mobile_cron_schedule_error(&cron_expr, recurring) {
            return Err(MobileEngineError::Internal(error));
        }
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|error| {
                MobileEngineError::Internal(format!("lock scheduled_tasks.json: {error}"))
            })?;
        let mut document = read_cron_tasks(self.fs.as_ref(), &self.cwd).await;
        let now = self.clock.now();
        let now_ms = now
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let task = document
            .tasks
            .iter_mut()
            .find(|task| task.id == id)
            .ok_or(MobileEngineError::NotFound)?;
        if let Some(mut automation) = automation {
            if let Some(previous) = &task.automation {
                automation.runs = previous.runs.clone();
                automation.owned_session_id = if automation.run_mode == previous.run_mode {
                    previous.owned_session_id.clone()
                } else {
                    None
                };
            }
            if automation.status != cron::AutomationStatus::Active {
                automation
                    .runs
                    .retain(|run| run.status != cron::AutomationRunStatus::Queued);
            }
            task.automation = Some(automation);
        }
        task.cron = cron_expr;
        task.prompt = prompt.into();
        task.recurring = Some(recurring);
        task.created_at = now_ms;
        task.last_fired_at = None;
        let updated = task.clone();
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            &self.cwd,
            &cron::serialize_tasks(&document),
        )
        .await
        .map_err(|error| {
            MobileEngineError::Internal(format!("write scheduled_tasks.json: {error}"))
        })?;
        Ok(cron_task_dto(updated, now))
    }

    /// Update only automation settings without resetting the schedule anchor.
    pub async fn update_automation(
        &self,
        id: String,
        automation_json: String,
    ) -> Result<CronTaskDto, MobileEngineError> {
        let mut automation = decode_cron_automation(&automation_json)?.ok_or_else(|| {
            MobileEngineError::Internal("automation settings are required".into())
        })?;
        let _guard = cron::lock_cron_file().await;
        let _file = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let body = cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let mut document = cron::tasks_file::parse_automation_tasks_strict(&body)
            .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        let task = document
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(MobileEngineError::NotFound)?;
        if let Some(previous) = &task.automation {
            automation.runs = previous.runs.clone();
            automation.owned_session_id = if automation.run_mode == previous.run_mode {
                previous.owned_session_id.clone()
            } else {
                None
            };
            if previous.status != cron::AutomationStatus::Active
                && automation.status == cron::AutomationStatus::Active
            {
                task.last_fired_at = Some(
                    self.clock
                        .now()
                        .duration_since(std::time::SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64,
                );
            }
        }
        if automation.status != cron::AutomationStatus::Active {
            automation
                .runs
                .retain(|run| run.status != cron::AutomationRunStatus::Queued);
        }
        task.automation = Some(automation);
        let updated = task.clone();
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            &self.cwd,
            &cron::serialize_tasks(&document),
        )
        .await
        .map_err(|e| MobileEngineError::Internal(e.to_string()))?;
        Ok(cron_task_dto(updated, self.clock.now()))
    }

    pub async fn delete(&self, id: String) -> bool {
        let _process_guard = cron::lock_cron_file().await;
        let Ok(_file_guard) =
            cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd).await
        else {
            return false;
        };
        let mut document = read_cron_tasks(self.fs.as_ref(), &self.cwd).await;
        let previous_len = document.tasks.len();
        document.tasks.retain(|task| task.id != id);
        previous_len != document.tasks.len()
            && cron::tasks_file::write_automation_tasks_body(
                self.fs.as_ref(),
                &self.cwd,
                &cron::serialize_tasks(&document),
            )
            .await
            .is_ok()
    }

    pub async fn next_fire_time(&self) -> Option<u64> {
        self.list()
            .await
            .into_iter()
            .filter(|task| task.mobile_supported)
            .filter_map(|task| task.next_fire_ms)
            .min()
    }

    pub async fn due_occurrences(&self, now_ms: u64) -> Vec<CronDueOccurrenceDto> {
        self.list()
            .await
            .into_iter()
            .filter(|task| task.mobile_supported)
            .filter_map(|task| {
                task.next_fire_ms
                    .filter(|scheduled_at_ms| *scheduled_at_ms <= now_ms)
                    .map(|scheduled_at_ms| CronDueOccurrenceDto {
                        task_id: task.id,
                        scheduled_at_ms,
                    })
            })
            .collect()
    }

    /// Mark a due occurrence complete after the host exhausts retries. This is
    /// intentionally available on the lightweight store so iOS background
    /// reconciliation never needs to construct an LLM engine just to advance
    /// durable schedule bookkeeping.
    pub async fn acknowledge_occurrence(&self, task_id: String, scheduled_at_ms: u64) -> bool {
        let _process_guard = cron::lock_cron_file().await;
        let Ok(_file_guard) =
            cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd).await
        else {
            return false;
        };
        let mut document = read_cron_tasks(self.fs.as_ref(), &self.cwd).await;
        let now = self.clock.now();
        let now_ms = now
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        let Some(task) = document.tasks.iter().find(|task| task.id == task_id) else {
            return false;
        };
        if !cron_task_active(task) {
            return false;
        }
        let expected = cron::next_fire_epoch_ms_for_persisted_task(task, now);
        if expected != Some(scheduled_at_ms) || scheduled_at_ms > now_ms {
            return false;
        }
        finalize_cron_occurrence(&mut document, &task_id, now_ms);
        cron::tasks_file::write_automation_tasks_body(
            self.fs.as_ref(),
            &self.cwd,
            &cron::serialize_tasks(&document),
        )
        .await
        .is_ok()
    }
}

pub(super) fn cron_task_active(task: &cron::CronTask) -> bool {
    task.automation
        .as_ref()
        .is_none_or(|automation| automation.status == cron::AutomationStatus::Active)
}

pub(super) fn decode_cron_automation(
    json: &str,
) -> Result<Option<cron::CronAutomation>, MobileEngineError> {
    if json.trim().is_empty() {
        return Ok(None);
    }
    let value: cron::CronAutomation = serde_json::from_str(json).map_err(|error| {
        MobileEngineError::Internal(format!("invalid automation settings: {error}"))
    })?;
    if value.version != 2 || value.model.trim().is_empty() {
        return Err(MobileEngineError::Internal(
            "automation version 2 and a model are required".into(),
        ));
    }
    Ok(Some(value))
}

pub(super) fn finalize_cron_occurrence(
    document: &mut cron::ScheduledTasks,
    task_id: &str,
    completed_at_ms: u64,
) {
    if let Some(task) = document.tasks.iter_mut().find(|task| task.id == task_id) {
        task.last_fired_at = Some(completed_at_ms);
        if !task.recurring.unwrap_or(false) {
            if let Some(automation) = &mut task.automation {
                automation.status = cron::AutomationStatus::Completed;
            } else {
                document.tasks.retain(|task| task.id != task_id);
            }
        }
    }
}
