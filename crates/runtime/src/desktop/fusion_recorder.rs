//! Common durable terminal recorder for all production Fusion origins.
//!
//! Preparation only attaches this capability. The owned Fusion supervisor
//! calls it after computation facts are sealed and before task/UI watchers see
//! the terminal outcome. Agent and Workflow receive no parent transcript
//! target; Slash receives one target pinned by the trusted task/session claim.

use crate::desktop::session_state::{
    DurableFusionOutboxRecord, DurableFusionTerminalRecord, SessionStateCoordinator,
    SessionStateManager,
};
use async_trait::async_trait;
use platform_api::{
    FusionPublicationReceipt, FusionRunOutcome, FusionRunRecorder, FusionRunRecorderFactory,
    FusionSlashPublicationTarget, FusionStatus, OrchestratorHandle,
};
use serde_json::json;
use session::jsonl::{JsonlWriter, TranscriptAppendOutcome, TranscriptWriterError};
use std::collections::HashMap;
use std::sync::{Arc, RwLock, Weak};
use tokio::sync::Mutex;

/// Shared late-bound link from the recorder to the live orchestrator history.
pub type LiveHistoryLink = Arc<RwLock<Option<Weak<orchestrator::ConversationOrchestrator>>>>;

/// Approved transcript destination captured by the host before a run starts.
/// The ordinary writer and Fusion delivery share one in-process writer plus
/// the coordinator's durable session-state transaction. The active path is
/// resolved by that writer under the transaction, so `/cd` relocation cannot
/// leave a terminal append on a stale project path.
#[derive(Clone)]
pub struct FusionTranscriptTarget {
    writer: Arc<JsonlWriter>,
    history: LiveHistoryLink,
    session_id: Option<protocol::SessionId>,
}

impl FusionTranscriptTarget {
    /// Bind the shared production writer. Its durable lock must be configured
    /// before this target is attached; an unconfigured writer fails closed.
    #[must_use]
    pub fn new(writer: Arc<JsonlWriter>) -> Self {
        Self::with_history(writer, Arc::new(RwLock::new(None)))
    }

    /// Bind a shared live-history link.  The durable append remains the
    /// source of truth; this link only projects an already-persisted row into
    /// the active orchestrator when the originating session is still mounted.
    #[must_use]
    pub fn with_history(writer: Arc<JsonlWriter>, history: LiveHistoryLink) -> Self {
        Self {
            writer,
            history,
            session_id: None,
        }
    }

