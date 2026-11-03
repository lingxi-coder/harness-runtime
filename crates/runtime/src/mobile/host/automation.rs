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

use super::{MobileConfig, MobileEngineError, MobileEngineHandle};

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
    /// The turn may have executed, but its terminal record could not be
    /// confirmed. This occurrence must never be automatically replayed.
    OutcomeUnknown { message: String },
}

/// One fired-job record the Android service turns into a result notification.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone)]
pub struct FiredCronJobDto {
    /// Persisted result conversation, when execution created one.
    pub session_id: Option<String>,
    /// Exact source path emitted in the host's mobile path coordinates.
    pub session_transcript_path: Option<String>,
    /// Immutable capability mode of the result conversation.
    pub session_mode: Option<String>,
    /// Opaque app owner captured when this occurrence was claimed.
    pub session_scope_key: Option<String>,
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

/// A durable reservation for one native conversation owner. A reservation
/// does not imply that its scheduled turn was executed.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronOwnerReservationDto {
    pub session_id: String,
    pub session_transcript_path: String,
    pub session_mode: String,
    pub session_scope_key: String,
}

/// The candidate owner known before any child runtime or transcript is
/// created. A fresh UUID has no transcript path yet.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronOwnerCandidateDto {
    pub session_id: String,
    pub session_transcript_path: Option<String>,
    pub session_mode: String,
    pub session_scope_key: String,
}

/// Native hosts durably mirror the reservation under their short admission
/// gate before returning true. Rejection or cancellation never starts a turn.
#[cfg_attr(feature = "uniffi", uniffi::export(callback_interface))]
#[async_trait]
pub trait MobileCronOwnerAdmission: Send + Sync {
    /// Reserve this UUID in the host before any transcript or owner ledger is
    /// created. Other UUIDs remain available while the child is prepared.
    async fn begin_owner(&self, candidate: CronOwnerCandidateDto) -> bool;
    async fn confirm_owner(&self, reservation: CronOwnerReservationDto) -> bool;
}

/// A one-use barrier around the short interval in which a scheduled owner can
/// be persisted. It remains observable after the foreign fire future is
/// cancelled, even while the owned execution task is finishing its I/O.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct MobileCronAdmission {
    owner: Arc<dyn MobileCronOwnerAdmission>,
    state: std::sync::Mutex<MobileCronAdmissionState>,
    cancelled: CancellationToken,
    settled: tokio::sync::watch::Sender<bool>,
}

#[derive(Default)]
struct MobileCronAdmissionState {
    started: bool,
    active: bool,
    closed: bool,
    reservation: Option<CronOwnerReservationDto>,
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileCronAdmission {
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn new(owner: Box<dyn MobileCronOwnerAdmission>) -> Arc<Self> {
        let (settled, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            owner: Arc::from(owner),
            state: std::sync::Mutex::new(MobileCronAdmissionState::default()),
            cancelled: CancellationToken::new(),
            settled,
        })
    }

    /// Close this admission and wait until no task can write its reservation
    /// or still be executing the host callback. This does not wait for an
    /// already admitted model turn.
    pub async fn cancel_and_wait(&self) {
        let mut settled = self.settled.subscribe();
        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            self.cancelled.cancel();
            if !state.active {
                return;
            }
        }
        while !*settled.borrow_and_update() {
            if settled.changed().await.is_err() {
                break;
            }
        }
    }
}

impl MobileCronAdmission {
    pub(super) fn begin(self: &Arc<Self>) -> Option<MobileCronAdmissionLease> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed || state.started {
            return None;
        }
        state.started = true;
        state.active = true;
        Some(MobileCronAdmissionLease(self.clone(), None, None))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MobileCronSourceKey {
    run_mode: cron::RunMode,
    creation_mode: Option<String>,
    creation_scope: Option<String>,
    target_id: Option<String>,
    target_path: Option<String>,
    target_mode: Option<String>,
    target_scope: Option<String>,
    owned_id: Option<String>,
    owned_path: Option<String>,
    owned_mode: Option<String>,
    owned_scope: Option<String>,
}

impl From<&cron::CronAutomation> for MobileCronSourceKey {
    fn from(value: &cron::CronAutomation) -> Self {
        Self {
            run_mode: value.run_mode,
            creation_mode: value.session_mode.clone(),
            creation_scope: value.session_scope_key.clone(),
            target_id: value.target_session_id.clone(),
            target_path: value.target_session_transcript_path.clone(),
            target_mode: value.target_session_mode.clone(),
            target_scope: value.target_session_scope_key.clone(),
            owned_id: value.owned_session_id.clone(),
            owned_path: value.owned_session_transcript_path.clone(),
            owned_mode: value.owned_session_mode.clone(),
            owned_scope: value.owned_session_scope_key.clone(),
        }
    }
}

pub(super) struct MobileCronAdmissionLease(
    Arc<MobileCronAdmission>,
    Option<uuid::Uuid>,
    Option<MobileCronSourceKey>,
);

pub(super) const CRON_OWNER_RESERVATION_BUSY: &str =
    "busy: Scheduled session owner could not be reserved";

/// Reserve the UUID before waiting for the task claim lock. This owned task
/// outlives a cancelled FFI future, so its callback is covered by the barrier.
pub(super) async fn reserve_mobile_cron_owner(
    mut lease: MobileCronAdmissionLease,
    automation: &cron::CronAutomation,
) -> Result<MobileCronAdmissionLease, String> {
    cron::validate_automation_source(automation).map_err(|error| format!("paused: {error}"))?;
    let source_key = MobileCronSourceKey::from(automation);
    let id = match automation.run_mode {
        cron::RunMode::SelectedSession => automation.target_session_id.as_deref(),
        cron::RunMode::TaskSession => automation.owned_session_id.as_deref(),
        cron::RunMode::NewSession => None,
    };
    let id = id
        .map(|id| uuid::Uuid::parse_str(id.strip_prefix("sess:").unwrap_or(id)))
        .transpose()
        .map_err(|error| format!("paused: Invalid session: {error}"))?
        .unwrap_or_else(uuid::Uuid::new_v4);
    let (path, mode, scope) = match automation.run_mode {
        cron::RunMode::SelectedSession => (
            automation.target_session_transcript_path.clone(),
            automation.target_session_mode.clone(),
            automation.target_session_scope_key.clone(),
        ),
        cron::RunMode::TaskSession if automation.owned_session_id.is_some() => (
            automation.owned_session_transcript_path.clone(),
            automation.owned_session_mode.clone(),
            automation.owned_session_scope_key.clone(),
        ),
        _ => (
            None,
            automation.session_mode.clone(),
            automation.session_scope_key.clone(),
        ),
    };
    let candidate = CronOwnerCandidateDto {
        session_id: id.to_string(),
        session_transcript_path: path,
        session_mode: mode.ok_or("paused: Session mode is required")?,
        session_scope_key: scope.ok_or("paused: Session scope is required")?,
    };
    mobile_automation_runtime()
        .spawn(async move {
            if lease.0.cancelled.is_cancelled() {
                return Err("cancelled: Scheduled owner admission was cancelled".into());
            }
            let accepted = lease.0.owner.begin_owner(candidate).await;
            if lease.0.cancelled.is_cancelled() {
                return Err("cancelled: Scheduled owner admission was cancelled".into());
            }
            if !accepted {
                return Err(CRON_OWNER_RESERVATION_BUSY.into());
            }
            lease.1 = Some(id);
            lease.2 = Some(source_key);
            Ok(lease)
        })
        .await
        .map_err(|error| format!("scheduled owner preparation failed: {error}"))?
}

impl MobileCronAdmissionLease {
    pub(super) fn admission(&self) -> Arc<MobileCronAdmission> {
        self.0.clone()
    }