    /// Pin this target to its originating session. Late terminal work then
    /// cannot follow a newer active session's writer after a hot switch.
    #[must_use]
    pub fn for_session(mut self, session_id: protocol::SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    /// Attach the concrete orchestrator after the composition cycle closes.
    /// This is a pure pointer publication and performs no transcript I/O.
    pub fn attach_history(
        history: &LiveHistoryLink,
        orchestrator: &Arc<orchestrator::ConversationOrchestrator>,
    ) {
        *history
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(Arc::downgrade(orchestrator));
    }

    /// Attach the live orchestrator through this target's shared link.
    pub fn attach_orchestrator(&self, orchestrator: &Arc<orchestrator::ConversationOrchestrator>) {
        Self::attach_history(&self.history, orchestrator);
    }
}

/// Durable recorder pinned to one hydrated session coordinator.
#[derive(Clone)]
pub struct DesktopFusionRecorder {
    coordinator: Arc<SessionStateCoordinator>,
    transcript: Option<FusionTranscriptTarget>,
    delivery_locks: DeliveryLocks,
}

type DeliveryLocks = Arc<Mutex<HashMap<(protocol::SessionId, String), Arc<Mutex<()>>>>>;

/// How long a delivery owner waits for the foreground turn gate before giving
/// up on the in-memory history projection. The durable transcript row is
/// already written by then, and the projection is UUID-idempotent, so a later
/// owner can still add it. Generous, because a real turn holds that gate for
/// as long as the model takes.
const LIVE_PROJECTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Wait for every in-flight delivery to release its lock, but no longer than
/// `deadline`. A delivery that is still running when the budget expires keeps
/// its lock and its persisted receipt: abandoning the wait must never upgrade
/// a receipt this host did not observe delivered. The detached owner finishes
/// on its own and the next startup retry picks up whatever is still queued.
async fn await_delivery_locks(locks: &DeliveryLocks, deadline: tokio::time::Instant) {
    let Ok(snapshot) = tokio::time::timeout_at(deadline, locks.lock()).await else {
        return;
    };
    let snapshot = snapshot.values().cloned().collect::<Vec<_>>();
    for lock in snapshot {
        if tokio::time::timeout_at(deadline, lock.lock())
            .await
            .is_err()
        {
            tracing::warn!("Fusion delivery did not drain within the shutdown budget");
            return;
        }
    }
}

fn retry_delay_seconds(attempt: u64, cycle_start: u64) -> Option<u64> {
    let power = attempt.checked_sub(cycle_start)?;
    let power = u32::try_from(power).ok()?;
    1_u64.checked_shl(power)
}

/// Pure per-session recorder factory used by long-lived Bridge runtimes. The
/// manager lookup is synchronous and only returns an already-hydrated entry;
/// all I/O remains in the common terminal supervisor after preparation.
pub struct DesktopFusionRecorderFactory {
    manager: Arc<SessionStateManager>,
    transcript: FusionTranscriptTarget,
    delivery_locks: DeliveryLocks,
    recorders: std::sync::Mutex<HashMap<protocol::SessionId, Arc<DesktopFusionRecorder>>>,
}

impl DesktopFusionRecorderFactory {
    /// Bind the manager and shared active-path transcript target.
    #[must_use]
    pub fn new(manager: Arc<SessionStateManager>, transcript: FusionTranscriptTarget) -> Self {
        Self {
            manager,
            transcript,
            delivery_locks: Arc::new(Mutex::new(HashMap::new())),
            recorders: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Retry every mounted session's durable outbox. The factory retains one
    /// recorder per session so teardown can drain accepted deliveries without
    /// rerunning a Fusion executor.
    pub async fn retry_pending_all(&self) -> Vec<FusionPublicationReceipt> {
        // Hydration may have opened a session without a Fusion run in this
        // process (notably the boot session). Materialize those recorders from
        // the manager before taking the retry snapshot so startup/shutdown do
        // not silently omit a durable pending item.
        for session_id in self.manager.session_ids() {
            let _ = self.recorder_for_session(session_id);
        }
        let recorders: Vec<Arc<DesktopFusionRecorder>> = self
            .recorders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|recorder| recorder.pinned_view())
            .collect();
        let mut receipts = Vec::new();
        for recorder in recorders {
            receipts.extend(recorder.retry_pending().await);
        }
        receipts
    }

    /// Host-shutdown drain. Producers must already be stopped. First wait for
    /// every detached append/fsync that owns a delivery lock, then run normal
    /// automatic recovery and wait once more for any operation whose caller's
    /// five-second waiter elapsed. Dead letters remain explicit-local-only.
    pub async fn drain_pending_all(
        &self,
        deadline: tokio::time::Instant,
    ) -> Vec<FusionPublicationReceipt> {
        self.await_in_flight_deliveries(deadline).await;
        if tokio::time::timeout_at(deadline, self.retry_pending_all())
            .await
            .is_err()
        {
            tracing::warn!("Fusion outbox retry did not finish within the shutdown budget");
        }
        self.await_in_flight_deliveries(deadline).await;

        let mut receipts = Vec::new();
        for session_id in self.manager.session_ids() {
            if let Some(coordinator) = self.manager.coordinator(session_id) {
                receipts.extend(
                    coordinator
                        .fusion_outboxes()
                        .into_iter()
                        .map(|outbox| outbox.receipt),
                );
            }
        }
        receipts
    }

    async fn await_in_flight_deliveries(&self, deadline: tokio::time::Instant) {
        await_delivery_locks(&self.delivery_locks, deadline).await;
    }

    /// Return the concrete recorder for one already-hydrated session.
    #[must_use]
    pub fn recorder_for_session(
        &self,
        session_id: protocol::SessionId,
    ) -> Option<Arc<DesktopFusionRecorder>> {
        let coordinator = self.manager.coordinator_core(session_id)?;
        // Acquire the external capability before publishing a cache core.
        // Otherwise retirement could remove the manager entry between lookup
        // and insertion, leaving a stale closed recorder for the next mount.
        let pinned = coordinator.pinned_view().ok()?;
        let mut recorders = self
            .recorders
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let core = recorders.entry(session_id).or_insert_with(|| {
            Arc::new(
                DesktopFusionRecorder::new(
                    coordinator,
                    Some(self.transcript.clone().for_session(session_id)),
                )
                .with_delivery_locks(self.delivery_locks.clone()),
            )
        });
        if !core.coordinator.shares_authority(&pinned) {
            return None;
        }
        Some(Arc::new(DesktopFusionRecorder {
            coordinator: pinned,
            transcript: core.transcript.clone(),
            delivery_locks: core.delivery_locks.clone(),
        }))
    }

    pub(crate) async fn retire_cache(
        &self,
        session_id: protocol::SessionId,
        coordinator: &SessionStateCoordinator,
    ) -> Result<(), cost::CostPersistError> {
        {
            let mut recorders = self
                .recorders
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if recorders
                .get(&session_id)
                .is_some_and(|recorder| !recorder.coordinator.shares_authority(coordinator))
            {
                return Err(cost::CostPersistError::Rejected(
                    "recorder retirement authority changed".into(),
                ));
            }
            recorders.remove(&session_id);
        }
        // Every delivery task owns a pinned recorder view. Reaching retirement
        // therefore proves these session-specific lock entries are idle.
        self.delivery_locks
            .lock()
            .await
            .retain(|(owner, _), _| *owner != session_id);
        Ok(())
    }

    /// Explicit local retry for one stable run in the current hydrated
    /// session. This consults only the persisted outbox and never invokes a
    /// Fusion executor or provider.
    pub async fn retry_publication(
        &self,
        session_id: protocol::SessionId,
        run_id: &str,
    ) -> FusionPublicationReceipt {
        let Some(recorder) = self.recorder_for_session(session_id) else {
            return FusionPublicationReceipt::storage_failure(
                "Fusion publication session is not hydrated",
            );
        };
        recorder.retry_run_local(run_id).await
    }
}

impl FusionRunRecorderFactory for DesktopFusionRecorderFactory {
    fn recorder_for(&self, session_id: protocol::SessionId) -> Option<Arc<dyn FusionRunRecorder>> {
        let Some(recorder) = self.recorder_for_session(session_id) else {
            // The pure factory cannot hydrate or claim here. Return an
            // explicit terminal StorageFailure so an entrypoint never falls
            // through to the legacy current-session sink for an unmounted
            // origin.
            return Some(Arc::new(UnavailableFusionRecorder));
        };
        Some(recorder)
    }
}

/// Explicit no-persistence recorder factory. Returning a recorder rather than
/// `None` keeps Agent/Workflow terminal answers truthful without granting them
/// a durable parent-session Slash target.
#[derive(Debug, Default)]
pub struct UnavailableFusionRecorderFactory;

impl FusionRunRecorderFactory for UnavailableFusionRecorderFactory {
    fn recorder_for(&self, _session_id: protocol::SessionId) -> Option<Arc<dyn FusionRunRecorder>> {
        Some(Arc::new(UnavailableFusionRecorder))
    }
}

/// Explicit no-persistence projection for Agent/Workflow. Their computation
/// answer remains available, but the terminal receipt truthfully reports that
/// no durable ledger was available; this is not a fake NotRequired success.
#[derive(Debug, Default)]
pub struct UnavailableFusionRecorder;

#[async_trait]
impl FusionRunRecorder for UnavailableFusionRecorder {
    async fn record_terminal(
        &self,
        _outcome: FusionRunOutcome,
        _slash_target: Option<FusionSlashPublicationTarget>,
    ) -> FusionPublicationReceipt {
        FusionPublicationReceipt::storage_failure("session persistence is disabled")
    }
}

impl DesktopFusionRecorder {
    fn pinned_view(&self) -> Option<Arc<Self>> {
        Some(Arc::new(Self {
            coordinator: self.coordinator.pinned_view().ok()?,
            transcript: self.transcript.clone(),
            delivery_locks: self.delivery_locks.clone(),
        }))
    }
    /// Construct a recorder for a specific coordinator/lease. None transcript
    /// means Slash publication is unsupported and therefore fails closed;
    /// Agent/Workflow still record their terminal computation.
    #[must_use]
    pub fn new(
        coordinator: Arc<SessionStateCoordinator>,
        transcript: Option<FusionTranscriptTarget>,
    ) -> Self {
        Self {
            coordinator,
            transcript,
            delivery_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn with_delivery_locks(mut self, delivery_locks: DeliveryLocks) -> Self {
        self.delivery_locks = delivery_locks;
        self
    }

    /// Bounded wait for this recorder's own in-flight deliveries. Shares the
    /// factory's lock map when the factory created it.
    async fn await_in_flight_deliveries(&self, deadline: tokio::time::Instant) {
        await_delivery_locks(&self.delivery_locks, deadline).await;
    }

    async fn lock_for_delivery(&self, delivery_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.delivery_locks.lock().await;
        locks
            .entry((self.coordinator.session_id(), delivery_id.to_string()))
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn event_id(outcome: &FusionRunOutcome) -> String {
        format!("fusion-terminal:{}", outcome.identity.run_id)
    }

    fn message_uuid(outcome: &FusionRunOutcome) -> String {
        let suffix = outcome
            .identity
            .run_id
            .as_str()
            .strip_prefix("fu_")
            .unwrap_or("00000000000000000000000000000000");
        format!(
            "{}-{}-{}-{}-{}",
            &suffix[0..8],
            &suffix[8..12],
            &suffix[12..16],
            &suffix[16..20],
            &suffix[20..32]
        )
    }

    /// Build the transcript row for a run that is being published. Only a
    /// successful result reaches here: `record_terminal` suppresses the outbox
    /// for anything else, so there is no error body to render.
    fn transcript_payload(
        result: &platform_api::FusionResult,
        facts: &platform_api::FusionRunFacts,
        target: FusionSlashPublicationTarget,
        message_uuid: &str,
        cwd: &std::path::Path,
    ) -> serde_json::Value {
        let mut body = tasks::fusion_result_xml(result);
        if let Some(platform_api::FusionAttemptSettlementStatus::Failed { reason }) =
            &facts.attempt_settlement
        {
            body.push_str(&format!(
                "\n<fusion-accounting-error>{}</fusion-accounting-error>",
                tasks::escape_xml(reason)
            ));
        }
        // This is a transcript timestamp, not a persistence identity. It is
        // RFC3339 so the normal loader can order the row; retries reuse the
        // already-persisted payload rather than rebuilding it.
        let timestamp =
            session::jsonl::loader::format_rfc3339_seconds(std::time::SystemTime::now());
        json!({
            "parentUuid": serde_json::Value::Null,
            "isSidechain": false,
            "type": "user",
            "isMeta": true,
            "isModelContextExcluded": true,
            "uuid": message_uuid,
            "timestamp": timestamp,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd.to_string_lossy(),
            "sessionId": target.session_id.as_uuid().to_string(),
            "version": env!("CARGO_PKG_VERSION"),
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": body}],
            },
            "fusionRunId": result.run_id,
            "fusionStatus": match result.status {
                FusionStatus::Analyzed => "analyzed",
                FusionStatus::Unanalyzed => "unanalyzed",
            },
        })
    }

    async fn append_transcript(
        &self,
        outbox: &DurableFusionOutboxRecord,
    ) -> Result<TranscriptAppendOutcome, TranscriptWriterError> {
        let Some(target) = self.transcript.clone() else {
            return Err(TranscriptWriterError::Fs(platform_api::FsError::Io(
                "no trusted transcript target".into(),
            )));
        };
        let delivery_id = outbox.delivery_id.clone();
        let payload = outbox.payload.clone();
        let orchestrator = target
            .history
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade);
        let append = if let Some(session_id) = target.session_id {
            if let Some(orchestrator) = orchestrator {
                orchestrator
                    .append_fusion_transcript(
                        target.writer.as_ref(),
                        session_id,
                        &delivery_id,
                        payload,
                    )
                    .await?
            } else {
                target
                    .writer
                    .append_json_once_durable_for_session(session_id, &delivery_id, payload)
                    .await?
            }
        } else {
            target
                .writer
                .append_json_once_durable(&delivery_id, payload)
                .await?
        };
        Ok(append)
    }

    async fn project_published(&self, outbox: &DurableFusionOutboxRecord) {
        let Some(target) = self.transcript.as_ref() else {
            return;
        };
        let history = target
            .history
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(orchestrator) = history.and_then(|history| history.upgrade()) {
            let text = outbox
                .payload
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(|content| content.as_array())
                .and_then(|content| content.first())
                .and_then(|content| content.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            // This runs inside the delivery-lock owner, which survives the
            // public five-second waiter and is joined by host shutdown.  Wait
            // for the foreground turn gate instead of timing out and losing
            // the only live-history projection permanently. The projection is
            // UUID-idempotent, so a late owner cannot append it twice.
            // This waits on the foreground turn gate, which a long turn holds
            // legitimately, so the budget is generous: a busy history must
            // delay the projection, not lose it. It is still a budget, because
            // this owner holds the delivery lock -- a permanently wedged gate
            // would otherwise block every later retry of the same delivery.
            // Host shutdown does not depend on this bound: it abandons the
            // wait for the delivery lock on its own budget.
            match tokio::time::timeout(
                LIVE_PROJECTION_BUDGET,
                orchestrator.record_persisted_fusion_meta(
                    outbox.session_id,
                    &outbox.message_uuid,
                    text,
                ),
            )
            .await
            {
                Ok(Err(error)) => {
                    tracing::warn!(%error, "live Fusion history projection failed");
                }
                Err(_) => {
                    tracing::warn!("live Fusion history projection timed out");
                }
                Ok(Ok(())) => {}
            }

            if let Ok(current_session) = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                orchestrator.current_session_id(),
            )
            .await
            {
                let status = outbox
                    .payload
                    .get("fusionStatus")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown");
                let body = crate::desktop::fusion_command::fusion_persisted_notice(
                    status,
                    &outbox.session_id.to_string(),
                    current_session == outbox.session_id,
                );
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(250),
                    orchestrator.emit_background_system_notice(&body),
                )
                .await;
            }
        }
    }

    async fn deliver(&self, outbox: DurableFusionOutboxRecord) -> FusionPublicationReceipt {
        let delivery_lock = self.lock_for_delivery(&outbox.delivery_id).await;
        let waiter_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let Ok(_delivery_guard) =
            tokio::time::timeout_at(waiter_deadline, delivery_lock.clone().lock_owned()).await
        else {
            // A prior blocking append/fsync owns the durable writer. Do not
            // cancel it or launch an overlapping retry; leave the persisted
            // queued/failed item for the next local or startup retry.
            return FusionPublicationReceipt::queued();
        };
        // The candidate may have waited behind an earlier detached append.
        // Re-read the authoritative generation while holding the same delivery
        // lock: a late Published acknowledgement is absorbing, and an already
        // failed generation must never be replayed under the same attempt id.
        if let Some(latest) = self.coordinator.fusion_outbox(&outbox.delivery_id) {
            if latest.receipt.is_published()
                || latest.retry_cycle_end != outbox.retry_cycle_end
                || latest.attempt > outbox.attempt
                || (latest.attempt == outbox.attempt
                    && latest.receipt.status == platform_api::FusionPublicationStatus::OutboxFailed)
            {
                return latest.receipt;
            }
            if latest.attempt != outbox.attempt
                || outbox.attempt > outbox.retry_cycle_end
                || latest.receipt.status != platform_api::FusionPublicationStatus::Queued
            {
                return FusionPublicationReceipt::storage_failure(
                    "Fusion outbox dispatch does not own the durable queued generation",
                );
            }
        }
        // The filesystem worker is deliberately detached from the caller's
        // cancellation.  A `spawn_blocking` operation may already have
        // entered open/write/fsync when the terminal waiter is dropped; the
        // owned delivery guard must remain held until that operation records
        // its late acknowledgement, otherwise a local retry could overlap it.
        let delivery_id = outbox.delivery_id.clone();
        let recorder = self.clone();
        let operation = tokio::spawn(async move {
            let _delivery_guard = _delivery_guard;
            let mut outbox = outbox;
            match recorder.append_transcript(&outbox).await {
                Ok(_) => {
                    outbox.receipt = FusionPublicationReceipt::published();
                    if let Err(error) = recorder
                        .coordinator
                        .append_fusion_outbox(outbox.clone())
                        .await
                    {
                        return FusionPublicationReceipt::storage_failure(error.to_string());
                    }
                    recorder.project_published(&outbox).await;
                    FusionPublicationReceipt::published()
                }
                Err(error) => {
                    let reason = error.to_string();
                    outbox.receipt = FusionPublicationReceipt::outbox_failed(reason.clone());
                    if let Err(persist_error) =
                        recorder.coordinator.append_fusion_outbox(outbox).await
                    {
                        return FusionPublicationReceipt::storage_failure(
                            persist_error.to_string(),
                        );
                    }
                    FusionPublicationReceipt::outbox_failed(reason)
                }
            }
        });
        match tokio::time::timeout_at(waiter_deadline, operation).await {
            Ok(Ok(receipt)) => receipt,
            Ok(Err(error)) => FusionPublicationReceipt::storage_failure(format!(
                "Fusion delivery worker failed: {error}"
            )),
            Err(_) => self
                .coordinator
                .fusion_outbox(&delivery_id)
                .filter(|latest| latest.receipt.is_published())
                .map_or_else(FusionPublicationReceipt::queued, |latest| latest.receipt),
        }
    }

    /// Retry durable outbox items after boot or an explicit local retry. The
    /// coordinator already deduplicates payload/UUID identity, so this never
    /// reruns Fusion or overlaps an in-flight append for the same item. An
    /// exhausted attempt-4 failure is a durable dead letter: startup and host
    /// teardown leave it alone until an explicit local retry asks for another
    /// monotonic generation.
    pub async fn retry_pending(&self) -> Vec<FusionPublicationReceipt> {
        self.retry_pending_inner(false).await
    }

    /// Explicit local retry for one run. The run id is mapped to its stable
    /// delivery id; no prompt, model choice, or executor handle is accepted.
    pub async fn retry_run_local(&self, run_id: &str) -> FusionPublicationReceipt {
        let delivery_id = format!("fusion-delivery:{run_id}");
        let Some(outbox) = self.coordinator.fusion_outbox(&delivery_id) else {
            return FusionPublicationReceipt::storage_failure(
                "Fusion publication was not found in the current session",
            );
        };
        if outbox.receipt.is_published() {
            return outbox.receipt;
        }
        self.deliver_with_backoff(outbox, true).await
    }

    async fn retry_pending_inner(
        &self,
        include_dead_letters: bool,
    ) -> Vec<FusionPublicationReceipt> {
        let pending = self.coordinator.fusion_outboxes();
        let mut receipts = Vec::with_capacity(pending.len());
        for outbox in pending {
            if outbox.receipt.is_published()
                || (!include_dead_letters
                    && outbox.receipt.status == platform_api::FusionPublicationStatus::OutboxFailed
                    && outbox.attempt >= outbox.retry_cycle_end)
            {
                continue;
            }
            receipts.push(
                self.deliver_with_backoff(outbox, include_dead_letters)
                    .await,
            );
        }
        receipts
    }