    fn remember_reservation(&self, reservation: CronOwnerReservationDto) {
        self.0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reservation = Some(reservation);
    }

    /// Linearize host confirmation with `cancel_and_wait`: whichever acquires
    /// this mutex first determines whether the model may be admitted.
    fn confirm(self, caller_cancel: &CancellationToken) -> bool {
        let allowed = {
            let mut state = self
                .0
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.closed || caller_cancel.is_cancelled() {
                false
            } else {
                state.active = false;
                let _ = self.0.settled.send(true);
                true
            }
        };
        allowed
    }
}

impl Drop for MobileCronAdmissionLease {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.active = false;
        let _ = self.0.settled.send(true);
    }
}

#[cfg(test)]
struct AllowCronOwner;

#[cfg(test)]
#[async_trait]
impl MobileCronOwnerAdmission for AllowCronOwner {
    async fn begin_owner(&self, _: CronOwnerCandidateDto) -> bool {
        true
    }
    async fn confirm_owner(&self, _: CronOwnerReservationDto) -> bool {
        true
    }
}

#[cfg(test)]
pub(super) fn allow_cron_owner() -> Arc<MobileCronAdmission> {
    MobileCronAdmission::new(Box::new(AllowCronOwner))
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
            // Scheduled runs assemble and drive complete mobile sessions on
            // this worker, just like the main mobile engine runtime. Use the
            // same supported budget instead of Tokio's smaller default stack.
            .thread_stack_size(crate::RUNTIME_THREAD_STACK_SIZE)
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
    supervise_mobile_automation_with_cancel(fs, cwd, clock, claim, move |request, cancel| async move {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err("cancelled: Scheduled execution was cancelled by the host".into()),
            result = execute(request) => result,
        }
    }, None).await
}