    async fn deliver_with_backoff(
        &self,
        mut outbox: DurableFusionOutboxRecord,
        explicit_local_retry: bool,
    ) -> FusionPublicationReceipt {
        // A queued terminal is delivered at generation zero. A durable
        // failure starts the next local retry at the next checked generation;
        // this keeps repeated retries and late Published acknowledgments from
        // reusing an old mutation id.
        if outbox.receipt.is_published() {
            return outbox.receipt;
        }
        if outbox.receipt.status == platform_api::FusionPublicationStatus::OutboxFailed {
            let exhausted = outbox.attempt >= outbox.retry_cycle_end;
            if exhausted && !explicit_local_retry {
                return outbox.receipt;
            }
            outbox = match self.queue_next_attempt(outbox, exhausted).await {
                Ok(outbox) => outbox,
                Err(receipt) => return receipt,
            };
        }
        if outbox.receipt.status != platform_api::FusionPublicationStatus::Queued
            || outbox.attempt > outbox.retry_cycle_end
        {
            return FusionPublicationReceipt::storage_failure(
                "Fusion outbox has no dispatchable durable generation",
            );
        }
        let Some(cycle_start) = outbox.retry_cycle_end.checked_sub(4) else {
            return FusionPublicationReceipt::storage_failure(
                "Fusion outbox retry cycle is malformed",
            );
        };
        let owned_cycle_end = outbox.retry_cycle_end;
        loop {
            let receipt = self.deliver(outbox.clone()).await;
            if receipt.is_published()
                || receipt.status == platform_api::FusionPublicationStatus::StorageFailure
                || receipt.status == platform_api::FusionPublicationStatus::Queued
            {
                return receipt;
            }
            let Some(latest) = self.coordinator.fusion_outbox(&outbox.delivery_id) else {
                return FusionPublicationReceipt::storage_failure(
                    "Fusion outbox disappeared after a delivery attempt",
                );
            };
            if latest.retry_cycle_end != owned_cycle_end {
                // Another explicit owner started a newer durable cycle. It owns
                // that cycle's I/O; this stale automatic/local loop stops.
                return latest.receipt;
            }
            if latest.receipt.is_published() || latest.attempt >= owned_cycle_end {
                return latest.receipt;
            }
            if latest.receipt.status != platform_api::FusionPublicationStatus::OutboxFailed {
                return latest.receipt;
            }
            let Some(delay_seconds) = retry_delay_seconds(latest.attempt, cycle_start) else {
                return FusionPublicationReceipt::storage_failure(
                    "Fusion outbox retry delay is not representable",
                );
            };
            let failed_attempt = latest.attempt;
            tokio::time::sleep(std::time::Duration::from_secs(delay_seconds)).await;
            let Some(after_delay) = self.coordinator.fusion_outbox(&outbox.delivery_id) else {
                return FusionPublicationReceipt::storage_failure(
                    "Fusion outbox disappeared during retry backoff",
                );
            };
            if after_delay.retry_cycle_end != owned_cycle_end
                || after_delay.attempt != failed_attempt
                || after_delay.receipt.is_published()
                || after_delay.receipt.status == platform_api::FusionPublicationStatus::Queued
            {
                return after_delay.receipt;
            }
            outbox = match self.queue_next_attempt(after_delay, false).await {
                Ok(outbox) => outbox,
                Err(receipt) => return receipt,
            };
        }
    }

    /// Durably claim the next attempt before any transcript I/O. Extending a
    /// cycle is allowed only from its exhausted failed boundary; this record is
    /// the crash-replay marker that lets startup finish a partially attempted
    /// explicit-local cycle without rerunning Fusion.
    async fn queue_next_attempt(
        &self,
        mut outbox: DurableFusionOutboxRecord,
        extend_cycle: bool,
    ) -> Result<DurableFusionOutboxRecord, FusionPublicationReceipt> {
        let Some(next_attempt) = outbox.checked_next_attempt() else {
            return Err(FusionPublicationReceipt::storage_failure(
                "Fusion outbox retry generation overflow",
            ));
        };
        let retry_cycle_end = if extend_cycle {
            let Some(end) = outbox.attempt.checked_add(5) else {
                return Err(FusionPublicationReceipt::storage_failure(
                    "Fusion outbox retry cycle overflow",
                ));
            };
            end
        } else {
            outbox.retry_cycle_end
        };
        if next_attempt > retry_cycle_end {
            return Err(FusionPublicationReceipt::storage_failure(
                "Fusion outbox retry cycle is exhausted",
            ));
        }
        outbox.attempt = next_attempt;
        outbox.retry_cycle_end = retry_cycle_end;
        outbox.receipt = FusionPublicationReceipt::queued();
        self.coordinator
            .append_fusion_outbox(outbox.clone())
            .await
            .map_err(|error| FusionPublicationReceipt::storage_failure(error.to_string()))?;
        Ok(outbox)
    }

    fn same_terminal_immutable(
        persisted: &DurableFusionTerminalRecord,
        outcome: &FusionRunOutcome,
        target: Option<FusionSlashPublicationTarget>,
    ) -> bool {
        persisted.event_id == Self::event_id(outcome)
            && persisted.identity == outcome.identity
            && persisted.result == outcome.result
            && persisted.facts == outcome.facts
            && match (&persisted.outbox, target) {
                (None, None) => true,
                (Some(outbox), Some(target)) => {
                    outbox.delivery_id == format!("fusion-delivery:{}", outcome.identity.run_id)
                        && outbox.session_id == target.session_id
                        && outbox.message_uuid == Self::message_uuid(outcome)
                }
                _ => false,
            }
    }
}

#[async_trait]
impl FusionRunRecorder for DesktopFusionRecorder {
    async fn record_terminal(
        &self,
        outcome: FusionRunOutcome,
        slash_target: Option<FusionSlashPublicationTarget>,
    ) -> FusionPublicationReceipt {
        let event_id = Self::event_id(&outcome);
        let run_id = outcome.identity.run_id.clone();
        let session_id = outcome.identity.session_id;
        // Capture the computation error before serialization can replace it
        // with a persistence error. Do not log the outcome or its prompt data.
        if let Err(error) = &outcome.result {
            tracing::error!(
                %run_id,
                ?session_id,
                %error,
                "Fusion computation failed before terminal persistence"
            );
        }
        if slash_target.is_some_and(|target| Some(target.session_id) != outcome.identity.session_id)
        {
            return FusionPublicationReceipt::storage_failure(
                "Slash publication target does not match the trusted run identity",
            );
        }
        // Only caller-owned immutable inputs participate in replay validation.
        // cwd, timestamp, version and parent metadata belong to the first
        // persisted envelope and must survive relocation or later host changes.
        if let Some(existing) = self.coordinator.fusion_terminal(&event_id) {
            if !Self::same_terminal_immutable(&existing, &outcome, slash_target) {
                return FusionPublicationReceipt::storage_failure(
                    "conflicting Fusion terminal reused its event id",
                );
            }
            let Some(outbox) = existing.outbox else {
                return existing.publication;
            };
            let persisted = self
                .coordinator
                .fusion_outbox(&outbox.delivery_id)
                .unwrap_or(outbox);
            return self.deliver_with_backoff(persisted, false).await;
        }
        // Enforcement point for "Failed and Killed are never published": a run
        // that did not produce a result still gets its durable terminal record
        // below, but no outbox, no transcript row and no notice.
        let Some(target) = slash_target.filter(|_| outcome.result.is_ok()) else {
            let record = DurableFusionTerminalRecord {
                event_id: event_id.clone(),
                identity: outcome.identity,
                result: outcome.result,
                facts: outcome.facts,
                publication: FusionPublicationReceipt::not_required(),
                outbox: None,
            };
            return match self.coordinator.append_fusion_terminal(record).await {
                Ok(_) => FusionPublicationReceipt::not_required(),
                Err(error) => {
                    tracing::error!(
                        %run_id,
                        ?session_id,
                        %error,
                        "Fusion terminal persistence failed"
                    );
                    FusionPublicationReceipt::storage_failure(error.to_string())
                }
            };
        };
        let cwd = self
            .transcript
            .as_ref()
            .and_then(|transcript| transcript.writer.session_target_cwd(target.session_id))
            .unwrap_or_default();
        let message_uuid = Self::message_uuid(&outcome);
        let published = outcome
            .result
            .as_ref()
            .expect("publication is filtered to a successful result");
        let outbox = DurableFusionOutboxRecord {
            delivery_id: format!("fusion-delivery:{}", outcome.identity.run_id),
            session_id: target.session_id,
            message_uuid: message_uuid.clone(),
            payload: Self::transcript_payload(
                published,
                &outcome.facts,
                target,
                &message_uuid,
                &cwd,
            ),
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::queued(),
        };
        let record = DurableFusionTerminalRecord {
            event_id: event_id.clone(),
            identity: outcome.identity,
            result: outcome.result,
            facts: outcome.facts,
            publication: FusionPublicationReceipt::queued(),
            outbox: Some(outbox.clone()),
        };
        if let Err(error) = self.coordinator.append_fusion_terminal(record).await {
            tracing::error!(
                %run_id,
                ?session_id,
                %error,
                "Fusion terminal persistence failed"
            );
            return FusionPublicationReceipt::storage_failure(error.to_string());
        }
        self.deliver_with_backoff(outbox, false).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cost::CostHydrator as _;
    use platform_api::{
        DurableFusionTerminalRecord, FusionError, FusionOrigin, FusionResult, FusionRunFacts,
        FusionRunId, FusionRunIdentity, FusionStatus, FusionTiming, FusionUsage,
        SessionWriterLease,
    };

    #[derive(Debug)]
    struct TestLease(String);

    impl SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    async fn started_coordinator() -> (
        tempfile::TempDir,
        Arc<SessionStateCoordinator>,
        protocol::SessionId,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let session_id = protocol::SessionId::new();
        let coordinator = SessionStateCoordinator::open(
            directory.path(),
            session_id,
            Arc::new(TestLease(session_id.to_string())),
        )
        .unwrap();
        coordinator.start().await.unwrap();
        (directory, coordinator, session_id)
    }

    async fn exhausted_outbox(
        coordinator: &SessionStateCoordinator,
        session_id: protocol::SessionId,
        exhausted_at: u64,
    ) -> DurableFusionOutboxRecord {
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            Some(session_id),
            FusionOrigin::Slash,
            None,
        );
        let message_uuid = protocol::MessageId::new().as_uuid().to_string();
        let mut outbox = DurableFusionOutboxRecord {
            delivery_id: format!("fusion-delivery:{}", identity.run_id),
            session_id,
            message_uuid: message_uuid.clone(),
            payload: serde_json::json!({"uuid": message_uuid}),
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::queued(),
        };
        coordinator
            .append_fusion_terminal(DurableFusionTerminalRecord {
                event_id: format!("fusion-terminal:{}", identity.run_id),
                identity: identity.clone(),
                // A queued outbox only ever accompanies a successful result;
                // `record_terminal` suppresses publication for anything else.
                result: Ok(FusionResult {
                    schema_version: 1,
                    run_id: identity.run_id.to_string(),
                    status: FusionStatus::Analyzed,
                    analysis_failure: None,
                    analysis: None,
                    responses: vec![platform_api::PanelMaterial {
                        panel_id: "P1".into(),
                        summary: "summary".into(),
                        candidate_answer: "answer".into(),
                        claims: vec![],
                        evidence_checks: Default::default(),
                        risks: vec![],
                        unresolved_questions: vec![],
                    }],
                    panels: vec![],
                    usage: Default::default(),
                    timing: Default::default(),
                    egress_profiles: vec![],
                }),
                facts: FusionRunFacts::default(),
                publication: FusionPublicationReceipt::queued(),
                outbox: Some(outbox.clone()),
            })
            .await
            .unwrap();
        for attempt in 0..=exhausted_at {
            if attempt != 0 {
                outbox.attempt = attempt;
                outbox.receipt = FusionPublicationReceipt::queued();
                coordinator
                    .append_fusion_outbox(outbox.clone())
                    .await
                    .unwrap();
            }
            outbox.receipt = FusionPublicationReceipt::outbox_failed("fixture failure");
            coordinator
                .append_fusion_outbox(outbox.clone())
                .await
                .unwrap();
        }
        outbox
    }

    async fn wait_for_failed_attempt(
        coordinator: &SessionStateCoordinator,
        delivery_id: &str,
        attempt: u64,
    ) -> DurableFusionOutboxRecord {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(outbox) = coordinator.fusion_outbox(delivery_id) {
                    if outbox.attempt == attempt
                        && outbox.receipt.status
                            == platform_api::FusionPublicationStatus::OutboxFailed
                    {
                        return outbox;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("attempt transition")
    }

    async fn wait_for_outbox(
        coordinator: &SessionStateCoordinator,
        delivery_id: &str,
    ) -> DurableFusionOutboxRecord {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if let Some(outbox) = coordinator.fusion_outbox(delivery_id) {
                    return outbox;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("outbox publication")
    }

    fn completed_outcome(session_id: protocol::SessionId) -> FusionRunOutcome {
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            Some(session_id),
            FusionOrigin::Slash,
            None,
        );
        let control = platform_api::FusionRunControl::new(
            identity.clone(),
            1_000,
            tokio_util::sync::CancellationToken::new(),
            platform_api::FusionRunFactsRecorder::default(),
        );
        let mut outcome = FusionRunOutcome::from_control(
            &control,
            Ok(FusionResult {
                schema_version: 1,
                run_id: identity.run_id.to_string(),
                status: FusionStatus::Analyzed,
                analysis_failure: None,
                analysis: None,
                responses: vec![platform_api::PanelMaterial {
                    panel_id: "P1".into(),
                    summary: "summary".into(),
                    candidate_answer: "answer".into(),
                    claims: vec![],
                    evidence_checks: Default::default(),
                    risks: vec![],
                    unresolved_questions: vec![],
                }],
                panels: vec![],
                usage: FusionUsage::default(),
                timing: FusionTiming::default(),
                egress_profiles: vec![],
            }),
        );
        outcome.publication = FusionPublicationReceipt::pending();
        outcome
    }

    #[test]
    fn accounting_failure_is_disclosed_without_erasing_the_answer() {
        let session = protocol::SessionId::new();
        let mut outcome = completed_outcome(session);
        outcome.facts.attempt_settlement =
            Some(platform_api::FusionAttemptSettlementStatus::Failed {
                reason: "ledger <unavailable>".into(),
            });
        let payload = DesktopFusionRecorder::transcript_payload(
            outcome.result.as_ref().unwrap(),
            &outcome.facts,
            FusionSlashPublicationTarget {
                session_id: session,
            },
            &DesktopFusionRecorder::message_uuid(&outcome),
            std::path::Path::new("/test"),
        );
        let body = payload["message"]["content"][0]["text"].as_str().unwrap();
        // The user-facing row is the compact summary: the material itself
        // reaches the parent model through the task notification, so the
        // accounting disclosure must sit next to that summary, not erase it.
        assert!(body.contains("<fusion-result>"), "{body}");
        assert!(body.contains("<status>analyzed</status>"), "{body}");
        assert!(!body.contains("<answer>"), "{body}");
        assert!(body.contains(
            "<fusion-accounting-error>ledger &lt;unavailable&gt;</fusion-accounting-error>"
        ));
    }

    #[test]
    fn slash_payload_is_a_loadable_meta_user_transcript_row() {
        let session_id = protocol::SessionId::new();
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            Some(session_id),
            FusionOrigin::Slash,
            None,
        );
        let control = platform_api::FusionRunControl::new(
            identity.clone(),
            1_000,
            tokio_util::sync::CancellationToken::new(),
            platform_api::FusionRunFactsRecorder::default(),
        );
        let mut outcome = FusionRunOutcome::from_control(
            &control,
            Ok(FusionResult {
                schema_version: 1,
                run_id: identity.run_id.to_string(),
                status: FusionStatus::Analyzed,
                analysis_failure: None,
                analysis: None,
                responses: vec![platform_api::PanelMaterial {
                    panel_id: "P1".into(),
                    summary: "summary".into(),
                    candidate_answer: "answer".into(),
                    claims: vec![],
                    evidence_checks: Default::default(),
                    risks: vec![],
                    unresolved_questions: vec![],
                }],
                panels: vec![],
                usage: FusionUsage::default(),
                timing: FusionTiming::default(),
                egress_profiles: vec![],
            }),
        );
        outcome.publication = FusionPublicationReceipt::queued();
        let uuid = DesktopFusionRecorder::message_uuid(&outcome);
        let payload = DesktopFusionRecorder::transcript_payload(
            outcome.result.as_ref().unwrap(),
            &outcome.facts,
            FusionSlashPublicationTarget { session_id },
            &uuid,
            std::path::Path::new("/workspace"),
        );
        let parsed: session::jsonl::JsonlMessage = serde_json::from_value(payload).unwrap();
        assert_eq!(parsed.message_type, "user");
        assert_eq!(parsed.session_id, session_id.as_uuid().to_string());
        assert_eq!(parsed.uuid, uuid);
        assert_eq!(parsed.cwd, "/workspace");
        assert_eq!(parsed.extra.get("isMeta"), Some(&serde_json::json!(true)));
    }

    #[tokio::test]
    async fn duplicate_terminal_reuses_original_envelope_across_cwd_relocation() {
        for publish_first in [false, true] {
            let (directory, coordinator, session_id) = started_coordinator().await;
            let cwd_a = directory.path().join("workspace-a");
            let cwd_b = directory.path().join("workspace-b");
            std::fs::create_dir_all(&cwd_a).unwrap();
            std::fs::create_dir_all(&cwd_b).unwrap();
            let path_for = |cwd: &std::path::Path| {
                session::jsonl::path::session_path(
                    directory.path(),
                    &cwd.to_string_lossy(),
                    &session_id.as_uuid().to_string(),
                )
            };
            let fs: Arc<dyn platform_api::FileSystem> = Arc::new(
                platform_posix::fs::PosixFileSystem::new(directory.path().to_path_buf()),
            );
            let durable = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
                coordinator.journal().root().to_path_buf(),
                coordinator.journal().root_identity(),
            ));
            let writer =
                Arc::new(JsonlWriter::new(path_for(&cwd_a), fs).with_durable_lock(durable));
            writer
                .activate_session_target(session_id, path_for(&cwd_a), cwd_a.clone())
                .unwrap();
            let recorder = DesktopFusionRecorder::new(
                coordinator.clone(),
                Some(FusionTranscriptTarget::new(writer.clone()).for_session(session_id)),
            );
            let outcome = completed_outcome(session_id);
            let target = FusionSlashPublicationTarget { session_id };
            let event_id = DesktopFusionRecorder::event_id(&outcome);
            let uuid = DesktopFusionRecorder::message_uuid(&outcome);
            if publish_first {
                assert!(recorder
                    .record_terminal(outcome.clone(), Some(target))
                    .await
                    .is_published());
            } else {
                coordinator
                    .append_fusion_terminal(DurableFusionTerminalRecord {
                        event_id: event_id.clone(),
                        identity: outcome.identity.clone(),
                        result: outcome.result.clone(),
                        facts: outcome.facts.clone(),
                        publication: FusionPublicationReceipt::queued(),
                        outbox: Some(DurableFusionOutboxRecord {
                            delivery_id: format!("fusion-delivery:{}", outcome.identity.run_id),
                            session_id,
                            message_uuid: uuid.clone(),
                            payload: DesktopFusionRecorder::transcript_payload(
                                outcome.result.as_ref().unwrap(),
                                &outcome.facts,
                                target,
                                &uuid,
                                &cwd_a,
                            ),
                            attempt: 0,
                            retry_cycle_end: 4,
                            receipt: FusionPublicationReceipt::queued(),
                        }),
                    })
                    .await
                    .unwrap();
            }
            let original = coordinator.fusion_terminal(&event_id).unwrap();
            writer
                .retarget_with_relocation(
                    path_for(&cwd_b),
                    &session_id.as_uuid().to_string(),
                    &cwd_b.to_string_lossy(),
                )
                .await
                .unwrap();
            assert_eq!(writer.session_target_cwd(session_id), Some(cwd_b.clone()));
            let receipt = recorder
                .record_terminal(outcome.clone(), Some(target))
                .await;
            assert!(
                receipt.is_published(),
                "publish_first={publish_first}: {receipt:?}"
            );
            assert_eq!(coordinator.fusion_terminal(&event_id).unwrap(), original);
            let persisted = coordinator
                .fusion_outbox(&format!("fusion-delivery:{}", outcome.identity.run_id))
                .unwrap();
            assert_eq!(persisted.payload, original.outbox.as_ref().unwrap().payload);
            assert_eq!(persisted.message_uuid, uuid);
            assert_eq!(
                recorder
                    .record_terminal(outcome.clone(), Some(target))
                    .await,
                receipt
            );
            let body = std::fs::read_to_string(path_for(&cwd_b)).unwrap();
            assert_eq!(
                body.lines()
                    .filter(|line| {
                        serde_json::from_str::<serde_json::Value>(line)
                            .unwrap()
                            .get("uuid")
                            == Some(&json!(uuid))
                    })
                    .count(),
                1
            );
            assert_eq!(
                recorder.record_terminal(outcome.clone(), None).await.status,
                platform_api::FusionPublicationStatus::StorageFailure,
            );
            assert_eq!(
                recorder
                    .record_terminal(
                        outcome.clone(),
                        Some(FusionSlashPublicationTarget {
                            session_id: protocol::SessionId::new(),
                        })
                    )
                    .await
                    .status,
                platform_api::FusionPublicationStatus::StorageFailure,
            );
            let mut fact_conflict = outcome.clone();
            fact_conflict.facts.allocated_panels = Some(99);
            assert_eq!(
                recorder
                    .record_terminal(fact_conflict, Some(target))
                    .await
                    .status,
                platform_api::FusionPublicationStatus::StorageFailure,
            );
            let mut conflict = outcome;
            conflict.result.as_mut().unwrap().responses[0]
                .candidate_answer
                .push_str(" changed");
            assert_eq!(
                recorder
                    .record_terminal(conflict, Some(target))
                    .await
                    .status,
                platform_api::FusionPublicationStatus::StorageFailure
            );
            coordinator.close_and_drain().await.unwrap();
        }
    }