/// Production executions cooperate with cancellation and settle their owned
/// cleanup before a terminal cancellation is committed to the task ledger.
pub(super) async fn supervise_mobile_automation_with_cancel<F>(
    fs: Arc<dyn FileSystem>,
    cwd: std::path::PathBuf,
    clock: Arc<dyn Clock>,
    claim: impl std::future::Future<Output = Option<cron::AutomationRunRequest>> + Send + 'static,
    execute: impl FnOnce(cron::AutomationRunRequest, CancellationToken) -> F + Send + 'static,
    admission: Option<Arc<MobileCronAdmission>>,
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
        let result = if cancel.is_cancelled() {
            // The execution closure owns the admission lease. No reservation
            // will be written, so settle it before terminal bookkeeping.
            drop(execute);
            Err("cancelled: Scheduled execution was cancelled by the host".into())
        } else {
            let result = execute(request.clone(), cancel.clone()).await;
            if cancel.is_cancelled() {
                Err("cancelled: Scheduled execution was cancelled by the host".into())
            } else {
                result
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
            let source = admission.as_ref().and_then(|admission|
                admission.state.lock().unwrap_or_else(|error| error.into_inner()).reservation.clone()
            ).or_else(|| result.as_ref().ok().and_then(|value| Some(CronOwnerReservationDto {
                session_id: value.session_id.clone(),
                session_transcript_path: value.session_transcript_path.clone()?,
                session_mode: value.session_mode.clone()?,
                session_scope_key: value.session_scope_key.clone()?,
            })));
            let mut fired = fired_cron_dto(&request.task, Err("Scheduled outcome could not be confirmed".into()));
            fired.status = CronFireStatusDto::OutcomeUnknown { message: "Scheduled outcome could not be confirmed; do not replay this occurrence".into() };
            fired.retryable = false;
            if let Some(source) = source {
                fired.session_id = Some(source.session_id);
                fired.session_transcript_path = Some(source.session_transcript_path);
                fired.session_mode = Some(source.session_mode);
                fired.session_scope_key = Some(source.session_scope_key);
            }
            return Some(fired);
        };
        let finished = matches!(disposition, cron::AutomationFinishDisposition::Terminal);
        let result_session_id = result.as_ref().ok().map(|value| value.session_id.clone());
        let result_path = result.as_ref().ok().and_then(|value| value.session_transcript_path.clone());
        let result_mode = result.as_ref().ok().and_then(|value| value.session_mode.clone());
        let result_scope = result.as_ref().ok().and_then(|value| value.session_scope_key.clone());
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
        dto.session_id = persisted.as_ref().and_then(|run| run.session_id.clone()).or(result_session_id);
        dto.session_transcript_path = persisted.as_ref().and_then(|run| run.session_transcript_path.clone()).or(result_path);
        dto.session_mode = persisted.as_ref().and_then(|run| run.session_mode.clone()).or(result_mode);
        dto.session_scope_key = persisted.and_then(|run| run.session_scope_key).or(result_scope);
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

/// A caller can disappear while a scheduled child is still unwinding. Keep the
/// child task (and its session gate) alive, but request cancellation immediately.
async fn run_owned_mobile_cron<F, Fut>(operation: F) -> Result<cron::AutomationRunResult, String>
where
    F: FnOnce(CancellationToken) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<cron::AutomationRunResult, String>> + Send + 'static,
{
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    mobile_automation_runtime()
        .spawn(operation(cancel))
        .await
        .map_err(|error| format!("scheduled execution owner failed: {error}"))?
}

async fn drain_scheduled_child(runtime: &mut super::MobileRuntime) {
    if let Some(retirement) = runtime.retirement.take() {
        // The foreground reader remains fenced until task workers and command
        // handlers from this child can no longer write or launch more work.
        retirement.retire().await;
    }
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

fn bind_scheduled_media_route(
    route: &dyn orchestrator::ScheduledMediaChildRoute,
    parent_session: &str,
    child_session: &str,
    profile: &str,
    parent_is_current: orchestrator::ScheduledMediaParentCheck,
) -> Result<(), String> {
    route.bind_session(
        parent_session,
        child_session,
        profile,
        lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE,
        parent_is_current,
    )
}

impl MobileTurnFirer {
    pub(super) async fn fire_automation_with_cancel(
        &self,
        request: &cron::AutomationRunRequest,
        cancel: CancellationToken,
        owner_admission: MobileCronAdmissionLease,
    ) -> Result<cron::AutomationRunResult, String> {
        llm_runtime::catalog_task::scope(Box::pin(self.fire_session_scoped(
            request.task.prompt.clone(),
            request.task.automation.as_ref(),
            Some(request),
            cancel,
            Some(owner_admission),
        )))
        .await
    }

    /// Build a transient scheduled runtime with its own host-issued media
    /// attachment. The child cannot borrow the parent's owner namespace or
    /// conversation adapter, and parent revocation cascades through the
    /// attachment lifecycle.
    pub(super) fn scheduled_runtime_config(
        &self,
    ) -> Result<(MobileConfig, Option<ScheduledChildMedia>), String> {
        let mut config = self.cfg.clone();
        // The parent identity belongs only to its live source; each scheduled
        // child either creates a session or explicitly mounts its own target.
        config.initial_session_id = None;
        config.initial_session_transcript_path = None;
        config.scheduler_controller = false;
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
        self.fire_session_with_admission(prompt, automation, request, None)
            .await
    }

    pub(super) async fn fire_session_with_admission(
        &self,
        prompt: Utf16JsonProjection,
        automation: Option<&cron::CronAutomation>,
        request: Option<&cron::AutomationRunRequest>,
        owner_admission: Option<Arc<MobileCronAdmission>>,
    ) -> Result<cron::AutomationRunResult, String> {
        let owner = MobileTurnFirer {
            cfg: self.cfg.clone(),
            platform: self.platform.clone(),
        };
        let automation = automation.cloned();
        let request = request.cloned();
        let owner_admission = owner_admission
            .map(|owner| {
                owner
                    .begin()
                    .ok_or("cancelled: Scheduled admission is closed")
            })
            .transpose()?;
        let owner_admission = match owner_admission {
            Some(owner) => Some(
                reserve_mobile_cron_owner(
                    owner,
                    automation
                        .as_ref()
                        .ok_or("paused: Automation settings are required")?,
                )
                .await?,
            ),
            None => None,
        };
        run_owned_mobile_cron(move |cancel| async move {
            llm_runtime::catalog_task::scope(Box::pin(owner.fire_session_scoped(
                prompt,
                automation.as_ref(),
                request.as_ref(),
                cancel,
                owner_admission,
            )))
            .await
        })
        .await
    }

    async fn fire_session_scoped(
        &self,
        mut prompt: Utf16JsonProjection,
        automation: Option<&cron::CronAutomation>,
        request: Option<&cron::AutomationRunRequest>,
        cancel: CancellationToken,
        mut owner_admission: Option<MobileCronAdmissionLease>,
    ) -> Result<cron::AutomationRunResult, String> {
        if cancel.is_cancelled() {
            return Err("cancelled: Scheduled execution was cancelled by the host".into());
        }
        if let Some(automation) = automation {
            cron::validate_automation_source(automation)
                .map_err(|error| format!("paused: {error}"))?;
            if request.is_none() || owner_admission.is_none() {
                return Err("paused: Scheduled owner admission is required".into());
            }
        }
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
        let selected_uuid = match owner_admission.as_ref() {
            Some(owner) => {
                let reserved = owner.1.ok_or("paused: Scheduled owner was not reserved")?;
                if automation.map(MobileCronSourceKey::from) != owner.2 {
                    return Err("paused: Scheduled source changed after owner reservation".into());
                }
                if target_uuid.is_some_and(|id| id != reserved) {
                    return Err("paused: Scheduled owner changed after reservation".into());
                }
                reserved
            }
            None => target_uuid.unwrap_or_else(uuid::Uuid::new_v4),
        };
        let (mode, scope, selected_path) = match automation {
            Some(a) if a.run_mode == cron::RunMode::SelectedSession => (
                a.target_session_mode.as_deref(),
                a.target_session_scope_key.as_deref(),
                a.target_session_transcript_path.as_deref(),
            ),
            Some(a) if a.run_mode == cron::RunMode::TaskSession && target_uuid.is_some() => {
                if a.owned_session_mode != a.session_mode
                    || a.owned_session_scope_key != a.session_scope_key
                {
                    return Err("paused: Owned session mode or owner changed".into());
                }
                (
                    a.owned_session_mode.as_deref(),
                    a.owned_session_scope_key.as_deref(),
                    a.owned_session_transcript_path.as_deref(),
                )
            }
            Some(a) => (
                a.session_mode.as_deref(),
                a.session_scope_key.as_deref(),
                None,
            ),
            None => (Some(self.cfg.session_mode.as_str()), None, None),
        };
        let mode = mode
            .and_then(session::jsonl::SessionMode::from_str)
            .ok_or("paused: Invalid scheduled conversation mode")?;
        if target_uuid.is_some() && selected_path.is_none() {
            return Err("paused: Selected conversation has no exact source".into());
        }
        let captured_initial = if let Some(uuid) = target_uuid {
            let mut source_config = self.cfg.clone();
            source_config.initial_session_id = Some(uuid.to_string());
            source_config.initial_session_transcript_path =
                selected_path.map(std::path::PathBuf::from);
            source_config.session_mode = mode;
            Some(
                super::restoration::capture_initial_mobile_session(&source_config, &self.platform)
                    .await
                    .map_err(|error| format!("paused: Cannot restore conversation: {error}"))?
                    .ok_or("paused: Selected conversation has no source")?,
            )
        } else {
            None
        };
        let task_id = request.map(|r| r.task.id.as_str()).unwrap_or("scheduled");
        let title = automation.and_then(|a| a.name.as_deref()).unwrap_or("task");
        let workspace = match (automation, captured_initial.as_ref()) {
            (Some(a), Some(source)) if a.run_mode == cron::RunMode::SelectedSession => {
                super::scheduled_workspaces::ScheduledWorkspace::prepare_selected(
                    &self.cfg,
                    task_id,
                    title,
                    source.id,
                    &source.logical_cwd,
                    &source.replayed.transcript_path,
                )
            }
            (Some(_), _) => super::scheduled_workspaces::ScheduledWorkspace::prepare(
                &self.cfg,
                task_id,
                title,
                target_uuid,
            ),
            _ => Ok(None),
        }
        .map_err(|error| format!("paused: {error}"))?;
        let cwd = captured_initial
            .as_ref()
            .map(|source| source.logical_cwd.to_string_lossy().into_owned())
            .or_else(|| {
                workspace
                    .as_ref()
                    .map(|w| w.logical.to_string_lossy().into_owned())
            })
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
        let mut reader_transitions = Vec::new();
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
                if reader.lingxi_home == self.cfg.lingxi_home
                    && handle.current_session_id().await.as_uuid() == uuid
                {
                    let expected_path = captured_initial
                        .as_ref()
                        .map(|source| &source.replayed.transcript_path)
                        .ok_or("paused: Missing captured conversation source")?;
                    if !mobile_cron_reader_matches_source(&reader, &cwd, expected_path) {
                        return Err(
                            "busy: Conversation owner has a different source or workspace".into(),
                        );
                    }
                    // Foreground submission takes transition before the cron
                    // gate; never wait in the reverse order. Holding this lease
                    // also fences realtime and queued wake admission.
                    let transition = reader
                        .loop_transition
                        .clone()
                        .try_lock_owned()
                        .map_err(|_| "busy: Conversation is changing".to_string())?;
                    if handle.current_session_id().await.as_uuid() != uuid
                        || !mobile_cron_reader_matches_source(&reader, &cwd, expected_path)
                    {
                        return Err("busy: Conversation changed".into());
                    }
                    let captured = captured_initial
                        .as_ref()
                        .expect("selected source was captured");
                    let source_authority =
                        session::jsonl::writer::TranscriptFileAuthority::from_capture(
                            captured.transcript_root.clone(),
                            captured.transcript_relative.clone(),
                            captured.transcript_identity,
                            captured.transcript_file_identity,
                        )
                        .map_err(|error| format!("paused: Invalid session source: {error}"))?;
                    reader
                        .inner
                        .session_writer
                        .verify_active_transcript_source(&source_authority)
                        .map_err(|_| "busy: Conversation source identity changed".to_string())?;
                    if reader.active_cancel.lock().await.is_some() {
                        return Err("busy: Conversation has an active turn".into());
                    }
                    reader_transitions.push(transition);
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
        config.session_mode = mode;
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
        if let Some(uuid) = target_uuid {
            config.initial_session_id = Some(uuid.to_string());
            config.initial_session_transcript_path = selected_path.map(std::path::PathBuf::from);
        }
        if let Some(automation) = automation {
            config.default_model = automation.model.clone();
        }
        let borrowed_sessions = cleanup
            .readers
            .first()
            .map(|reader| reader.inner.transcript_sessions.clone());
        if let Some(owner) = &borrowed_sessions {
            if cleanup
                .readers
                .iter()
                .any(|reader| !Arc::ptr_eq(owner, &reader.inner.transcript_sessions))
            {
                return Err("busy: Multiple conversation owners".into());
            }
        }
        let planned_path = super::assembly::planned_mobile_transcript_path(
            &config,
            captured_initial.as_ref(),
            lingxi_core::types::SessionId::from_uuid(selected_uuid),
        );
        // Selected paths retain the host's exact coordinate spelling; new
        // paths are projected using the same mount table as the writer.
        let transcript_path = selected_path
            .map(str::to_owned)
            .unwrap_or_else(|| scheduled_transcript_path(&planned_path, self.platform.as_ref()));
        if cancel.is_cancelled()
            || owner_admission
                .as_ref()
                .is_some_and(|owner| owner.0.cancelled.is_cancelled())
        {
            return Err("cancelled: Scheduled owner admission was cancelled".into());
        }
        if let Some(request) = request {
            // Reserve durably before assembly can create a header or anchor.
            // A later build failure leaves an allocated, unexecuted receipt.
            cron::reserve_automation_owner(
                model_fs.as_ref(),
                std::path::Path::new(&store_cwd),
                request,
                &selected_uuid.to_string(),
                &transcript_path,
                mode.as_str(),
                scope,
            )
            .await?;
            if let Some(owner) = owner_admission.as_ref() {
                owner.remember_reservation(CronOwnerReservationDto {
                    session_id: selected_uuid.to_string(),
                    session_transcript_path: transcript_path.clone(),
                    session_mode: mode.as_str().to_owned(),
                    session_scope_key: scope.expect("validated owner scope").to_owned(),
                });
            }
        }
        // Keep the scheduled child assembly state off the enclosing session
        // future while retaining this task's ownership of cleanup and gates.
        let mut rt = Box::pin(super::assembly::build_mobile_inner_for_scheduled(
            config,
            self.platform.clone(),
            listener,
            sink,
            borrowed_sessions,
            captured_initial,
            target_uuid
                .is_none()
                .then(|| lingxi_core::types::SessionId::from_uuid(selected_uuid)),
        ))
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
        if cancel.is_cancelled() {
            drain_scheduled_child(&mut rt).await;
            return Err("cancelled: Scheduled execution was cancelled by the host".into());
        }

        let handle: Arc<dyn OrchestratorHandle> = rt.orchestrator.clone();
        let session_id = handle.current_session_id().await.as_uuid().to_string();
        let preparation = async {
            if session_id != selected_uuid.to_string() {
                return Err("paused: Scheduled session differs from the reserved owner".into());
            }
            if cancel.is_cancelled()
                || owner_admission
                    .as_ref()
                    .is_some_and(|lease| lease.0.cancelled.is_cancelled())
            {
                return Err("cancelled: Scheduled owner admission was cancelled".into());
            }
            let actual_mode = handle.conversation_mode().await;
            if actual_mode.session_id != session_id || actual_mode.mode != mode {
                return Err("paused: Scheduled session identity or mode changed".into());
            }
            if rt.session_writer.active_path() != planned_path {
                return Err("paused: Scheduled transcript differs from the reserved source".into());
            }
            if selected_path.is_none()
                && scheduled_transcript_path(
                    &rt.session_writer.active_path(),
                    self.platform.as_ref(),
                ) != transcript_path
            {
                return Err("paused: Scheduled transcript coordinates changed".into());
            }
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
            }
            if let Some(workspace) = &workspace {
                workspace.bind(&self.cfg.lingxi_home, &session_id)?;
            }
            if let Some(request) = request {
                cron::bind_automation_run_session_with_source(
                    model_fs.as_ref(),
                    std::path::Path::new(&store_cwd),
                    request,
                    &session_id,
                    &transcript_path,
                    mode.as_str(),
                    scope,
                )
                .await?;
            }
            Ok::<String, String>(transcript_path.clone())
        }
        .await;
        let transcript_path = match preparation {
            Ok(path) => path,
            Err(error) => {
                drain_scheduled_child(&mut rt).await;
                return Err(error);
            }
        };
        if automation.is_some() {
            let owner = owner_admission.as_ref().expect("validated owner admission");
            let reservation = CronOwnerReservationDto {
                session_id: session_id.clone(),
                session_transcript_path: transcript_path.clone(),
                session_mode: mode.as_str().to_owned(),
                session_scope_key: scope.expect("validated owner scope").to_owned(),
            };
            // Do not abandon an in-flight foreign callback: the barrier must
            // include the host's complete owner persistence, even on cancel.
            let allowed = if cancel.is_cancelled() || owner.0.cancelled.is_cancelled() {
                false
            } else {
                owner.0.owner.confirm_owner(reservation).await
            };
            if !allowed {
                drain_scheduled_child(&mut rt).await;
                return Err("cancelled: Scheduled owner admission was not confirmed".into());
            }
            // No more reservation writes or admission callbacks occur after
            // this point; do not hold the host gate for the model turn.
            if !owner_admission
                .take()
                .expect("validated owner admission")
                .confirm(&cancel)
            {
                drain_scheduled_child(&mut rt).await;
                return Err("cancelled: Scheduled owner admission was not confirmed".into());
            }
        }
        if cancel.is_cancelled() {
            drain_scheduled_child(&mut rt).await;
            return Err("cancelled: Scheduled execution was cancelled by the host".into());
        }
        let startup = rt
            .scheduled_startup
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(startup) = startup {
            if let Err(error) = startup.await {
                drain_scheduled_child(&mut rt).await;
                return Err(format!("paused: Scheduled session startup failed: {error}"));
            }
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
                        if let Err(error) = bind_scheduled_media_route(
                            route.as_ref(),
                            &parent_session,
                            &child_session,
                            &child_profile,
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
        let turn_cancel = cancel;
        let outcome = {
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

            tokio::pin!(run);
            tokio::select! {
                result = &mut run => Ok(result),
                () = tokio::time::sleep(CRON_TURN_TIMEOUT) => {
                    turn_cancel.cancel();
                    // Never release the borrowed session gate while child cleanup
                    // can still persist a prompt, result, or admission transaction.
                    let _ = run.await;
                    Err(())
                }
            }
        };
        drain_scheduled_child(&mut rt).await;
        let result = match outcome {
            Ok(Ok(_outcome)) => Ok(cron::AutomationRunResult {
                session_id,
                session_transcript_path: Some(transcript_path),
                session_mode: Some(mode.as_str().to_owned()),
                session_scope_key: scope.map(str::to_owned),
                summary: captured.lock().await.clone(),
            }),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("cron turn timed out".to_string()),
        };
        cleanup.revoke_media_attachment().await?;
        result
    }
}

pub(super) fn mobile_cron_reader_matches_source(
    reader: &MobileEngineHandle,
    cwd: &str,
    expected_path: &std::path::Path,
) -> bool {
    reader.session_cwd() == cwd
        && canonical_cwd_string(&reader.inner.session_writer.active_path())
            == canonical_cwd_string(expected_path)
}

fn scheduled_transcript_path(path: &std::path::Path, platform: &dyn Platform) -> String {
    if let Some(linux) = platform.mobile_linux() {
        let mut mounts = linux.current_mounts();
        for mount in &mut mounts {
            mount.host_path = std::path::PathBuf::from(canonical_cwd_string(&mount.host_path));
        }
        if let Some(path) = mobile_linux_api::map_host_path_to_guest(
            std::path::Path::new(&canonical_cwd_string(&path)),
            &mounts,
        ) {
            return path;
        }
    }
    path.to_string_lossy().into_owned()
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

    #[test]
    fn scheduled_route_forwards_session_identity_with_unbound_account_scope() {
        #[derive(Default)]
        struct Capture(StdMutex<Option<(String, String, String, String, bool)>>);
        impl orchestrator::ScheduledMediaChildRoute for Capture {
            fn bind_session(
                &self,
                parent: &str,
                child: &str,
                profile: &str,
                scope: &str,
                is_current: orchestrator::ScheduledMediaParentCheck,
            ) -> Result<(), String> {
                *self.0.lock().unwrap() = Some((
                    parent.into(),
                    child.into(),
                    profile.into(),
                    scope.into(),
                    is_current(),
                ));
                Ok(())
            }
            fn release_after_media_drain(&self) -> Result<(), String> {
                Ok(())
            }
        }
        let route = Capture::default();
        bind_scheduled_media_route(&route, "parent", "child", "anthropic", Arc::new(|| true))
            .unwrap();
        assert_eq!(
            *route.0.lock().unwrap(),
            Some((
                "parent".into(),
                "child".into(),
                "anthropic".into(),
                lingxi_core::host::provider_accounts::UNBOUND_ACCOUNT_SCOPE.into(),
                true,
            ))
        );
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
            session_transcript_path: None,
            session_mode: None,
            session_scope_key: None,
            id: task.id.clone(),
            prompt: source_display(&task.prompt),
            result_text: Some(text),
            status: CronFireStatusDto::Ok,
            retryable: false,
        },
        Err(message) => FiredCronJobDto {
            session_id: None,
            session_transcript_path: None,
            session_mode: None,
            session_scope_key: None,
            id: task.id.clone(),
            prompt: source_display(&task.prompt),
            result_text: None,
            retryable: cron_failure_is_retryable(&message),
            status: CronFireStatusDto::Failed { message },
        },
    }
}

pub(super) struct CronAdmissionSnapshot {
    pub(super) task: cron::CronTask,
    root_identity: Option<lingxi_core::host::rooted_fs::RootIdentity>,
    state_identity: Option<lingxi_core::host::rooted_fs::RootIdentity>,
    claimable: Option<bool>,
}

/// Read a task under the same process and file locks used by claim. Pin both
/// roots so an identical replacement cannot become an occurrence receipt.
pub(super) async fn cron_admission_snapshot(
    fs: &dyn FileSystem,
    cwd: &std::path::Path,
    id: &str,
    occurrence: Option<(CronAdmissionOccurrence, std::time::SystemTime)>,
) -> Result<Option<CronAdmissionSnapshot>, String> {
    let _process_guard = cron::lock_cron_file().await;
    let root_identity = fs
        .root_identity_no_follow(cwd)
        .await
        .map_err(|e| e.to_string())?;
    let task_path = cron::scheduled_tasks_path(cwd);
    let state_root = task_path.parent().ok_or("invalid scheduler state root")?;
    let state_identity = match fs.root_identity_no_follow(state_root).await {
        Ok(identity) => identity,
        Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let lock =
        cron::tasks_file::lock_automation_tasks_pinned(fs, state_root, state_identity.as_ref())
            .await
            .map_err(|e| e.to_string())?;
    lock.validate_current().map_err(|e| e.to_string())?;
    let body = match cron::tasks_file::read_automation_tasks_body_pinned(
        fs,
        state_root,
        state_identity.as_ref(),
    )
    .await
    {
        Ok(body) => body,
        Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let document = cron::tasks_file::parse_automation_tasks_strict(&body)?;
    if !document.unmodeled.is_empty() {
        return Err("scheduled task record is invalid".into());
    }
    lock.validate_current().map_err(|e| e.to_string())?;
    if fs
        .root_identity_no_follow(cwd)
        .await
        .map_err(|e| e.to_string())?
        != root_identity
        || fs
            .root_identity_no_follow(state_root)
            .await
            .map_err(|e| e.to_string())?
            != state_identity
    {
        return Err("scheduler scope changed during admission".into());
    }
    let mut tasks = document.tasks.into_iter().filter(|task| task.id == id);
    let task = tasks.next();
    if tasks.next().is_some() {
        return Err("duplicate scheduled task identity".into());
    }
    Ok(task.map(|task| {
        let claimable = occurrence.map(|(occurrence, now)| match occurrence {
            CronAdmissionOccurrence::Due(at) => {
                cron::can_claim_automation_run(&task, now, Some(at))
            }
            CronAdmissionOccurrence::ManualAt(at) => {
                cron::can_claim_automation_run_now(&task, now, Some(at))
            }
            CronAdmissionOccurrence::Manual => cron::can_claim_automation_run_now(&task, now, None),
        });
        CronAdmissionSnapshot {
            task,
            root_identity,
            state_identity,
            claimable,
        }
    }))
}

pub(super) enum CronAdmissionOccurrence {
    Due(u64),
    ManualAt(u64),
    Manual,
}

/// A rejected callback has not claimed a run itself. Recheck that this exact
/// occurrence was not claimed by another caller while the callback ran.
pub(super) async fn cron_owner_reservation_failure_dto(
    fs: &dyn FileSystem,
    cwd: &std::path::Path,
    snapshot: &CronAdmissionSnapshot,
    occurrence: CronAdmissionOccurrence,
    now: std::time::SystemTime,
    admission: &MobileCronAdmission,
    error: String,
) -> Option<FiredCronJobDto> {
    if error != CRON_OWNER_RESERVATION_BUSY {
        return Some(fired_cron_dto(&snapshot.task, Err(error)));
    }
    if admission.cancelled.is_cancelled() {
        return Some(fired_cron_dto(
            &snapshot.task,
            Err("cancelled: Scheduled owner admission was cancelled".into()),
        ));
    }
    let current =
        match cron_admission_snapshot(fs, cwd, &snapshot.task.id, Some((occurrence, now))).await {
            Ok(Some(current)) => current,
            Ok(None) => return Some(cron_admission_unknown_dto(snapshot.task.id.clone())),
            Err(_) => return Some(cron_admission_unknown_dto(snapshot.task.id.clone())),
        };
    if current.root_identity != snapshot.root_identity
        || current.state_identity != snapshot.state_identity
    {
        return Some(cron_admission_unknown_dto(snapshot.task.id.clone()));
    }
    if current.claimable != Some(true) {
        return None;
    }
    let mut initial = snapshot.task.clone();
    let mut latest = current.task;
    initial.last_fired_at = None;
    latest.last_fired_at = None;
    if let Some(config) = initial.automation.as_mut() {
        config.runs.clear();
    }
    if let Some(config) = latest.automation.as_mut() {
        config.runs.clear();
    }
    if initial != latest {
        return Some(cron_admission_unknown_dto(snapshot.task.id.clone()));
    }
    if admission.cancelled.is_cancelled() {
        return Some(fired_cron_dto(
            &snapshot.task,
            Err("cancelled: Scheduled owner admission was cancelled".into()),
        ));
    }
    let mut dto = fired_cron_dto(&snapshot.task, Err(error));
    dto.retryable = true;
    Some(dto)
}

/// The persisted occurrence cannot be verified; it may already have run.
pub(super) fn cron_admission_unknown_dto(id: String) -> FiredCronJobDto {
    let mut dto = busy_cron_dto(id);
    dto.status = CronFireStatusDto::OutcomeUnknown {
        message: "Scheduled occurrence could not be verified; do not replay it automatically"
            .into(),
    };
    dto.retryable = false;
    dto
}

/// No occurrence was claimed when the host transition was busy. The native
/// caller can retry the same scheduled timestamp after the foreground settles.
pub(super) fn busy_cron_dto(id: String) -> FiredCronJobDto {
    FiredCronJobDto {
        session_id: None,
        session_transcript_path: None,
        session_mode: None,
        session_scope_key: None,
        id,
        prompt: String::new(),
        result_text: None,
        status: CronFireStatusDto::Failed {
            message: "busy: Conversation is changing".into(),
        },
        retryable: true,
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

/// Mutations must never rewrite a damaged or partly unmodeled task document.
pub(super) async fn read_cron_tasks_for_mutation(
    fs: &dyn FileSystem,
    cwd: &std::path::Path,
    allow_absent: bool,
) -> Result<cron::ScheduledTasks, MobileEngineError> {
    let body = match cron::tasks_file::read_automation_tasks_body(fs, cwd).await {
        Ok(body) => body,
        Err(lingxi_core::host::FsError::NotFound(_)) if allow_absent => {
            return Ok(Default::default())
        }
        Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
    };
    let document = cron::tasks_file::parse_automation_tasks_strict(&body)
        .map_err(MobileEngineError::Internal)?;
    if !document.unmodeled.is_empty() {
        return Err(MobileEngineError::Internal(
            "scheduled task record is invalid".into(),
        ));
    }
    Ok(document)
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
}

#[cfg_attr(feature = "uniffi", uniffi::export(async_runtime = "tokio"))]
impl MobileCronStoreHandle {
    pub async fn list(&self) -> Vec<CronTaskDto> {
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

    /// Read the independent, non-pruned owner ledger for this scheduler root.
    /// An absent ledger is empty; incomplete or conflicting records fail closed.
    pub async fn list_owner_reservations_strict(
        &self,
    ) -> Result<Vec<CronOwnerReservationDto>, MobileEngineError> {
        cron::list_owner_reservations_strict(self.fs.as_ref(), &self.cwd)
            .await
            .map(|reservations| {
                reservations
                    .into_iter()
                    .map(|owner| CronOwnerReservationDto {
                        session_id: owner.session_id,
                        session_transcript_path: owner.session_transcript_path,
                        session_mode: owner.session_mode,
                        session_scope_key: owner.session_scope_key,
                    })
                    .collect()
            })
            .map_err(MobileEngineError::Internal)
    }

    /// Read durable execution receipts for host index recovery. A damaged or
    /// inaccessible store is an error; only an absent store is empty.
    pub async fn list_strict(&self) -> Result<Vec<CronTaskDto>, MobileEngineError> {
        let _guard = cron::lock_cron_file().await;
        let _file = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|error| MobileEngineError::Internal(error.to_string()))?;
        let body =
            match cron::tasks_file::read_automation_tasks_body(self.fs.as_ref(), &self.cwd).await {
                Ok(body) => body,
                Err(lingxi_core::host::FsError::NotFound(_)) => return Ok(Vec::new()),
                Err(error) => return Err(MobileEngineError::Internal(error.to_string())),
            };
        let document = cron::tasks_file::parse_automation_tasks_strict(&body)
            .map_err(MobileEngineError::Internal)?;
        if !document.unmodeled.is_empty() {
            return Err(MobileEngineError::Internal(
                "scheduled task record is invalid".into(),
            ));
        }
        for automation in document
            .tasks
            .iter()
            .filter_map(|task| task.automation.as_ref())
        {
            if !complete_or_empty_owner_source(
                automation.owned_session_id.as_deref(),
                automation.owned_session_transcript_path.as_deref(),
                automation.owned_session_mode.as_deref(),
                automation.owned_session_scope_key.as_deref(),
            ) {
                return Err(MobileEngineError::Internal(
                    "scheduled task owner is incomplete".into(),
                ));
            }
            for run in &automation.runs {
                if !complete_or_empty_owner_source(
                    run.session_id.as_deref(),
                    run.session_transcript_path.as_deref(),
                    run.session_mode.as_deref(),
                    run.session_scope_key.as_deref(),
                ) || (run.status == cron::AutomationRunStatus::Succeeded
                    && run.session_id.is_none())
                {
                    return Err(MobileEngineError::Internal(
                        "scheduled run owner is incomplete".into(),
                    ));
                }
            }
        }
        let now = self.clock.now();
        Ok(document
            .tasks
            .into_iter()
            .map(|task| cron_task_dto(task, now))
            .collect())
    }

    pub async fn create_configured(
        &self,
        cron_expr: String,
        prompt: String,
        recurring: bool,
        automation_json: String,
    ) -> Result<CronTaskDto, MobileEngineError> {
        let mut automation = decode_cron_automation(&automation_json)?.ok_or_else(|| {
            MobileEngineError::Internal("automation settings are required".into())
        })?;
        automation.runs.clear();
        automation.owned_session_id = None;
        automation.owned_session_transcript_path = None;
        automation.owned_session_mode = None;
        automation.owned_session_scope_key = None;
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
        let mut document = read_cron_tasks_for_mutation(self.fs.as_ref(), &self.cwd, true).await?;
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
            automation: Some(automation),
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

    pub async fn update_configured(
        &self,
        id: String,
        cron_expr: String,
        prompt: String,
        recurring: bool,
        automation_json: String,
    ) -> Result<CronTaskDto, MobileEngineError> {
        let mut automation = decode_cron_automation(&automation_json)?.ok_or_else(|| {
            MobileEngineError::Internal("automation settings are required".into())
        })?;
        if let Some(error) = mobile_cron_schedule_error(&cron_expr, recurring) {
            return Err(MobileEngineError::Internal(error));
        }
        let _process_guard = cron::lock_cron_file().await;
        let _file_guard = cron::tasks_file::lock_automation_tasks(self.fs.as_ref(), &self.cwd)
            .await
            .map_err(|error| {
                MobileEngineError::Internal(format!("lock scheduled_tasks.json: {error}"))
            })?;
        let mut document = read_cron_tasks_for_mutation(self.fs.as_ref(), &self.cwd, false).await?;
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
        if let Some(previous) = &task.automation {
            preserve_owned_automation(previous, &mut automation)?;
        } else {
            clear_owned_automation(&mut automation);
        }
        if automation.status != cron::AutomationStatus::Active {
            automation
                .runs
                .retain(|run| run.status != cron::AutomationRunStatus::Queued);
        }
        task.automation = Some(automation);
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
        let mut document = read_cron_tasks_for_mutation(self.fs.as_ref(), &self.cwd, false).await?;
        let task = document
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(MobileEngineError::NotFound)?;
        if let Some(previous) = &task.automation {
            preserve_owned_automation(previous, &mut automation)?;
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
        } else {
            clear_owned_automation(&mut automation);
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
        let Ok(mut document) =
            read_cron_tasks_for_mutation(self.fs.as_ref(), &self.cwd, false).await
        else {
            return false;
        };
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
        let Ok(mut document) =
            read_cron_tasks_for_mutation(self.fs.as_ref(), &self.cwd, false).await
        else {
            return false;
        };
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

fn complete_or_empty_owner_source(
    id: Option<&str>,
    path: Option<&str>,
    mode: Option<&str>,
    scope: Option<&str>,
) -> bool {
    match (id, path, mode, scope) {
        (None, None, None, None) => true,
        (Some(id), Some(path), Some(mode), Some(scope)) => {
            lingxi_core::types::SessionId::parse_prefixed(id).is_some()
                && std::path::Path::new(path).is_absolute()
                && matches!(mode, "chat" | "code")
                && !scope.trim().is_empty()
                && scope.len() <= 8192
        }
        _ => false,
    }
}

fn clear_owned_automation(automation: &mut cron::CronAutomation) {
    automation.runs.clear();
    automation.owned_session_id = None;
    automation.owned_session_transcript_path = None;
    automation.owned_session_mode = None;
    automation.owned_session_scope_key = None;
}

fn preserve_owned_automation(
    previous: &cron::CronAutomation,
    incoming: &mut cron::CronAutomation,
) -> Result<(), MobileEngineError> {
    incoming.runs = previous.runs.clone();
    if incoming.run_mode == cron::RunMode::TaskSession
        && previous.run_mode == cron::RunMode::TaskSession
        && previous.owned_session_id.is_some()
        && (incoming.session_mode != previous.owned_session_mode
            || incoming.session_scope_key != previous.owned_session_scope_key)
    {
        return Err(MobileEngineError::Internal(
            "Create a new task to change an owned session's mode or owner".into(),
        ));
    }
    let same_mode = incoming.run_mode == previous.run_mode;
    incoming.owned_session_id = same_mode
        .then(|| previous.owned_session_id.clone())
        .flatten();
    incoming.owned_session_transcript_path = same_mode
        .then(|| previous.owned_session_transcript_path.clone())
        .flatten();
    incoming.owned_session_mode = same_mode
        .then(|| previous.owned_session_mode.clone())
        .flatten();
    incoming.owned_session_scope_key = same_mode
        .then(|| previous.owned_session_scope_key.clone())
        .flatten();
    Ok(())
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
    cron::validate_automation_source(&value).map_err(MobileEngineError::Internal)?;
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

#[cfg(test)]
mod owned_cron_tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

    struct BlockingCandidate {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl MobileCronOwnerAdmission for BlockingCandidate {
        async fn begin_owner(&self, _: CronOwnerCandidateDto) -> bool {
            self.entered.notify_one();
            self.release.notified().await;
            true
        }
        async fn confirm_owner(&self, _: CronOwnerReservationDto) -> bool {
            panic!("cancelled candidate must never be confirmed")
        }
    }

    #[tokio::test]
    async fn cancelled_caller_waits_for_owned_begin_callback_before_barrier_settles() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let owner = MobileCronAdmission::new(Box::new(BlockingCandidate {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let automation: cron::CronAutomation = serde_json::from_value(serde_json::json!({
            "version":2, "status":"active", "model":"test/model", "reasoning":{"type":"automatic"},
            "runMode":"new_session", "sessionMode":"code", "sessionScopeKey":"project.test",
            "notificationPolicy":"all"
        }))
        .unwrap();
        let lease = owner.begin().unwrap();
        let caller =
            tokio::spawn(async move { reserve_mobile_cron_owner(lease, &automation).await });
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        caller.abort();
        let _ = caller.await;
        let first = tokio::spawn({
            let owner = owner.clone();
            async move { owner.cancel_and_wait().await }
        });
        let second = tokio::spawn({
            let owner = owner.clone();
            async move { owner.cancel_and_wait().await }
        });
        tokio::time::timeout(Duration::from_secs(2), owner.cancelled.cancelled())
            .await
            .unwrap();
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap();
        assert!(owner.begin().is_none());
    }

    #[tokio::test]
    async fn admission_barrier_waits_for_all_owned_work_and_closes_registration() {
        let admission = allow_cron_owner();
        let lease = admission.begin().expect("first registration");
        assert!(admission.begin().is_none());
        let first = tokio::spawn({
            let admission = admission.clone();
            async move { admission.cancel_and_wait().await }
        });
        let second = tokio::spawn({
            let admission = admission.clone();
            async move { admission.cancel_and_wait().await }
        });
        tokio::time::timeout(Duration::from_secs(2), admission.cancelled.cancelled())
            .await
            .unwrap();
        assert!(!first.is_finished());
        assert!(!second.is_finished());
        drop(lease);
        tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap();
        assert!(admission.begin().is_none());
    }

    #[tokio::test]
    async fn admission_barrier_closed_before_fire_rejects_late_registration() {
        let admission = allow_cron_owner();
        admission.cancel_and_wait().await;
        assert!(admission.begin().is_none());
    }

    #[tokio::test]
    async fn cancelled_caller_keeps_session_gate_until_cleanup_finishes() {
        let gate = Arc::new(Mutex::new(()));
        let entered = Arc::new(Notify::new());
        let cleaning = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let done = Arc::new(Notify::new());
        let caller = tokio::spawn({
            let gate = gate.clone();
            let entered = entered.clone();
            let cleaning = cleaning.clone();
            let release = release.clone();
            let done = done.clone();
            async move {
                run_owned_mobile_cron(move |cancel| async move {
                    let held = gate.lock().await;
                    entered.notify_one();
                    cancel.cancelled().await;
                    cleaning.notify_one();
                    release.notified().await;
                    drop(held);
                    done.notify_one();
                    Ok(cron::AutomationRunResult {
                        session_transcript_path: None,
                        session_mode: None,
                        session_scope_key: None,
                        session_id: "same".into(),
                        summary: String::new(),
                    })
                })
                .await
            }
        });
        entered.notified().await;
        caller.abort();
        let _ = caller.await;
        tokio::time::timeout(Duration::from_secs(2), cleaning.notified())
            .await
            .unwrap();
        assert!(
            gate.try_lock().is_err(),
            "the session gate must outlive the cancelled caller"
        );
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), done.notified())
            .await
            .unwrap();
        assert!(gate.try_lock().is_ok());
    }

    #[test]
    fn dropped_caller_runtime_cannot_abandon_the_scheduled_owner() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let gate = Arc::new(Mutex::new(()));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (cleaning_tx, cleaning_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let owned_gate = gate.clone();
        runtime.spawn(async move {
            let _ = run_owned_mobile_cron(move |cancel| async move {
                let held = owned_gate.lock().await;
                let _ = entered_tx.send(());
                cancel.cancelled().await;
                let _ = cleaning_tx.send(());
                let _ = release_rx.await;
                drop(held);
                let _ = done_tx.send(());
                Ok(cron::AutomationRunResult {
                    session_transcript_path: None,
                    session_mode: None,
                    session_scope_key: None,
                    session_id: "same".into(),
                    summary: String::new(),
                })
            })
            .await;
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        runtime.shutdown_timeout(Duration::from_secs(2));
        cleaning_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(gate.try_lock().is_err());
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(gate.try_lock().is_ok());
    }
}