    #[tokio::test]
    async fn published_slash_row_loads_through_the_normal_resume_path() {
        let (directory, coordinator, session_id) = started_coordinator().await;
        let cwd = directory.path().join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let transcript_path = session::jsonl::path::session_path(
            directory.path(),
            &cwd.to_string_lossy(),
            &session_id.as_uuid().to_string(),
        );
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let fs: Arc<dyn platform_api::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer =
            Arc::new(JsonlWriter::new(transcript_path, fs.clone()).with_durable_lock(durable));
        writer
            .activate_session_target(session_id, writer.path().to_path_buf(), cwd.clone())
            .unwrap();
        let recorder = DesktopFusionRecorder::new(
            coordinator.clone(),
            Some(FusionTranscriptTarget::new(writer).for_session(session_id)),
        );
        let outcome = completed_outcome(session_id);
        let run_id = outcome.identity.run_id.to_string();
        let receipt = recorder
            .record_terminal(outcome, Some(FusionSlashPublicationTarget { session_id }))
            .await;
        assert!(receipt.is_published());

        let loaded = session::jsonl::load_session(
            directory.path(),
            &cwd.to_string_lossy(),
            session_id.as_uuid(),
            fs,
        )
        .await
        .expect("normal resume loader accepts durable Fusion row");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0]
                .extra
                .get("fusionRunId")
                .and_then(serde_json::Value::as_str),
            Some(run_id.as_str())
        );
        assert_eq!(
            loaded[0].extra.get("isModelContextExcluded"),
            Some(&serde_json::json!(true))
        );
        coordinator.close_and_drain().await.unwrap();
    }

    /// P0-5: shutdown must not wait forever on a delivery that is wedged.
    ///
    /// `await_in_flight_deliveries` took every delivery lock unconditionally,
    /// and the operation holding one waits on the foreground turn gate with no
    /// timeout. A turn stuck on a provider at quit time therefore parked the
    /// whole host barrier -- which every desktop exit path awaits.
    #[tokio::test(start_paused = true)]
    async fn shutdown_drain_abandons_a_wedged_delivery_within_its_budget() {
        let (_directory, coordinator, session_id) = started_coordinator().await;
        let recorder = DesktopFusionRecorder::new(coordinator.clone(), None);
        let outbox = exhausted_outbox(&coordinator, session_id, 4).await;
        // Hold the delivery lock for the whole test, the way a blocked
        // append/fsync owner does.
        let wedged = recorder.lock_for_delivery(&outbox.delivery_id).await;
        let _held = wedged.lock_owned().await;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let drain = tokio::spawn(async move {
            recorder.await_in_flight_deliveries(deadline).await;
        });
        tokio::time::advance(std::time::Duration::from_secs(3)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), drain)
                .await
                .is_ok(),
            "the drain never returned while a delivery lock was held"
        );
        // The wedged item is still queued: abandoning the wait must never
        // upgrade a receipt it did not deliver.
        let persisted = coordinator
            .fusion_outbox(&outbox.delivery_id)
            .expect("the outbox survives an abandoned drain");
        assert!(
            !persisted.receipt.is_published(),
            "an abandoned delivery must not claim publication: {:?}",
            persisted.receipt
        );
        coordinator.close_and_drain().await.unwrap();
    }

    /// P0-8: a killed or failed run leaves a durable terminal record but must
    /// publish nothing. Before this, `record_terminal` built a slash outbox for
    /// any result, so cancelling a run wrote a `<fusion-error>` row into the
    /// user's transcript and pushed a "Fusion run failed" notice -- including
    /// for a prepared run the registry dropped before the user ever saw it.
    #[tokio::test]
    async fn a_killed_run_records_its_terminal_but_publishes_nothing() {
        let (_directory, coordinator, session_id) = started_coordinator().await;
        let recorder = DesktopFusionRecorder::new(coordinator.clone(), None);
        let mut outcome = completed_outcome(session_id);
        outcome.result = Err(FusionError::Cancelled);
        let run_id = outcome.identity.run_id.to_string();

        let receipt = recorder
            .record_terminal(outcome, Some(FusionSlashPublicationTarget { session_id }))
            .await;

        assert_eq!(
            receipt,
            FusionPublicationReceipt::not_required(),
            "a killed run has nothing to publish"
        );
        assert!(
            coordinator
                .fusion_outbox(&format!("fusion-delivery:{run_id}"))
                .is_none(),
            "no delivery may be queued for a killed run"
        );
        let terminal = coordinator
            .fusion_terminal(&format!("fusion-terminal:{run_id}"))
            .expect("the terminal record is still durable");
        assert!(terminal.outbox.is_none());
        assert!(terminal.result.is_err());
        coordinator.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn restart_resumes_a_partially_failed_explicit_retry_cycle() {
        let (directory, coordinator, session_id) = started_coordinator().await;
        let recorder = DesktopFusionRecorder::new(coordinator.clone(), None);
        let failed = exhausted_outbox(&coordinator, session_id, 4).await;
        let mut first_local = recorder
            .queue_next_attempt(failed, true)
            .await
            .expect("open explicit cycle");
        assert_eq!((first_local.attempt, first_local.retry_cycle_end), (5, 9));
        first_local.receipt = FusionPublicationReceipt::outbox_failed("crashed local attempt");
        coordinator
            .append_fusion_outbox(first_local.clone())
            .await
            .unwrap();
        drop(recorder);
        coordinator.close_and_drain().await.unwrap();
        drop(coordinator);

        let coordinator = SessionStateCoordinator::open(
            directory.path(),
            session_id,
            Arc::new(TestLease(session_id.to_string())),
        )
        .unwrap();
        coordinator.start().await.unwrap();
        coordinator.hydrate(session_id).await.unwrap();
        let restarted = DesktopFusionRecorder::new(coordinator.clone(), None);
        let recovery = tokio::spawn(async move { restarted.retry_pending().await });
        let resumed = wait_for_failed_attempt(&coordinator, &first_local.delivery_id, 6).await;
        assert_eq!(resumed.retry_cycle_end, 9);
        recovery.abort();
        let _ = recovery.await;
    }

    #[tokio::test]
    async fn five_real_delivery_failures_deadletter_then_local_retry_uses_no_executor() {
        let (directory, coordinator, session_id) = started_coordinator().await;
        let blocked_parent = directory.path().join("blocked-parent");
        std::fs::write(&blocked_parent, "not a directory").unwrap();
        let transcript_path = blocked_parent.join("session.jsonl");
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let fs: Arc<dyn platform_api::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer =
            Arc::new(JsonlWriter::new(transcript_path.clone(), fs).with_durable_lock(durable));
        writer
            .activate_session_target(session_id, transcript_path, directory.path().to_path_buf())
            .unwrap();
        let recorder = DesktopFusionRecorder::new(
            coordinator.clone(),
            Some(FusionTranscriptTarget::new(writer).for_session(session_id)),
        );
        let outcome = completed_outcome(session_id);
        let message_uuid = DesktopFusionRecorder::message_uuid(&outcome);
        let mut outbox = DurableFusionOutboxRecord {
            delivery_id: format!("fusion-delivery:{}", outcome.identity.run_id),
            session_id,
            message_uuid: message_uuid.clone(),
            payload: DesktopFusionRecorder::transcript_payload(
                outcome.result.as_ref().unwrap(),
                &outcome.facts,
                FusionSlashPublicationTarget { session_id },
                &message_uuid,
                directory.path(),
            ),
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::queued(),
        };
        coordinator
            .append_fusion_terminal(DurableFusionTerminalRecord {
                event_id: format!("fusion-terminal:{}", outcome.identity.run_id),
                identity: outcome.identity,
                result: outcome.result,
                facts: outcome.facts,
                publication: FusionPublicationReceipt::queued(),
                outbox: Some(outbox.clone()),
            })
            .await
            .unwrap();

        for expected_attempt in 0..=4 {
            let receipt = recorder.deliver(outbox.clone()).await;
            assert_eq!(
                receipt.status,
                platform_api::FusionPublicationStatus::OutboxFailed
            );
            outbox = coordinator
                .fusion_outbox(&outbox.delivery_id)
                .expect("failed attempt persisted");
            assert_eq!(outbox.attempt, expected_attempt);
            if expected_attempt < 4 {
                outbox = recorder
                    .queue_next_attempt(outbox, false)
                    .await
                    .expect("queue next automatic attempt");
            }
        }
        assert_eq!(outbox.retry_cycle_end, 4);
        assert!(
            recorder.retry_pending().await.is_empty(),
            "startup recovery must not dispatch an exhausted cycle"
        );

        let local = recorder
            .queue_next_attempt(outbox, true)
            .await
            .expect("explicit local retry opens a new durable cycle");
        assert_eq!((local.attempt, local.retry_cycle_end), (5, 9));
        let receipt = recorder.deliver(local.clone()).await;
        assert_eq!(
            receipt.status,
            platform_api::FusionPublicationStatus::OutboxFailed
        );
        let persisted = coordinator.fusion_outbox(&local.delivery_id).unwrap();
        assert_eq!((persisted.attempt, persisted.retry_cycle_end), (5, 9));
        coordinator.close_and_drain().await.unwrap();
    }

    #[test]
    fn retry_cycle_has_five_attempts_at_zero_one_two_four_eight_seconds() {
        let mut elapsed = 0_u64;
        let mut dispatch_times = vec![elapsed];
        for failed_attempt in 0..4 {
            elapsed += retry_delay_seconds(failed_attempt, 0).expect("bounded retry delay");
            dispatch_times.push(elapsed);
        }
        assert_eq!(dispatch_times, vec![0, 1, 3, 7, 15]);
        assert_eq!(
            [
                0,
                retry_delay_seconds(0, 0).unwrap(),
                retry_delay_seconds(1, 0).unwrap(),
                retry_delay_seconds(2, 0).unwrap(),
                retry_delay_seconds(3, 0).unwrap(),
            ],
            [0, 1, 2, 4, 8],
            "the five dispatches use immediate/1/2/4/8-second backoff slots"
        );
    }

    #[tokio::test]
    async fn older_retry_loop_stops_when_another_owner_advances_its_attempt() {
        let (_directory, coordinator, session_id) = started_coordinator().await;
        let initial = exhausted_outbox(&coordinator, session_id, 0).await;
        let delivery_id = initial.delivery_id.clone();
        let automatic_recorder = DesktopFusionRecorder::new(coordinator.clone(), None);
        let automatic = tokio::spawn(async move { automatic_recorder.retry_pending().await });
        wait_for_failed_attempt(&coordinator, &delivery_id, 1).await;

        let local_recorder = DesktopFusionRecorder::new(coordinator.clone(), None);
        let local_run_id = delivery_id
            .strip_prefix("fusion-delivery:")
            .expect("delivery id")
            .to_string();
        let local =
            tokio::spawn(async move { local_recorder.retry_run_local(&local_run_id).await });
        wait_for_failed_attempt(&coordinator, &delivery_id, 2).await;
        local.abort();
        let _ = local.await;

        tokio::time::timeout(std::time::Duration::from_secs(3), automatic)
            .await
            .expect("stale automatic loop must stop after its backoff")
            .expect("automatic task");
        let latest = coordinator.fusion_outbox(&delivery_id).unwrap();
        assert_eq!(
            latest.attempt, 2,
            "the stale loop must not dispatch attempt 3"
        );
    }

    #[tokio::test]
    async fn waiter_timeout_retains_one_inflight_append_and_accepts_its_late_ack() {
        let (directory, coordinator, session_id) = started_coordinator().await;
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let transcript_path = directory
            .path()
            .join(format!("{}.jsonl", session_id.as_uuid()));
        let fs: Arc<dyn platform_api::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer = Arc::new(
            JsonlWriter::new(transcript_path.clone(), fs).with_durable_lock(durable.clone()),
        );
        writer
            .activate_session_target(
                session_id,
                transcript_path.clone(),
                directory.path().to_path_buf(),
            )
            .unwrap();
        let recorder = Arc::new(DesktopFusionRecorder::new(
            coordinator.clone(),
            Some(FusionTranscriptTarget::new(writer).for_session(session_id)),
        ));
        let outcome = completed_outcome(session_id);
        let run_id = outcome.identity.run_id.to_string();
        let delivery_id = format!("fusion-delivery:{run_id}");

        // Hold the exact cross-process transcript transaction. The recorder's
        // filesystem worker enters a real flock wait, not a synthetic sleep.
        let transaction = durable.begin_transaction().unwrap();
        let first_recorder = recorder.clone();
        let first = tokio::spawn(async move {
            first_recorder
                .record_terminal(outcome, Some(FusionSlashPublicationTarget { session_id }))
                .await
        });
        let queued = wait_for_outbox(&coordinator, &delivery_id).await;
        assert_eq!(queued.attempt, 0);
        let retry_recorder = recorder.clone();
        let retry_run_id = run_id.clone();
        let retry =
            tokio::spawn(async move { retry_recorder.retry_run_local(&retry_run_id).await });

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(6), first)
                .await
                .expect("terminal waiter is bounded")
                .expect("terminal task")
                .status,
            platform_api::FusionPublicationStatus::Queued
        );
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), retry)
                .await
                .expect("overlapping retry shares the same five-second window")
                .expect("retry task")
                .status,
            platform_api::FusionPublicationStatus::Queued
        );

        drop(transaction);
        let delivery_lock = recorder.lock_for_delivery(&delivery_id).await;
        let settled = tokio::time::timeout(std::time::Duration::from_secs(2), delivery_lock.lock())
            .await
            .expect("late filesystem owner settles");
        drop(settled);
        let latest = coordinator.fusion_outbox(&delivery_id).unwrap();
        assert_eq!(
            latest.receipt.status,
            platform_api::FusionPublicationStatus::Published
        );
        assert_eq!(
            latest.attempt, 0,
            "the waiting retry must not create an attempt"
        );
        assert_eq!(
            std::fs::read_to_string(transcript_path)
                .unwrap()
                .lines()
                .count(),
            1,
            "the retained append and retry share one physical delivery"
        );
    }

    #[tokio::test]
    async fn delivery_lock_wait_and_filesystem_wait_share_one_five_second_deadline() {
        let (directory, coordinator, session_id) = started_coordinator().await;
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let transcript_path = directory.path().join("deadline.jsonl");
        let fs: Arc<dyn platform_api::FileSystem> = Arc::new(
            platform_posix::fs::PosixFileSystem::new(directory.path().to_path_buf()),
        );
        let writer = Arc::new(
            JsonlWriter::new(transcript_path.clone(), fs).with_durable_lock(durable.clone()),
        );
        writer
            .activate_session_target(session_id, transcript_path, directory.path().to_path_buf())
            .unwrap();
        let recorder = Arc::new(DesktopFusionRecorder::new(
            coordinator.clone(),
            Some(FusionTranscriptTarget::new(writer).for_session(session_id)),
        ));
        let failed = exhausted_outbox(&coordinator, session_id, 0).await;
        let queued = recorder
            .queue_next_attempt(failed, false)
            .await
            .expect("queue attempt");
        let delivery_id = queued.delivery_id.clone();
        let delivery_lock = recorder.lock_for_delivery(&queued.delivery_id).await;
        let held_delivery = delivery_lock.clone().lock_owned().await;
        let held_transcript = durable.begin_transaction().unwrap();
        let started = tokio::time::Instant::now();
        let attempt_recorder = recorder.clone();
        let attempt = tokio::spawn(async move { attempt_recorder.deliver(queued).await });
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;
        drop(held_delivery);

        let receipt = tokio::time::timeout(std::time::Duration::from_secs(2), attempt)
            .await
            .expect("only the unspent waiter budget remains")
            .expect("delivery task");
        assert_eq!(
            receipt.status,
            platform_api::FusionPublicationStatus::Queued
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(6),
            "lock and filesystem waits must not each receive a fresh five seconds"
        );

        drop(held_transcript);
        let settled = tokio::time::timeout(std::time::Duration::from_secs(2), delivery_lock.lock())
            .await
            .expect("retained filesystem owner settles");
        drop(settled);
        assert_eq!(
            coordinator
                .fusion_outbox(&delivery_id)
                .expect("latest outbox")
                .receipt
                .status,
            platform_api::FusionPublicationStatus::Published
        );
    }

    #[tokio::test]
    async fn published_projection_waits_for_busy_history_and_remains_uuid_idempotent() {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        use std::path::PathBuf;
        use tool_api::registry::ToolRegistry;

        let (_directory, coordinator, session_id) = started_coordinator().await;
        let orchestrator = Arc::new(
            orchestrator::ConversationOrchestrator::new(
                orchestrator::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/workspace"),
            )
            .with_session_id(session_id),
        );
        let history = Arc::new(RwLock::new(None));
        FusionTranscriptTarget::attach_history(&history, &orchestrator);
        let writer = Arc::new(JsonlWriter::new(
            PathBuf::from("/unused.jsonl"),
            Arc::new(platform_posix::fs::PosixFileSystem::new(PathBuf::from(
                "/workspace",
            ))),
        ));
        let recorder = DesktopFusionRecorder::new(
            coordinator,
            Some(FusionTranscriptTarget::with_history(writer, history).for_session(session_id)),
        );
        let message_id = protocol::MessageId::new();
        let message_uuid = message_id.as_uuid().to_string();
        let outbox = DurableFusionOutboxRecord {
            delivery_id: "fusion-delivery:projection".into(),
            session_id,
            message_uuid: message_uuid.clone(),
            payload: serde_json::json!({
                "uuid": message_uuid,
                "fusionStatus": "analyzed",
                "message": {"content": [{"type": "text", "text": "project me"}]}
            }),
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::published(),
        };

        let session_handle = orchestrator.session();
        let busy_history = session_handle.lock().await;
        let project_recorder = recorder.clone();
        let project_outbox = outbox.clone();
        let projection = tokio::spawn(async move {
            project_recorder.project_published(&project_outbox).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(650)).await;
        assert!(
            !projection.is_finished(),
            "a foreground history lock must delay, not permanently discard, projection"
        );
        drop(busy_history);
        projection.await.expect("eventual projection");
        recorder.project_published(&outbox).await;

        let session = session_handle.lock().await;
        assert_eq!(
            session
                .history
                .iter()
                .filter(|message| message.id() == message_id)
                .count(),
            1,
            "late/repeated Published handling must not duplicate live history"
        );
    }
}
