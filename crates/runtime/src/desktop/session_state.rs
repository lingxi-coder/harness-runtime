//! Desktop-owned durable session coordinator.
//!
//! This module is intentionally additive to the existing composition root.
//! It owns the bounded cost queue and delegates authoritative ordering/replay
//! to [`session::jsonl::DurableJournal`].  The engine root wires one instance
//! per canonical session after it has acquired the process writer lease;
//! scoped cost views retain the same Arc authority rather than reacquiring a
//! lock.

use async_trait::async_trait;
use cost::{
    AttemptIntent, AttemptLedger, AttemptPersistAck, AttemptPersistMutation, AttemptPersistPermit,
    AttemptPersistRequest, AttemptReceipt, CostDurabilityGate, CostHydration, CostHydrator,
    CostMutationId, CostMutationRecord, CostMutationSource, CostPersistAck, CostPersistError,
    CostPersistPermit, CostPersistRequest, CostPersistResult, CostPersistence, CostState,
    CostStateVector, CostTracker,
};
pub use lingxi_core::host::{DurableFusionOutboxRecord, DurableFusionTerminalRecord};
use lingxi_core::host::{FusionPublicationReceipt, FusionPublicationStatus, FusionRunIdentity};
use lingxi_core::types::SessionId;
use session::jsonl::{DurableJournal, JournalError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch, Mutex as AsyncMutex, Notify};

const COST_QUEUE_CAPACITY: usize = 64;

mod attempts;
mod retention;
use attempts::AttemptProjection;

#[derive(Debug, Clone, Default)]
struct CoordinatorProjection {
    latest: Option<(CostState, u64)>,
    fusion_terminals: std::collections::HashMap<String, DurableFusionTerminalRecord>,
    fusion_outbox: std::collections::HashMap<String, DurableFusionOutboxRecord>,
    /// Stable acknowledgements keyed by the exact non-cost journal event id.
    /// The cost revision is the revision visible when that event was appended,
    /// not whatever cost revision happens to be current on a later retry.
    fusion_acks: std::collections::HashMap<String, CostPersistAck>,
}

#[derive(Debug, Clone)]
struct CachedCostResult {
    /// Pre-WAL failures retain the submitted record for exact retry matching.
    /// Durable successes keep only an ack and reread the one bounded WAL
    /// record on a duplicate, avoiding one cumulative vector per mutation.
    record: Option<CostMutationRecord>,
    result: CostPersistResult,
}

struct HydratedCostLedger {
    attempts: AttemptProjection,
    hydration: CostHydration,
    durable_results: std::collections::HashMap<CostMutationId, CachedCostResult>,
    fusion_terminals: std::collections::HashMap<String, DurableFusionTerminalRecord>,
    fusion_outbox: std::collections::HashMap<String, DurableFusionOutboxRecord>,
    fusion_acks: std::collections::HashMap<String, CostPersistAck>,
    /// Whether a snapshot file sits beside the WAL after this hydration.
    snapshot_present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionProjectionSnapshot {
    /// Independent cost projection; its revision is not the journal revision.
    cost: CostStateVector,
    /// Durable terminal records folded from the mixed journal.
    fusion_terminals: Vec<DurableFusionTerminalRecord>,
    /// Latest outbox status per delivery id.
    fusion_outbox: Vec<DurableFusionOutboxRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SessionEvent {
    AttemptIntent(AttemptIntent),
    AttemptReceipt(AttemptReceipt, CostStateVector),
    /// New mixed-journal cost mutation.
    Cost(CostMutationRecord),
    /// Terminal computation plus optional Slash outbox item.
    FusionTerminal(DurableFusionTerminalRecord),
    /// Append-only delivery status update.
    FusionOutbox(DurableFusionOutboxRecord),
}

fn encode_session_event(event: &SessionEvent) -> Result<serde_json::Value, CostPersistError> {
    let (tag, payload) = match event {
        SessionEvent::AttemptIntent(intent) => ("AttemptIntent", serde_json::to_value(intent)),
        SessionEvent::AttemptReceipt(receipt, state) => {
            ("AttemptReceipt", serde_json::to_value((receipt, state)))
        }
        SessionEvent::Cost(record) => ("Cost", serde_json::to_value(record)),
        SessionEvent::FusionTerminal(record) => ("FusionTerminal", serde_json::to_value(record)),
        SessionEvent::FusionOutbox(record) => ("FusionOutbox", serde_json::to_value(record)),
    };
    let mut envelope = serde_json::Map::with_capacity(1);
    envelope.insert(
        tag.into(),
        payload.map_err(|error| CostPersistError::Storage(error.to_string()))?,
    );
    Ok(serde_json::Value::Object(envelope))
}

fn encode_projection_snapshot(snapshot: &SessionProjectionSnapshot) -> serde_json::Value {
    serde_json::json!({
        "cost": snapshot.cost,
        "fusion_terminals": snapshot.fusion_terminals,
        "fusion_outbox": snapshot.fusion_outbox,
    })
}

#[cfg(test)]
fn decode_projection_snapshot(
    value: serde_json::Value,
) -> Result<SessionProjectionSnapshot, CostPersistError> {
    let serde_json::Value::Object(mut fields) = value else {
        return Err(CostPersistError::Storage(
            "session projection snapshot is not an object".into(),
        ));
    };
    let cost = fields
        .remove("cost")
        .ok_or_else(|| CostPersistError::Storage("session snapshot has no cost state".into()))?;
    let terminals = fields.remove("fusion_terminals").ok_or_else(|| {
        CostPersistError::Storage("session snapshot has no Fusion terminal projection".into())
    })?;
    let outbox = fields.remove("fusion_outbox").ok_or_else(|| {
        CostPersistError::Storage("session snapshot has no Fusion outbox projection".into())
    })?;
    if !fields.is_empty() {
        return Err(CostPersistError::Storage(
            "session snapshot contains unknown fields".into(),
        ));
    }
    Ok(SessionProjectionSnapshot {
        cost: serde_json::from_value(cost)
            .map_err(|error| CostPersistError::Storage(error.to_string()))?,
        fusion_terminals: serde_json::from_value(terminals)
            .map_err(|error| CostPersistError::Storage(error.to_string()))?,
        fusion_outbox: serde_json::from_value(outbox)
            .map_err(|error| CostPersistError::Storage(error.to_string()))?,
    })
}

enum SessionMutation {
    Attempt(AttemptPersistRequest),
    Cost(CostPersistRequest),
    FusionTerminal {
        event_id: String,
        record: DurableFusionTerminalRecord,
        ack: tokio::sync::oneshot::Sender<CostPersistResult>,
    },
    FusionOutbox {
        event_id: String,
        record: DurableFusionOutboxRecord,
        ack: tokio::sync::oneshot::Sender<CostPersistResult>,
    },
    /// FIFO fence used only after producers have been stopped/joined. It does
    /// not close the queue or retain a coordinator owner, so later live scopes
    /// and PR10 retirement remain possible.
    Barrier {
        ack: tokio::sync::oneshot::Sender<()>,
    },
}

struct QueuedMutation {
    mutation: SessionMutation,
    // Accepted work, not its possibly cancelled caller, owns this pin until
    // the writer has completed the blocking mutation and sent its ACK.
    _pin: Option<lingxi_core::host::SessionRetentionPin>,
}

fn publication_rank(status: FusionPublicationStatus) -> u8 {
    match status {
        FusionPublicationStatus::NotRequired => 0,
        FusionPublicationStatus::Pending => 1,
        FusionPublicationStatus::Queued => 2,
        FusionPublicationStatus::OutboxFailed => 3,
        FusionPublicationStatus::StorageFailure => 4,
        FusionPublicationStatus::Published => 5,
    }
}

fn outbox_event_id(record: &DurableFusionOutboxRecord) -> String {
    format!(
        "fusion-outbox:{}:{}:{}:{}",
        record.delivery_id,
        record.attempt,
        publication_rank(record.receipt.status),
        record.receipt.error.as_deref().unwrap_or_default()
    )
}

pub(crate) fn fusion_terminal_event_id(identity: &FusionRunIdentity) -> String {
    format!("fusion-terminal:{}", identity.run_id)
}

pub(crate) fn fusion_delivery_id(identity: &FusionRunIdentity) -> String {
    format!("fusion-delivery:{}", identity.run_id)
}

fn same_outbox_payload(
    left: &DurableFusionOutboxRecord,
    right: &DurableFusionOutboxRecord,
) -> bool {
    left.delivery_id == right.delivery_id
        && left.session_id == right.session_id
        && left.message_uuid == right.message_uuid
        && left.payload == right.payload
}

fn same_terminal_identity(
    left: &DurableFusionTerminalRecord,
    right: &DurableFusionTerminalRecord,
) -> bool {
    left.event_id == right.event_id
        && left.identity == right.identity
        && left.result == right.result
        && left.facts == right.facts
        && match (&left.outbox, &right.outbox) {
            (None, None) => true,
            (Some(left), Some(right)) => same_outbox_payload(left, right),
            _ => false,
        }
}

fn validate_outbox_record(
    record: &DurableFusionOutboxRecord,
    session_id: SessionId,
) -> Result<(), String> {
    if record.delivery_id.trim().is_empty() || record.message_uuid.trim().is_empty() {
        return Err("fusion outbox identity is empty".into());
    }
    if record.session_id != session_id {
        return Err("fusion outbox belongs to a different session".into());
    }
    if record.attempt > record.retry_cycle_end {
        return Err("fusion outbox attempt exceeds its retry cycle".into());
    }
    match record.receipt.status {
        FusionPublicationStatus::Queued | FusionPublicationStatus::Published => {
            if record.receipt.error.is_some() {
                return Err("successful fusion outbox status carries an error".into());
            }
        }
        FusionPublicationStatus::OutboxFailed => {
            if record.receipt.error.as_deref().is_none_or(str::is_empty) {
                return Err("failed fusion outbox status has no error".into());
            }
        }
        FusionPublicationStatus::NotRequired
        | FusionPublicationStatus::Pending
        | FusionPublicationStatus::StorageFailure => {
            return Err("fusion outbox contains a non-durable delivery status".into());
        }
    }
    Ok(())
}

fn validate_terminal_record(
    record: &DurableFusionTerminalRecord,
    session_id: SessionId,
) -> Result<(), String> {
    if record.event_id != fusion_terminal_event_id(&record.identity) {
        return Err("fusion terminal event id is not bound to its run id".into());
    }
    if record.identity.session_id != Some(session_id) {
        return Err("fusion terminal belongs to a different session".into());
    }
    if record
        .result
        .as_ref()
        .is_ok_and(|result| result.run_id != record.identity.run_id.as_str())
    {
        return Err("fusion terminal result belongs to a different run".into());
    }
    match record.outbox.as_ref() {
        Some(outbox) => {
            if record.identity.origin != lingxi_core::host::FusionOrigin::Slash {
                return Err("only a Slash terminal may contain a parent outbox".into());
            }
            if outbox.delivery_id != fusion_delivery_id(&record.identity) {
                return Err("fusion outbox delivery id is not bound to its run id".into());
            }
            validate_outbox_record(outbox, session_id)?;
            if outbox.attempt != 0
                || outbox.retry_cycle_end != 4
                || outbox.receipt != FusionPublicationReceipt::queued()
                || record.publication != FusionPublicationReceipt::queued()
            {
                return Err(
                    "fusion terminal must atomically contain its initial queued outbox".into(),
                );
            }
        }
        None => {
            if record.publication != FusionPublicationReceipt::not_required() {
                return Err(
                    "fusion terminal without an outbox has an invalid publication state".into(),
                );
            }
        }
    }
    Ok(())
}

fn validate_outbox_transition(
    previous: Option<&DurableFusionOutboxRecord>,
    record: &DurableFusionOutboxRecord,
) -> Result<(), String> {
    let Some(previous) = previous else {
        return Err("fusion outbox has no atomic terminal origin".into());
    };
    if previous.attempt > previous.retry_cycle_end || record.attempt > record.retry_cycle_end {
        return Err("fusion outbox attempt exceeds its retry cycle".into());
    }
    if !same_outbox_payload(previous, record) {
        return Err("fusion outbox payload changed for a stable delivery".into());
    }
    if previous.receipt.status == FusionPublicationStatus::Published {
        return if previous == record {
            Ok(())
        } else {
            Err("published fusion outbox is an absorbing state".into())
        };
    }
    if record.attempt < previous.attempt {
        return Err("fusion outbox generation regressed".into());
    }
    if record.attempt == previous.attempt {
        if record.retry_cycle_end != previous.retry_cycle_end {
            return Err("fusion outbox retry cycle changed within an attempt".into());
        }
        let previous_rank = publication_rank(previous.receipt.status);
        let next_rank = publication_rank(record.receipt.status);
        if next_rank < previous_rank {
            return Err("fusion outbox status regressed".into());
        }
        if next_rank == previous_rank && previous.receipt != record.receipt {
            return Err("fusion outbox status conflicts".into());
        }
        return Ok(());
    }
    let expected = previous
        .checked_next_attempt()
        .ok_or_else(|| "fusion outbox generation overflow".to_string())?;
    if record.attempt != expected {
        return Err("fusion outbox generations are not contiguous".into());
    }
    if previous.receipt.status != FusionPublicationStatus::OutboxFailed
        || record.receipt != FusionPublicationReceipt::queued()
    {
        return Err("fusion outbox advanced without a failed delivery retry".into());
    }
    if record.retry_cycle_end == previous.retry_cycle_end {
        return Ok(());
    }
    let expected_extended_end = previous
        .attempt
        .checked_add(5)
        .ok_or_else(|| "fusion outbox retry cycle overflow".to_string())?;
    if previous.attempt != previous.retry_cycle_end
        || record.retry_cycle_end != expected_extended_end
    {
        return Err("fusion outbox retry cycle changed outside its boundary".into());
    }
    Ok(())
}

/// One canonical session's durable ledger owner.
pub struct SessionStateCoordinator {
    state: Arc<CoordinatorState>,
    queue_tx: mpsc::Sender<QueuedMutation>,
    queue_rx: Arc<AsyncMutex<Option<mpsc::Receiver<QueuedMutation>>>>,
    admission_closed: Arc<AtomicBool>,
    close_tx: watch::Sender<bool>,
    worker_completion: Arc<WorkerCompletion>,
    _retention_pin: Option<lingxi_core::host::SessionRetentionPin>,
    #[cfg(test)]
    start_hydration_block: Arc<Mutex<Option<Arc<TestHydrationBlock>>>>,
}

#[cfg(test)]
struct TestHydrationBlock {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    released: Mutex<bool>,
    release: Condvar,
}

#[cfg(test)]
impl TestHydrationBlock {
    fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                entered: Mutex::new(Some(entered_tx)),
                released: Mutex::new(false),
                release: Condvar::new(),
            }),
            entered_rx,
        )
    }

    fn wait(&self) {
        if let Some(entered) = self
            .entered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = entered.send(());
        }
        let mut released = self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !*released {
            released = self
                .release
                .wait(released)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn release(&self) {
        *self
            .released
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.release.notify_all();
    }
}

#[derive(Clone)]
enum WorkerOutcome {
    Drained,
    Failed(String),
}

#[derive(Default)]
struct WorkerCompletion {
    started: AtomicBool,
    outcome: Mutex<Option<WorkerOutcome>>,
    notify: Notify,
}

impl WorkerCompletion {
    fn mark_started(&self) {
        self.started.store(true, AtomicOrdering::Release);
    }

    fn is_started(&self) -> bool {
        self.started.load(AtomicOrdering::Acquire)
    }

    fn finish(&self, outcome: WorkerOutcome) {
        let mut current = self
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_none() {
            *current = Some(outcome);
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<(), CostPersistError> {
        loop {
            let notified = self.notify.notified();
            let outcome = self
                .outcome
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            match outcome {
                Some(WorkerOutcome::Drained) => return Ok(()),
                Some(WorkerOutcome::Failed(error)) => {
                    return Err(CostPersistError::Storage(error));
                }
                None => notified.await,
            }
        }
    }
}

enum WriterTaskExit {
    Drained,
    StartupFailed {
        error: CostPersistError,
        ready: tokio::sync::oneshot::Sender<Result<(), CostPersistError>>,
    },
}

/// State retained by the writer while it drains accepted requests. It
/// intentionally owns no queue sender: when the last external/session owner
/// drops, the channel closes, the receiver drains, and the writer lease can be
/// released instead of being pinned by a self-owned sender cycle.
struct CoordinatorState {
    mutation_gate: Mutex<()>,
    session_id: SessionId,
    journal: Arc<DurableJournal>,
    projection: Mutex<CoordinatorProjection>,
    attempts: Mutex<AttemptProjection>,
    /// Stable mutation outcomes retained after ack receivers are dropped.
    results: Mutex<std::collections::HashMap<CostMutationId, CachedCostResult>>,
    writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
    durability_gate: CostDurabilityGate,
    /// Highest journal revision this coordinator has appended or hydrated.
    /// Kept here because the journal exposes no cheap accessor and a barrier
    /// must stamp the snapshot with the revision it actually covers.
    last_journal_revision: AtomicU64,
    /// Whether a snapshot file currently sits beside the WAL.  Hydration
    /// treats a snapshot with no WAL as proof that an authoritative ledger was
    /// lost, so that inference only holds while every WAL has one.
    snapshot_present: AtomicBool,
}

/// App-owned collection of hydrated session authorities. A hot clear/resume
/// first creates and fully hydrates a destination coordinator under this
/// manager, then asks the CostTracker to activate the exact coordinator entry.
/// Existing live sessions are reused by identity, preserving their lease,
/// queue, freeze gate, and background Fusion/outbox owners across A→B→A.
pub struct SessionStateManager {
    lingxi_home: PathBuf,
    entries: std::sync::RwLock<std::collections::HashMap<SessionId, Arc<SessionStateCoordinator>>>,
    creation_gate: Arc<AsyncMutex<()>>,
    closing: AtomicBool,
    legacy_shadow: Option<Arc<dyn Fn(SessionId) -> Option<u64> + Send + Sync + 'static>>,
    transcript_writer: std::sync::RwLock<Option<Arc<session::jsonl::writer::JsonlWriter>>>,
    last_used: Mutex<std::collections::HashMap<SessionId, std::time::Instant>>,
    maintenance: std::sync::OnceLock<retention::Maintenance>,
    maintenance_requested: AtomicBool,
    maintenance_running: AtomicBool,
    #[cfg(test)]
    next_start_hydration_block: Mutex<Option<Arc<TestHydrationBlock>>>,
}

impl SessionStateManager {
    /// Construct an empty process-local authority cache.
    #[must_use]
    pub fn new(lingxi_home: impl Into<PathBuf>) -> Arc<Self> {
        Self::new_with_legacy_shadow(lingxi_home, None)
    }

    /// Construct with a pure, composition-captured legacy balance matcher.
    /// The callback must not perform ambient I/O; it is evaluated exactly once
    /// for a newly opened identity and its Some/None decision is journaled
    /// before the coordinator becomes visible in `entries`.
    #[must_use]
    pub fn new_with_legacy_shadow(
        lingxi_home: impl Into<PathBuf>,
        legacy_shadow: Option<Arc<dyn Fn(SessionId) -> Option<u64> + Send + Sync + 'static>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            lingxi_home: lingxi_home.into(),
            entries: std::sync::RwLock::new(std::collections::HashMap::new()),
            creation_gate: Arc::new(AsyncMutex::new(())),
            closing: AtomicBool::new(false),
            legacy_shadow,
            transcript_writer: std::sync::RwLock::new(None),
            last_used: Mutex::new(std::collections::HashMap::new()),
            maintenance: std::sync::OnceLock::new(),
            maintenance_requested: AtomicBool::new(false),
            maintenance_running: AtomicBool::new(false),
            #[cfg(test)]
            next_start_hydration_block: Mutex::new(None),
        })
    }

    #[cfg(test)]
    fn block_next_start_hydration(&self, block: Arc<TestHydrationBlock>) {
        *self
            .next_start_hydration_block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(block);
    }

    /// Register the boot coordinator supplied by a construction-only host
    /// claim. The manager takes an Arc clone but never reacquires its lock.
    pub async fn register(
        &self,
        session_id: SessionId,
        coordinator: Arc<SessionStateCoordinator>,
    ) -> Result<(), CostPersistError> {
        let _creation = self.creation_gate.clone().lock_owned().await;
        if self.closing.load(AtomicOrdering::Acquire) {
            return Err(CostPersistError::Rejected(
                "session state manager is closing".into(),
            ));
        }
        if coordinator.state.session_id != session_id {
            return Err(CostPersistError::Rejected(
                "session coordinator identity does not match its manager key".into(),
            ));
        }
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = entries.get(&session_id) {
            if !existing.shares_authority(&coordinator) {
                return Err(CostPersistError::Rejected(
                    "session coordinator identity is already owned".into(),
                ));
            }
            return Ok(());
        }
        self.attach_retention_callback(&coordinator);
        self.touch(session_id);
        entries.insert(session_id, coordinator.view(None));
        Ok(())
    }

    /// Return the exact live authority for a session, if it has been opened in
    /// this process. This is a pure lookup used by host capability factories;
    /// it performs no I/O or claim acquisition.
    pub fn coordinator(&self, session_id: SessionId) -> Option<Arc<SessionStateCoordinator>> {
        self.coordinator_core(session_id)?.pinned_view().ok()
    }

    pub(crate) fn coordinator_core(
        &self,
        session_id: SessionId,
    ) -> Option<Arc<SessionStateCoordinator>> {
        self.entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
    }

    /// Snapshot the identities currently retained by this manager. The
    /// ordering is deterministic for shutdown diagnostics and tests.
    pub fn session_ids(&self) -> Vec<SessionId> {
        let mut ids = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect::<Vec<_>>();
        ids.sort_by_key(ToString::to_string);
        ids
    }

    /// Place a FIFO fence behind every mutation accepted before this snapshot.
    /// Composition must stop/join producers before calling this method when it
    /// needs a final shutdown drain; the queues deliberately remain open.
    pub async fn flush_all(&self) -> Result<(), CostPersistError> {
        let _creation = self.creation_gate.clone().lock_owned().await;
        let coordinators = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for coordinator in coordinators {
            coordinator.flush().await?;
        }
        Ok(())
    }

    /// Stop new session mutations, drain every already-issued queue claim,
    /// and wait for each writer task to release its worker-owned state.
    ///
    /// Successful completion removes the manager's coordinator references.
    /// Other live scopes may still retain the same coordinator and writer
    /// lease, but their closed queue can no longer accept paid work. The OS
    /// claim is released only when those final strong scopes are dropped.
    pub async fn close_and_drain(&self) -> Result<(), CostPersistError> {
        self.closing.store(true, AtomicOrdering::Release);
        let _creation = self.creation_gate.clone().lock_owned().await;
        let coordinators = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for coordinator in &coordinators {
            if let Err(error) = coordinator.close_and_drain().await {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        Ok(())
    }

    /// Attach the shared ordinary transcript writer after boot composition.
    /// Hot switches retarget its durable lock to the destination coordinator
    /// before activating that session.
    pub fn set_transcript_writer(&self, writer: Arc<session::jsonl::writer::JsonlWriter>) {
        *self
            .transcript_writer
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(writer);
    }

    async fn ensure_coordinator(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<SessionStateCoordinator>, CostPersistError> {
        if self.closing.load(AtomicOrdering::Acquire) {
            return Err(CostPersistError::Rejected(
                "session state manager is closing".into(),
            ));
        }
        let creation = self.creation_gate.clone().lock_owned().await;
        if self.closing.load(AtomicOrdering::Acquire) {
            return Err(CostPersistError::Rejected(
                "session state manager is closing".into(),
            ));
        }
        if let Some(existing) = self.coordinator_core(session_id) {
            self.touch(session_id);
            return existing.pinned_view();
        }
        let lease = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
            self.lingxi_home.join("sessions"),
        )
        .claim_session_id(&session_id.to_string(), std::process::id())
        .map_err(|error| {
            CostPersistError::Rejected(format!("session writer claim failed: {error}"))
        })?
        .into_shared();
        let coordinator = SessionStateCoordinator::open_core(&self.lingxi_home, session_id, lease)?;
        #[cfg(test)]
        if let Some(block) = self
            .next_start_hydration_block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            coordinator.block_start_hydration(block);
        }
        let legacy_shadow = self.legacy_shadow.clone();
        let (initialized_tx, initialized_rx) = tokio::sync::oneshot::channel();
        let (publish_tx, publish_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            // This owned guard prevents a cancelled mount from racing a second
            // claim for the same session while hydration/import cleanup still
            // owns the first writer lease.
            let _creation = creation;
            let initialization_coordinator = coordinator.clone();
            let initialization = tokio::spawn(async move {
                initialization_coordinator.start().await?;
                let opening_balance = legacy_shadow
                    .as_ref()
                    .and_then(|legacy_shadow| legacy_shadow(session_id));
                initialization_coordinator
                    .import_legacy_opening_balance(opening_balance)
                    .await?;
                initialization_coordinator.hydrate(session_id).await?;
                Ok::<(), CostPersistError>(())
            });
            let initialized = initialization.await.unwrap_or_else(|error| {
                Err(CostPersistError::Storage(format!(
                    "session coordinator initialization task failed: {error}"
                )))
            });
            match initialized {
                Ok(()) => {
                    if initialized_tx.send(Ok(coordinator.clone())).is_err()
                        || publish_rx.await.is_err()
                    {
                        let _ = coordinator.close_and_drain().await;
                    }
                    drop(coordinator);
                }
                Err(error) => {
                    // `close_and_drain` may repeat the startup error through
                    // its completion latch; either way it has waited for every
                    // worker/state owner before this task releases the gate.
                    let _ = coordinator.close_and_drain().await;
                    drop(coordinator);
                    let _ = initialized_tx.send(Err(error));
                }
            }
        });
        let coordinator = initialized_rx.await.map_err(|_| {
            CostPersistError::Storage("session coordinator owner task failed".into())
        })??;
        if self.closing.load(AtomicOrdering::Acquire) {
            drop(publish_tx);
            let _ = coordinator.close_and_drain().await;
            return Err(CostPersistError::Rejected(
                "session state manager closed during coordinator initialization".into(),
            ));
        }
        self.attach_retention_callback(&coordinator);
        self.touch(session_id);
        self.entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, coordinator.clone());
        let view = coordinator.pinned_view()?;
        let _ = publish_tx.send(());
        Ok(view)
    }
}

#[async_trait]
impl orchestrator::conversation::CostSessionSwitcher for SessionStateManager {
    async fn prepare_session(
        &self,
        tracker: Arc<CostTracker>,
        session_id: SessionId,
    ) -> Result<orchestrator::conversation::PreparedSessionSwitch, CostPersistError> {
        let coordinator = self.ensure_coordinator(session_id).await?;
        let core = self
            .coordinator_core(session_id)
            .ok_or_else(|| CostPersistError::Rejected("prepared coordinator disappeared".into()))?;
        let cost = tracker
            .prepare_session_hydrated_with_durable(
                session_id,
                coordinator.as_ref(),
                core.clone() as Arc<dyn CostPersistence>,
                core.writer_lease_core(),
                coordinator.durability_gate(),
            )
            .await?;
        let transcript_lock = Arc::new(session::jsonl::DurableTranscriptWriter::from_pinned(
            coordinator.journal().root().to_path_buf(),
            coordinator.journal().root_identity(),
        ));
        let prepared =
            orchestrator::conversation::PreparedSessionSwitch::new(cost, Some(transcript_lock));
        Ok(if let Some(maintenance) = self.maintenance.get() {
            let wake = maintenance.wake.clone();
            prepared.with_on_activated(move || wake())
        } else {
            prepared
        })
    }
}

impl std::fmt::Debug for SessionStateCoordinator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionStateCoordinator")
            .field("session_id", &self.state.session_id)
            .field("journal_root", &self.state.journal.root())
            .finish_non_exhaustive()
    }
}

impl SessionStateCoordinator {
    pub(crate) fn session_id(&self) -> SessionId {
        self.state.session_id
    }
    /// Open the stable `<lingxi_home>/session-state/<uuid>` directory.
    pub fn open(
        lingxi_home: impl AsRef<Path>,
        session_id: SessionId,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
    ) -> Result<Arc<Self>, CostPersistError> {
        Self::open_core(lingxi_home, session_id, writer_lease)?.pinned_view()
    }

    fn open_core(
        lingxi_home: impl AsRef<Path>,
        session_id: SessionId,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
    ) -> Result<Arc<Self>, CostPersistError> {
        if writer_lease.canonical_session_id() != Some(session_id) {
            return Err(CostPersistError::Rejected(
                "session-state writer claim does not match canonical session".into(),
            ));
        }
        let state_relative = PathBuf::from("session-state").join(session_id.as_uuid().to_string());
        let journal = DurableJournal::open_under(lingxi_home.as_ref(), &state_relative)
            .map_err(map_journal_error)?;
        let (queue_tx, queue_rx) = mpsc::channel(COST_QUEUE_CAPACITY);
        let (close_tx, _close_rx) = watch::channel(false);
        Ok(Arc::new(Self {
            state: Arc::new(CoordinatorState {
                mutation_gate: Mutex::new(()),
                session_id,
                journal: Arc::new(journal),
                projection: Mutex::new(CoordinatorProjection::default()),
                attempts: Mutex::new(AttemptProjection::new(session_id)),
                results: Mutex::new(std::collections::HashMap::new()),
                writer_lease,
                durability_gate: CostDurabilityGate::default(),
                last_journal_revision: AtomicU64::new(0),
                snapshot_present: AtomicBool::new(false),
            }),
            queue_tx,
            queue_rx: Arc::new(AsyncMutex::new(Some(queue_rx))),
            admission_closed: Arc::new(AtomicBool::new(false)),
            close_tx,
            worker_completion: Arc::new(WorkerCompletion::default()),
            _retention_pin: None,
            #[cfg(test)]
            start_hydration_block: Arc::new(Mutex::new(None)),
        }))
    }

    fn view(&self, pin: Option<lingxi_core::host::SessionRetentionPin>) -> Arc<Self> {
        Arc::new(Self {
            state: self.state.clone(),
            queue_tx: self.queue_tx.clone(),
            queue_rx: self.queue_rx.clone(),
            admission_closed: self.admission_closed.clone(),
            close_tx: self.close_tx.clone(),
            worker_completion: self.worker_completion.clone(),
            _retention_pin: pin,
            #[cfg(test)]
            start_hydration_block: self.start_hydration_block.clone(),
        })
    }

    fn retention_pin(&self) -> Result<lingxi_core::host::SessionRetentionPin, CostPersistError> {
        self.state
            .durability_gate
            .retention_gate()
            .try_pin()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))
    }

    pub(crate) fn pinned_view(&self) -> Result<Arc<Self>, CostPersistError> {
        Ok(self.view(Some(self.retention_pin()?)))
    }

    pub(crate) fn shares_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub(crate) fn writer_lease_core(
        &self,
    ) -> lingxi_core::host::live_sessions::SharedSessionWriterLease {
        self.state.writer_lease.clone()
    }

    /// Keep the writer claim alive while returning a scoped clone.
    #[must_use]
    pub fn writer_lease(&self) -> lingxi_core::host::live_sessions::SharedSessionWriterLease {
        let pin = self._retention_pin.clone().unwrap_or_else(|| {
            self.retention_pin()
                .expect("external writer lease requires a live authority")
        });
        lingxi_core::host::live_sessions::pin_writer_lease(self.writer_lease_core(), pin)
    }

    /// Per-session latch shared by ordinary and Fusion paid prechecks.
    #[must_use]
    pub fn durability_gate(&self) -> CostDurabilityGate {
        self.state.durability_gate.clone()
    }

    /// Shared durable journal handle used by terminal/outbox composition.
    #[must_use]
    pub fn journal(&self) -> Arc<DurableJournal> {
        self.state.journal.clone()
    }

    #[cfg(test)]
    fn block_start_hydration(&self, block: Arc<TestHydrationBlock>) {
        *self
            .start_hydration_block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(block);
    }

    /// Wait until every mutation accepted before this call has completed. The
    /// barrier bypasses the paid-work freeze gate because shutdown still has
    /// to drain already-owned settlements after a persistence failure.
    pub async fn flush(&self) -> Result<(), CostPersistError> {
        self.ensure_admission_open()?;
        let permit = self
            .queue_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| CostPersistError::Rejected("session state queue is closed".into()))?;
        self.ensure_admission_open()?;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::Barrier { ack: ack_tx },
            _pin: None,
        });
        ack_rx.await.map_err(|_| {
            CostPersistError::Storage("session state flush acknowledgment dropped".into())
        })
    }

    /// Stop accepting new mutations and wait until the single writer has
    /// drained the channel, including messages sent through permits issued
    /// before this close began. Signalling is synchronous, so dropping this
    /// future after its first poll cannot strand an open receiver.
    pub async fn close_and_drain(&self) -> Result<(), CostPersistError> {
        self.admission_closed.store(true, AtomicOrdering::Release);
        self.close_tx.send_replace(true);

        if !self.worker_completion.is_started() {
            let mut receiver = self.queue_rx.lock().await;
            if !self.worker_completion.is_started() {
                drop(receiver.take());
                self.worker_completion.finish(WorkerOutcome::Drained);
            }
        }
        self.worker_completion.wait().await
    }

    fn ensure_admission_open(&self) -> Result<(), CostPersistError> {
        if self.admission_closed.load(AtomicOrdering::Acquire) {
            Err(CostPersistError::Rejected(
                "session state queue is closed".into(),
            ))
        } else {
            Ok(())
        }
    }

    /// Start the single writer.  Filesystem work is moved to a blocking
    /// worker; requests remain serial even when a prior fsync is slow.
    pub async fn start(self: &Arc<Self>) -> Result<tokio::task::JoinHandle<()>, CostPersistError> {
        let mut receiver = {
            let mut receiver = self.queue_rx.lock().await;
            let receiver = receiver
                .take()
                .ok_or_else(|| CostPersistError::Rejected("coordinator already started".into()))?;
            self.worker_completion.mark_started();
            receiver
        };
        let state = self.state.clone();
        #[cfg(test)]
        let hydration_block = self
            .start_hydration_block
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mut close_rx = self.close_tx.subscribe();
        let completion = self.worker_completion.clone();
        let supervisor_completion = completion.clone();
        let supervisor_gate = state.durability_gate.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (ownership_tx, ownership_rx) = tokio::sync::oneshot::channel();
        let writer = tokio::spawn(async move {
            let hydration = tokio::task::spawn_blocking({
                let state = state.clone();
                move || {
                    #[cfg(test)]
                    if let Some(block) = hydration_block {
                        block.wait();
                    }
                    state.hydrate_blocking_with_recovery(true)
                }
            })
            .await
            .map_err(|error| CostPersistError::Storage(error.to_string()))
            .and_then(|hydration| hydration);
            let mut closing = match hydration {
                Ok(_) => ready_tx.send(Ok(())).is_err() || ownership_rx.await.is_err(),
                Err(error) => {
                    return WriterTaskExit::StartupFailed {
                        error,
                        ready: ready_tx,
                    };
                }
            };
            if closing {
                // The start waiter disappeared after the non-cancellable
                // hydration began. The owned writer, not that waiter, closes
                // and drains the receiver before releasing its state/lease.
                receiver.close();
            }
            loop {
                if !closing && *close_rx.borrow_and_update() {
                    receiver.close();
                    closing = true;
                }
                let mutation = if closing {
                    receiver.recv().await
                } else {
                    tokio::select! {
                        biased;
                        changed = close_rx.changed() => {
                            if changed.is_err() || *close_rx.borrow_and_update() {
                                receiver.close();
                                closing = true;
                            }
                            continue;
                        }
                        mutation = receiver.recv() => mutation,
                    }
                };
                let Some(mutation) = mutation else {
                    break;
                };
                let QueuedMutation { mutation, _pin } = mutation;
                match mutation {
                    SessionMutation::Attempt(request) => {
                        let worker = state.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            let result =
                                worker.persist_attempt(request.session_id, request.mutation);
                            if let Err(error) = &result {
                                freeze_then_try_recovery(&worker, error);
                            }
                            let _ = request.ack.send(result);
                        })
                        .await;
                        if let Err(error) = result {
                            freeze_then_try_recovery(
                                &state,
                                &CostPersistError::Storage(format!(
                                    "attempt persistence worker failed: {error}"
                                )),
                            );
                        }
                    }
                    SessionMutation::Cost(request) => {
                        let worker = state.clone();
                        let mutation_id = request.mutation_id.clone();
                        let record = CostMutationRecord {
                            cost_revision: request.cost_revision,
                            mutation_id: request.mutation_id.clone(),
                            source: request.source,
                            state: request.state.clone(),
                        };
                        let result =
                            tokio::task::spawn_blocking(move || worker.persist_request(request))
                                .await;
                        if let Err(join_error) = result {
                            let error = CostPersistError::Storage(format!(
                                "session state persistence worker failed: {join_error}"
                            ));
                            freeze_then_try_recovery(&state, &error);
                            state
                                .results
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .entry(mutation_id)
                                .or_insert(CachedCostResult {
                                    record: Some(record),
                                    result: Err(error),
                                });
                            tracing::error!(
                                "session state persistence worker failed: {join_error}"
                            );
                        }
                    }
                    SessionMutation::FusionTerminal {
                        event_id,
                        record,
                        ack,
                    } => {
                        let worker = state.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            worker.persist_fusion_terminal(&event_id, &record)
                        })
                        .await
                        .map_err(|error| {
                            CostPersistError::Storage(format!(
                                "session terminal persistence worker failed: {error}"
                            ))
                        })
                        .and_then(|result| result);
                        if let Err(error) = &result {
                            state.durability_gate.freeze(error.to_string());
                        }
                        let _ = ack.send(result);
                    }
                    SessionMutation::FusionOutbox {
                        event_id,
                        record,
                        ack,
                    } => {
                        let worker = state.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            worker.persist_fusion_outbox(&event_id, &record)
                        })
                        .await
                        .map_err(|error| {
                            CostPersistError::Storage(format!(
                                "session outbox persistence worker failed: {error}"
                            ))
                        })
                        .and_then(|result| result);
                        if let Err(error) = &result {
                            state.durability_gate.freeze(error.to_string());
                        }
                        let _ = ack.send(result);
                    }
                    SessionMutation::Barrier { ack } => {
                        // Reaching this arm proves every earlier accepted FIFO
                        // mutation finished its blocking append and projection
                        // update. A dropped waiter does not affect the drain.
                        // It is also the one point where refreshing the whole
                        // derivative snapshot is worth its fsync, so a clean
                        // shutdown leaves a current one behind.
                        let worker = state.clone();
                        let _ = tokio::task::spawn_blocking(move || {
                            let revision =
                                worker.last_journal_revision.load(AtomicOrdering::Acquire);
                            worker.write_projection_snapshot(revision);
                        })
                        .await;
                        let _ = ack.send(());
                    }
                }
            }
            // The supervisor publishes completion only after this future has
            // returned and therefore dropped its receiver and final state Arc.
            drop(receiver);
            drop(state);
            WriterTaskExit::Drained
        });
        tokio::spawn(async move {
            match writer.await {
                Ok(WriterTaskExit::Drained) => {
                    supervisor_completion.finish(WorkerOutcome::Drained);
                }
                Ok(WriterTaskExit::StartupFailed { error, ready }) => {
                    let detail = error.to_string();
                    supervisor_gate.freeze(detail.clone());
                    supervisor_completion.finish(WorkerOutcome::Failed(detail));
                    let _ = ready.send(Err(error));
                }
                Err(error) => {
                    let detail = format!("session state writer task failed: {error}");
                    supervisor_gate.freeze(detail.clone());
                    supervisor_completion.finish(WorkerOutcome::Failed(detail));
                }
            }
        });
        match ready_rx.await {
            Ok(Ok(())) => {
                // There is no await between this ownership handoff and the
                // Ready return from `start`. If the waiter was cancelled
                // earlier, dropping `ownership_tx` makes the writer drain.
                let _ = ownership_tx.send(());
                let facade = completion.clone();
                Ok(tokio::spawn(async move {
                    if let Err(error) = facade.wait().await {
                        panic!("session state writer failed: {error}");
                    }
                }))
            }
            Ok(Err(error)) => {
                let _ = completion.wait().await;
                Err(error)
            }
            Err(_) => match completion.wait().await {
                Err(error) => Err(error),
                Ok(()) => Err(CostPersistError::Storage(
                    "session state writer dropped its startup acknowledgment".into(),
                )),
            },
        }
    }

    /// Hydrate the complete folded cost state before active-session publish.
    /// Recovery never invokes an executor or provider; it only folds durable
    /// vectors already present in the journal.
    pub fn hydrate_blocking(&self) -> Result<CostHydration, CostPersistError> {
        self.state.hydrate_blocking()
    }

    /// Return the current freeze-safe projection for composition roots that
    /// need to seed a tracker synchronously after boot.
    pub fn projection(&self) -> Option<(CostState, u64)> {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
            .clone()
    }

    /// Return the latest durable terminal record for a Fusion run.
    pub fn fusion_terminal(&self, event_id: &str) -> Option<DurableFusionTerminalRecord> {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fusion_terminals
            .get(event_id)
            .cloned()
    }

    /// Return the latest durable parent-session delivery record.
    pub fn fusion_outbox(&self, delivery_id: &str) -> Option<DurableFusionOutboxRecord> {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fusion_outbox
            .get(delivery_id)
            .cloned()
    }

    /// Snapshot pending parent deliveries for startup/manual retry. The
    /// returned records are immutable payload snapshots; retry workers append
    /// a new monotonic status event rather than mutating this map in place.
    pub fn fusion_outboxes(&self) -> Vec<DurableFusionOutboxRecord> {
        self.state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fusion_outbox
            .values()
            .cloned()
            .collect()
    }

    /// Persist a terminal computation through the same bounded queue as cost
    /// mutations. The receipt is only returned after the mixed journal append
    /// is fsynced; callers retain the failed result and can retry by event id.
    pub async fn append_fusion_terminal(
        &self,
        record: DurableFusionTerminalRecord,
    ) -> Result<CostPersistAck, CostPersistError> {
        let event_id = record.event_id.clone();
        let (permit, pin) = self.acquire_session_mutation_permit().await?;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::FusionTerminal {
                event_id,
                record,
                ack: ack_tx,
            },
            _pin: Some(pin),
        });
        ack_rx.await.map_err(|_| {
            CostPersistError::Storage("fusion terminal acknowledgment dropped".into())
        })?
    }

    /// Persist one append-only outbox status update through the mixed journal.
    pub async fn append_fusion_outbox(
        &self,
        record: DurableFusionOutboxRecord,
    ) -> Result<CostPersistAck, CostPersistError> {
        let event_id = outbox_event_id(&record);
        let (permit, pin) = self.acquire_session_mutation_permit().await?;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit.send(QueuedMutation {
            mutation: SessionMutation::FusionOutbox {
                event_id,
                record,
                ack: ack_tx,
            },
            _pin: Some(pin),
        });
        ack_rx
            .await
            .map_err(|_| CostPersistError::Storage("fusion outbox acknowledgment dropped".into()))?
    }

    async fn acquire_session_mutation_permit(
        &self,
    ) -> Result<
        (
            mpsc::OwnedPermit<QueuedMutation>,
            lingxi_core::host::SessionRetentionPin,
        ),
        CostPersistError,
    > {
        let pin = self.retention_pin()?;
        self.ensure_admission_open()?;
        if let Some(reason) = self.state.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let permit = self
            .queue_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| CostPersistError::Rejected("session state queue is closed".into()))?;
        self.ensure_admission_open()?;
        Ok((permit, pin))
    }

    /// Fold the legacy `lastCost` import decision and opening balance into one
    /// durable mutation.  `None` is meaningful: it records that matching was
    /// evaluated and found no opening balance, preventing a later shadow edit
    /// from importing the same value.
    pub async fn import_legacy_opening_balance(
        &self,
        opening_nano_usd: Option<u64>,
    ) -> Result<CostPersistAck, CostPersistError> {
        let already_evaluated = self
            .state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
            .as_ref()
            .is_some_and(|(state, _)| state.legacy_import_evaluated);
        if already_evaluated {
            let state = self.state.clone();
            return tokio::task::spawn_blocking(move || state.legacy_import_ack())
                .await
                .map_err(|error| CostPersistError::Storage(error.to_string()))??
                .ok_or_else(|| {
                    CostPersistError::Storage(
                        "legacy import marker is set but its durable event is missing".into(),
                    )
                });
        }
        let permit = <Self as CostPersistence>::acquire_permit(self, self.state.session_id).await?;
        let ack_rx = {
            let projection = self
                .state
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (current, journal_revision) = projection.latest.as_ref().ok_or_else(|| {
                CostPersistError::Rejected(
                    "legacy import requires a started, hydrated coordinator".into(),
                )
            })?;
            if current.legacy_import_evaluated {
                return Err(CostPersistError::Rejected(
                    "legacy opening balance was already evaluated".into(),
                ));
            }
            let mut staged = current.clone();
            staged.legacy_import_evaluated = true;
            staged.cost_revision = staged
                .cost_revision
                .checked_add(1)
                .ok_or_else(|| CostPersistError::Storage("cost revision overflow".into()))?;
            // A pre-existing V1 WAL is already authoritative, including old
            // V1 prefixes written before the evaluated marker existed. Never
            // merge a later mutable legacy shadow into that ledger.
            let imported_opening = if *journal_revision == 0 {
                if let Some(opening) = opening_nano_usd {
                    staged.total_nano_usd = staged.total_nano_usd.saturating_add(opening);
                    staged.legacy_opening_balance_nano_usd = staged
                        .legacy_opening_balance_nano_usd
                        .saturating_add(opening);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            let mutation_id = CostMutationId::new(format!(
                "{}:v1:{}",
                if imported_opening {
                    "legacy-opening-balance"
                } else {
                    "legacy-import-evaluated"
                },
                self.state.session_id
            ));
            let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
            permit.enqueue(CostPersistRequest {
                session_id: self.state.session_id,
                cost_revision: staged.cost_revision,
                mutation_id,
                state: CostStateVector::from(&staged),
                source: if imported_opening {
                    CostMutationSource::LegacyOpeningBalance
                } else {
                    CostMutationSource::LegacyImportEvaluated
                },
                ack: ack_tx,
            })?;
            ack_rx
        };
        ack_rx.await.map_err(|_| {
            CostPersistError::Storage("legacy opening balance acknowledgment dropped".into())
        })?
    }
}

impl CoordinatorState {
    fn persist_request(&self, request: CostPersistRequest) {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mutation_id = request.mutation_id.clone();
        let record = CostMutationRecord {
            cost_revision: request.cost_revision,
            mutation_id: request.mutation_id.clone(),
            source: request.source,
            state: request.state.clone(),
        };
        let cached = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&mutation_id)
            .cloned();
        let outcome = if let Some(cached) = cached {
            self.resolve_cached_result(&cached, &record)
        } else {
            let outcome = self.persist_request_inner(&request, &record);
            let retained_record = outcome.is_err().then_some(record);
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    mutation_id.clone(),
                    CachedCostResult {
                        record: retained_record,
                        result: outcome.clone(),
                    },
                );
            outcome
        };
        if let Err(error) = &outcome {
            self.durability_gate.freeze(error.to_string());
            tracing::error!("durable cost mutation {mutation_id:?} failed: {error}");
        }
        // The ack channel is intentionally best-effort.  The journal has
        // already retained the stable mutation id and replay can recover the
        // result even when the original caller was cancelled.
        let _ = request.ack.send(outcome);
    }

    fn resolve_cached_result(
        &self,
        cached: &CachedCostResult,
        submitted: &CostMutationRecord,
    ) -> CostPersistResult {
        if let Some(previous) = cached.record.as_ref() {
            return if previous == submitted {
                cached.result.clone()
            } else {
                Err(mutation_conflict(&submitted.mutation_id))
            };
        }
        let Some(replayed) = self.replayed_result(submitted)? else {
            return Err(CostPersistError::Storage(format!(
                "durable cost mutation disappeared: {}",
                submitted.mutation_id.as_str()
            )));
        };
        if cached.result == Ok(replayed.clone()) {
            Ok(replayed)
        } else {
            Err(CostPersistError::Storage(format!(
                "cached cost acknowledgment disagrees with WAL: {}",
                submitted.mutation_id.as_str()
            )))
        }
    }

    fn replayed_result(
        &self,
        submitted: &CostMutationRecord,
    ) -> Result<Option<CostPersistAck>, CostPersistError> {
        let Some(entry) = self
            .journal
            .find_event_durable(submitted.mutation_id.as_str())
            .map_err(map_journal_error)?
        else {
            return Ok(None);
        };
        let persisted = match decode_session_event(entry.event.clone()) {
            Ok(SessionEvent::Cost(record)) => record,
            Ok(_) => {
                return Err(CostPersistError::Storage(
                    "cost mutation id refers to a non-cost session event".into(),
                ));
            }
            Err(error) => return Err(CostPersistError::Storage(error.to_string())),
        };
        if persisted.mutation_id.as_str() != entry.event_id || persisted != *submitted {
            return Err(mutation_conflict(&submitted.mutation_id));
        }
        Ok(Some(CostPersistAck {
            mutation_id: submitted.mutation_id.clone(),
            journal_revision: entry.journal_revision,
            cost_revision: submitted.cost_revision,
        }))
    }

    fn persist_request_inner(
        &self,
        request: &CostPersistRequest,
        record: &CostMutationRecord,
    ) -> CostPersistResult {
        if request.session_id != self.session_id {
            return Err(CostPersistError::Rejected(
                "cost mutation belongs to a different session".into(),
            ));
        }
        let validated_state = match request.state.clone().try_into_state() {
            Ok(state) => state,
            Err(error) => return Err(error),
        };
        if request.state.session_id != request.session_id
            || validated_state.session_id != self.session_id
        {
            return Err(CostPersistError::Rejected(
                "cost request/vector/coordinator session identity mismatch".into(),
            ));
        }
        if request.cost_revision != request.state.cost_revision
            || record.cost_revision != request.cost_revision
        {
            return Err(CostPersistError::Storage(
                "cost request/vector revision mismatch".into(),
            ));
        }
        let previous_revision = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
            .as_ref()
            .map_or(0, |(previous, _)| previous.cost_revision);
        let Some(expected_revision) = previous_revision.checked_add(1) else {
            return Err(CostPersistError::Storage("cost revision overflow".into()));
        };
        if request.cost_revision != expected_revision {
            if let Some(ack) = self.replayed_result(record)? {
                return Ok(ack);
            }
            return Err(CostPersistError::Storage(
                "cost mutation revision is not the next durable revision".into(),
            ));
        }
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let event = match encode_session_event(&SessionEvent::Cost(record.clone())) {
            Ok(event) => event,
            Err(error) => return Err(error),
        };
        let append = self
            .journal
            .append_once(request.mutation_id.as_str(), &event)
            .map_err(map_journal_error);
        match append {
            Ok(append) => {
                {
                    let mut projection = self
                        .projection
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    projection.latest = Some((validated_state.clone(), append.journal_revision));
                }
                self.note_durable_append(append.journal_revision);
                Ok(CostPersistAck {
                    mutation_id: request.mutation_id.clone(),
                    journal_revision: append.journal_revision,
                    cost_revision: request.cost_revision,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn persist_fusion_terminal(
        &self,
        event_id: &str,
        record: &DurableFusionTerminalRecord,
    ) -> CostPersistResult {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if event_id != record.event_id {
            return Err(CostPersistError::Rejected(
                "fusion terminal event id is not stable".into(),
            ));
        }
        validate_terminal_record(record, self.session_id).map_err(CostPersistError::Rejected)?;
        let (previous, stable_ack) = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                projection.fusion_terminals.get(event_id).cloned(),
                projection.fusion_acks.get(event_id).cloned(),
            )
        };
        if let Some(previous) = previous {
            if !same_terminal_identity(&previous, record) {
                return Err(mutation_conflict(&CostMutationId::new(event_id)));
            }
            let entry = self
                .journal
                .find_event_durable(event_id)
                .map_err(map_journal_error)?
                .ok_or_else(|| {
                    CostPersistError::Storage(
                        "durable Fusion terminal projection has no WAL event".into(),
                    )
                })?;
            let persisted = match decode_session_event(entry.event) {
                Ok(SessionEvent::FusionTerminal(persisted)) => persisted,
                _ => {
                    return Err(CostPersistError::Storage(
                        "fusion terminal event id refers to another event type".into(),
                    ));
                }
            };
            if !same_terminal_identity(&persisted, record) {
                return Err(mutation_conflict(&CostMutationId::new(event_id)));
            }
            let ack = stable_ack.ok_or_else(|| {
                CostPersistError::Storage(
                    "durable Fusion terminal projection has no stable acknowledgment".into(),
                )
            })?;
            if ack.journal_revision != entry.journal_revision {
                return Err(CostPersistError::Storage(
                    "fusion terminal acknowledgment disagrees with the WAL".into(),
                ));
            }
            return Ok(ack);
        }
        let cost_revision = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(outbox) = record.outbox.as_ref() {
                if projection.fusion_outbox.contains_key(&outbox.delivery_id) {
                    return Err(CostPersistError::Rejected(
                        "fusion terminal outbox already has a status history".into(),
                    ));
                }
            }
            projection
                .latest
                .as_ref()
                .map_or(0, |(state, _)| state.cost_revision)
        };
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let event = encode_session_event(&SessionEvent::FusionTerminal(record.clone()))?;
        let append = self
            .journal
            .append_once(event_id, &event)
            .map_err(map_journal_error)?;
        if append.duplicate {
            return Err(CostPersistError::Storage(
                "durable Fusion terminal is missing from the hydrated projection".into(),
            ));
        }
        let ack = CostPersistAck {
            mutation_id: CostMutationId::new(event_id),
            journal_revision: append.journal_revision,
            cost_revision,
        };
        {
            let mut projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            projection
                .fusion_terminals
                .insert(event_id.to_string(), record.clone());
            if let Some(outbox) = record.outbox.as_ref() {
                projection
                    .fusion_outbox
                    .insert(outbox.delivery_id.clone(), outbox.clone());
            }
            projection
                .fusion_acks
                .insert(event_id.to_string(), ack.clone());
        }
        self.note_durable_append(append.journal_revision);
        Ok(ack)
    }

    fn persist_fusion_outbox(
        &self,
        event_id: &str,
        record: &DurableFusionOutboxRecord,
    ) -> CostPersistResult {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if event_id != outbox_event_id(record) {
            return Err(CostPersistError::Rejected(
                "fusion outbox event id is not stable".into(),
            ));
        }
        validate_outbox_record(record, self.session_id).map_err(CostPersistError::Rejected)?;
        let stable_ack = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .fusion_acks
            .get(event_id)
            .cloned();
        if let Some(ack) = stable_ack {
            let entry = self
                .journal
                .find_event_durable(event_id)
                .map_err(map_journal_error)?
                .ok_or_else(|| {
                    CostPersistError::Storage(
                        "durable Fusion outbox acknowledgment has no WAL event".into(),
                    )
                })?;
            let persisted = match decode_session_event(entry.event) {
                Ok(SessionEvent::FusionOutbox(persisted)) => persisted,
                _ => {
                    return Err(CostPersistError::Storage(
                        "fusion outbox event id refers to another event type".into(),
                    ));
                }
            };
            if persisted != *record {
                return Err(mutation_conflict(&CostMutationId::new(event_id)));
            }
            if ack.journal_revision != entry.journal_revision {
                return Err(CostPersistError::Storage(
                    "fusion outbox acknowledgment disagrees with the WAL".into(),
                ));
            }
            return Ok(ack);
        }
        let cost_revision = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            validate_outbox_transition(projection.fusion_outbox.get(&record.delivery_id), record)
                .map_err(CostPersistError::Rejected)?;
            projection
                .latest
                .as_ref()
                .map_or(0, |(state, _)| state.cost_revision)
        };
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let event = encode_session_event(&SessionEvent::FusionOutbox(record.clone()))?;
        let append = self
            .journal
            .append_once(event_id, &event)
            .map_err(map_journal_error)?;
        if append.duplicate {
            return Err(CostPersistError::Storage(
                "durable Fusion outbox event is missing from the hydrated projection".into(),
            ));
        }
        let ack = CostPersistAck {
            mutation_id: CostMutationId::new(event_id),
            journal_revision: append.journal_revision,
            cost_revision,
        };
        {
            let mut projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            projection
                .fusion_outbox
                .insert(record.delivery_id.clone(), record.clone());
            projection
                .fusion_acks
                .insert(event_id.to_string(), ack.clone());
        }
        self.note_durable_append(append.journal_revision);
        Ok(ack)
    }

    fn snapshot_projection(&self) -> SessionProjectionSnapshot {
        let projection = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let cost = projection.latest.as_ref().map_or_else(
            || CostState {
                session_id: self.session_id,
                ..Default::default()
            },
            |(state, _)| state.clone(),
        );
        SessionProjectionSnapshot {
            cost: CostStateVector::from(&cost),
            fusion_terminals: projection.fusion_terminals.values().cloned().collect(),
            fusion_outbox: projection.fusion_outbox.values().cloned().collect(),
        }
    }

    /// Rebuild the whole derivative snapshot.  This serializes every terminal
    /// record, each carrying a run's full answer text, and fsyncs, so it
    /// belongs at hydration and at a flush barrier -- not on the append path.
    fn write_projection_snapshot(&self, journal_revision: u64) {
        let snapshot = encode_projection_snapshot(&self.snapshot_projection());
        match self.journal.write_snapshot(journal_revision, &snapshot) {
            Ok(()) => self.snapshot_present.store(true, AtomicOrdering::Release),
            Err(error) => {
                tracing::warn!("session projection snapshot rebuild deferred: {error}");
            }
        }
    }

    /// Record an accepted append and keep the snapshot's *existence* current
    /// without keeping its *contents* current.
    ///
    /// The snapshot has exactly one production reader, and that reader only
    /// asks whether the file is there: a snapshot beside a missing WAL means
    /// an authoritative ledger was lost.  Nothing reads what is inside it.
    /// Rewriting the full projection per append therefore bought that reader
    /// nothing while costing one fsync and one serialization of the entire
    /// session history for every model response.  Snapshot failure stays
    /// derivative either way: the WAL append already crossed the durable
    /// acknowledgement boundary.
    fn note_durable_append(&self, journal_revision: u64) {
        self.last_journal_revision
            .store(journal_revision, AtomicOrdering::Release);
        if !self.snapshot_present.load(AtomicOrdering::Acquire) {
            self.write_projection_snapshot(journal_revision);
        }
    }

    fn legacy_import_ack(&self) -> Result<Option<CostPersistAck>, CostPersistError> {
        let replay = self.journal.replay().map_err(map_journal_error)?;
        for entry in replay.entries {
            let event = decode_session_event(entry.event.clone())
                .map_err(|error| CostPersistError::Storage(error.to_string()))?;
            let record = match event {
                SessionEvent::Cost(record) => record,
                _ => continue,
            };
            if !matches!(
                record.source,
                CostMutationSource::LegacyOpeningBalance
                    | CostMutationSource::LegacyImportEvaluated
            ) {
                continue;
            }
            return Ok(Some(CostPersistAck {
                mutation_id: record.mutation_id,
                journal_revision: entry.journal_revision,
                cost_revision: record.cost_revision,
            }));
        }
        Ok(None)
    }

    fn hydrate_blocking(&self) -> Result<CostHydration, CostPersistError> {
        self.hydrate_blocking_with_recovery(false)
    }

    fn hydrate_blocking_with_recovery(
        &self,
        recover: bool,
    ) -> Result<CostHydration, CostPersistError> {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let hydrated = match hydrate_from_journal(&self.journal, self.session_id) {
            Ok(hydrated) => hydrated,
            Err(error) if rebuildable_ledger_damage(&error) => {
                // The transcript is a separate file and is untouched, so the
                // conversation resumes. What is lost is this session's
                // recorded spend and any queued Fusion delivery, which is why
                // the damaged bytes are preserved rather than deleted.
                let report = self
                    .journal
                    .quarantine(&error.to_string())
                    .map_err(|failure| {
                        CostPersistError::Storage(format!(
                            "cost ledger is damaged ({error}) and could not be set aside: {failure}"
                        ))
                    })?;
                tracing::warn!(
                    reason = %report.reason,
                    moved = ?report.moved,
                    "cost ledger was damaged; the session resumes with a fresh one"
                );
                hydrate_from_journal(&self.journal, self.session_id)?
            }
            Err(error) => return Err(error),
        };
        self.last_journal_revision
            .store(hydrated.hydration.journal_revision, AtomicOrdering::Release);
        self.snapshot_present
            .store(hydrated.snapshot_present, AtomicOrdering::Release);
        *self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = hydrated.attempts;
        let mut results = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (mutation_id, durable) in hydrated.durable_results {
            if let Some(existing) = results.get(&mutation_id) {
                if existing.record.is_some() || existing.result != durable.result {
                    return Err(CostPersistError::Storage(format!(
                        "cost mutation id conflict while hydrating: {}",
                        mutation_id.as_str()
                    )));
                }
            } else {
                results.insert(mutation_id, durable);
            }
        }
        {
            let mut projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            projection.latest = Some((
                hydrated.hydration.state.clone(),
                hydrated.hydration.journal_revision,
            ));
            projection.fusion_terminals = hydrated.fusion_terminals;
            projection.fusion_outbox = hydrated.fusion_outbox;
            projection.fusion_acks = hydrated.fusion_acks;
        }
        drop(results);
        // Only startup recovery can manufacture Unknown, never a provider
        // response. Persist every unresolved intent before this session is live.
        if recover {
            self.recover_attempts()?;
        }
        let attempt_outputs = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .output_recovery();
        let projection = self
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, journal_revision) = projection
            .latest
            .clone()
            .expect("hydration installed state");
        Ok(CostHydration {
            state,
            journal_revision,
            attempt_outputs,
        })
    }
}

/// Freeze on a persistence failure, then try to prove the ledger survived it.
///
/// Most write failures leave nothing behind: the journal advances its revision
/// only after the write, its fsync and its fingerprint check all pass, so a
/// failed append simply did not happen. Two other shapes are not survivable
/// and this must not clear either:
///
/// * the file is damaged -- a replay says so;
/// * the record landed but the in-memory projection never folded it, which is
///   what a failed acknowledgement after a complete write leaves behind. The
///   file replays perfectly there, so a replay alone would wrongly recover and
///   let later work build on a projection that is behind the disk.
///
/// So the proof is both: the ledger replays, AND the last revision on disk is
/// the one this coordinator has already accounted for. It runs off the
/// caller's path, in the worker that just failed.
fn freeze_then_try_recovery(state: &CoordinatorState, error: &CostPersistError) {
    state.durability_gate.freeze(error.to_string());
    let recovered = state.durability_gate.thaw_if_intact(|| {
        let Ok(replay) = state.journal.replay() else {
            return false;
        };
        let on_disk = replay
            .entries
            .last()
            .map_or(0, |entry| entry.journal_revision);
        let accounted = state
            .projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest
            .as_ref()
            .map_or(0, |(_, revision)| *revision);
        on_disk == accounted
    });
    if recovered {
        tracing::warn!(
            %error,
            "session ledger write failed and left nothing behind; the session continues"
        );
    } else {
        tracing::error!(
            %error,
            "session ledger write failed; the ledger and its projection disagree"
        );
    }
}

fn mutation_conflict(mutation_id: &CostMutationId) -> CostPersistError {
    CostPersistError::Storage(format!(
        "cost mutation id conflict: {}",
        mutation_id.as_str()
    ))
}

/// Whether a hydration failure describes damage this host can rebuild from.
///
/// Rebuildable means "the ledger's own bytes are unusable": the reader stays
/// strict and the damaged file is set aside. Everything else is deliberately
/// fatal -- an IO error means renaming would likely fail too and would
/// destroy evidence on a sick volume; a lock failure means another live
/// process owns this session; a frozen gate means this process already knows
/// its own writes are failing.
fn rebuildable_ledger_damage(error: &CostPersistError) -> bool {
    let CostPersistError::Storage(message) = error else {
        return false;
    };
    [
        "journal corruption",
        "does not follow",
        "journal event id conflict",
        "unsupported journal schema version",
        "journal record exceeds",
        "without its authoritative WAL",
        "cost vector",
        "cost mutation id conflict",
    ]
    .iter()
    .any(|marker| message.contains(marker))
}

fn hydrate_from_journal(
    journal: &DurableJournal,
    session_id: SessionId,
) -> Result<HydratedCostLedger, CostPersistError> {
    let mut latest = CostState {
        session_id,
        ..Default::default()
    };
    let mut last_cost_revision = 0_u64;
    let mut durable_results = std::collections::HashMap::new();
    let mut fusion_terminals = std::collections::HashMap::new();
    let mut fusion_outbox = std::collections::HashMap::new();
    let mut fusion_acks = std::collections::HashMap::new();
    let mut attempts = AttemptProjection::new(session_id);
    let mut fold_error = None;
    let replay = journal
        .replay_durable_with(|entry| {
            // Keep validating the physical WAL after a cost semantic error so
            // replay never publishes a prefix merely because its fold failed.
            if fold_error.is_none() {
                if let Err(error) = fold_session_entry(
                    entry,
                    session_id,
                    &mut last_cost_revision,
                    &mut latest,
                    &mut durable_results,
                    &mut fusion_terminals,
                    &mut fusion_outbox,
                    &mut fusion_acks,
                    &mut attempts,
                ) {
                    fold_error = Some(error);
                }
            }
        })
        .map_err(map_journal_error)?;
    if let Some(error) = fold_error {
        return Err(error);
    }
    if !replay.journal_present {
        return match journal.read_snapshot::<serde_json::Value>() {
            Ok(None) => Ok(HydratedCostLedger {
                attempts,
                hydration: CostHydration {
                    state: CostState {
                        session_id,
                        ..Default::default()
                    },
                    journal_revision: 0,
                    attempt_outputs: Vec::new(),
                },
                durable_results: std::collections::HashMap::new(),
                fusion_terminals,
                fusion_outbox,
                fusion_acks,
                snapshot_present: false,
            }),
            Ok(Some(_)) | Err(_) => Err(CostPersistError::Storage(
                "cost snapshot exists without its authoritative WAL".into(),
            )),
        };
    }
    // A valid WAL is authoritative. A missing, corrupt, behind, ahead, or
    // prefix-mismatched derivative snapshot is rebuilt without weakening the
    // successful replay result.
    let snapshot = SessionProjectionSnapshot {
        cost: CostStateVector::from(&latest),
        fusion_terminals: fusion_terminals.values().cloned().collect(),
        fusion_outbox: fusion_outbox.values().cloned().collect(),
    };
    let snapshot = encode_projection_snapshot(&snapshot);
    let snapshot_present = match journal.write_snapshot(replay.last_revision, &snapshot) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!("cost snapshot rebuild deferred: {error}");
            false
        }
    };
    Ok(HydratedCostLedger {
        attempts,
        hydration: CostHydration {
            state: latest,
            journal_revision: replay.last_revision,
            attempt_outputs: Vec::new(),
        },
        durable_results,
        fusion_terminals,
        fusion_outbox,
        fusion_acks,
        snapshot_present,
    })
}

fn decode_session_event(value: serde_json::Value) -> Result<SessionEvent, CostPersistError> {
    let serde_json::Value::Object(envelope) = value else {
        return Err(CostPersistError::Storage(
            "session journal event is not a tagged object".into(),
        ));
    };
    if envelope.len() != 1 {
        return Err(CostPersistError::Storage(
            "session journal event must contain exactly one tag".into(),
        ));
    }
    let (tag, payload) = envelope.into_iter().next().expect("one tag was validated");
    match tag.as_str() {
        "AttemptIntent" => serde_json::from_value(payload)
            .map(SessionEvent::AttemptIntent)
            .map_err(|error| CostPersistError::Storage(error.to_string())),
        "AttemptReceipt" => serde_json::from_value::<(AttemptReceipt, CostStateVector)>(payload)
            .map(|(receipt, state)| SessionEvent::AttemptReceipt(receipt, state))
            .map_err(|error| CostPersistError::Storage(error.to_string())),
        "Cost" => serde_json::from_value(payload)
            .map(SessionEvent::Cost)
            .map_err(|error| CostPersistError::Storage(error.to_string())),
        "FusionTerminal" => serde_json::from_value(payload)
            .map(SessionEvent::FusionTerminal)
            .map_err(|error| CostPersistError::Storage(error.to_string())),
        "FusionOutbox" => serde_json::from_value(payload)
            .map(SessionEvent::FusionOutbox)
            .map_err(|error| CostPersistError::Storage(error.to_string())),
        _ => Err(CostPersistError::Storage(format!(
            "unknown session journal event tag `{tag}`"
        ))),
    }
}

fn fold_session_entry(
    entry: session::jsonl::JournalEntry,
    session_id: SessionId,
    last_cost_revision: &mut u64,
    latest: &mut CostState,
    durable_results: &mut std::collections::HashMap<CostMutationId, CachedCostResult>,
    fusion_terminals: &mut std::collections::HashMap<String, DurableFusionTerminalRecord>,
    fusion_outbox: &mut std::collections::HashMap<String, DurableFusionOutboxRecord>,
    fusion_acks: &mut std::collections::HashMap<String, CostPersistAck>,
    attempts: &mut AttemptProjection,
) -> Result<(), CostPersistError> {
    let event = decode_session_event(entry.event.clone())
        .map_err(|error| CostPersistError::Storage(error.to_string()))?;
    match event {
        event @ (SessionEvent::AttemptIntent(_) | SessionEvent::AttemptReceipt(_, _)) => {
            attempts.replay(event, &entry.event_id, entry.journal_revision, latest)?;
            *last_cost_revision = latest.cost_revision;
            Ok(())
        }
        SessionEvent::Cost(event) => fold_cost_entry(
            entry,
            session_id,
            last_cost_revision,
            latest,
            durable_results,
            event,
        ),
        SessionEvent::FusionTerminal(record) => {
            if record.event_id != entry.event_id {
                return Err(CostPersistError::Storage(
                    "fusion terminal event id disagrees with journal id".into(),
                ));
            }
            validate_terminal_record(&record, session_id).map_err(CostPersistError::Storage)?;
            if let Some(previous) = fusion_terminals.get(&record.event_id) {
                if previous != &record {
                    return Err(mutation_conflict(&CostMutationId::new(&record.event_id)));
                }
            } else {
                if let Some(outbox) = record.outbox.as_ref() {
                    if fusion_outbox
                        .insert(outbox.delivery_id.clone(), outbox.clone())
                        .is_some()
                    {
                        return Err(CostPersistError::Storage(
                            "fusion terminal outbox already has a status history".into(),
                        ));
                    }
                }
                fusion_terminals.insert(record.event_id.clone(), record.clone());
            }
            insert_fusion_ack(
                fusion_acks,
                CostPersistAck {
                    mutation_id: CostMutationId::new(&record.event_id),
                    journal_revision: entry.journal_revision,
                    cost_revision: *last_cost_revision,
                },
            )?;
            Ok(())
        }
        SessionEvent::FusionOutbox(record) => {
            let expected = outbox_event_id(&record);
            if entry.event_id != expected {
                return Err(CostPersistError::Storage(
                    "fusion outbox event identity is invalid".into(),
                ));
            }
            validate_outbox_record(&record, session_id).map_err(CostPersistError::Storage)?;
            fold_outbox_record(&record, fusion_outbox)?;
            insert_fusion_ack(
                fusion_acks,
                CostPersistAck {
                    mutation_id: CostMutationId::new(&entry.event_id),
                    journal_revision: entry.journal_revision,
                    cost_revision: *last_cost_revision,
                },
            )
        }
    }
}

fn insert_fusion_ack(
    fusion_acks: &mut std::collections::HashMap<String, CostPersistAck>,
    ack: CostPersistAck,
) -> Result<(), CostPersistError> {
    let event_id = ack.mutation_id.as_str().to_string();
    if let Some(previous) = fusion_acks.insert(event_id.clone(), ack.clone()) {
        if previous != ack {
            return Err(CostPersistError::Storage(format!(
                "fusion journal event id `{event_id}` appears at multiple revisions"
            )));
        }
    }
    Ok(())
}

fn fold_outbox_record(
    record: &DurableFusionOutboxRecord,
    fusion_outbox: &mut std::collections::HashMap<String, DurableFusionOutboxRecord>,
) -> Result<(), CostPersistError> {
    validate_outbox_transition(fusion_outbox.get(&record.delivery_id), record)
        .map_err(CostPersistError::Storage)?;
    fusion_outbox.insert(record.delivery_id.clone(), record.clone());
    Ok(())
}

fn fold_cost_entry(
    entry: session::jsonl::JournalEntry,
    session_id: SessionId,
    last_cost_revision: &mut u64,
    latest: &mut CostState,
    durable_results: &mut std::collections::HashMap<CostMutationId, CachedCostResult>,
    event: CostMutationRecord,
) -> Result<(), CostPersistError> {
    if event.mutation_id.as_str() != entry.event_id {
        return Err(CostPersistError::Storage(
            "cost journal event id disagrees with mutation id".into(),
        ));
    }
    if event.state.session_id != session_id {
        return Err(CostPersistError::Storage(
            "cost journal contains a foreign session vector".into(),
        ));
    }
    if event.cost_revision != event.state.cost_revision {
        return Err(CostPersistError::Storage(
            "cost journal outer and vector revisions disagree".into(),
        ));
    }
    let expected_revision = last_cost_revision
        .checked_add(1)
        .ok_or_else(|| CostPersistError::Storage("cost revision overflow".into()))?;
    if event.cost_revision != expected_revision {
        return Err(CostPersistError::Storage(
            "cost journal cost revisions are not contiguous".into(),
        ));
    }
    let folded = event
        .state
        .clone()
        .try_into_state()
        .map_err(|error| CostPersistError::Storage(error.to_string()))?;
    let mutation_id = event.mutation_id.clone();
    let ack = CostPersistAck {
        mutation_id: mutation_id.clone(),
        journal_revision: entry.journal_revision,
        cost_revision: event.cost_revision,
    };
    *last_cost_revision = event.cost_revision;
    *latest = folded;
    durable_results.insert(
        mutation_id,
        CachedCostResult {
            record: None,
            result: Ok(ack),
        },
    );
    Ok(())
}

#[async_trait]
impl CostPersistence for SessionStateCoordinator {
    async fn acquire_attempt_permit(
        &self,
        session_id: SessionId,
    ) -> Result<AttemptPersistPermit, CostPersistError> {
        let pin = self.retention_pin()?;
        if session_id != self.state.session_id {
            return Err(CostPersistError::Rejected(
                "attempt permit belongs to a different session".into(),
            ));
        }
        self.ensure_admission_open()?;
        if let Some(reason) = self.state.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let permit = self
            .queue_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| CostPersistError::Rejected("session state queue is closed".into()))?;
        self.ensure_admission_open()?;
        if let Some(reason) = self.state.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let gate = self.state.durability_gate.clone();
        Ok(AttemptPersistPermit::new(move |request| {
            if let Some(reason) = gate.frozen_reason() {
                return Err(CostPersistError::Frozen(reason));
            }
            permit.send(QueuedMutation {
                mutation: SessionMutation::Attempt(request),
                _pin: Some(pin),
            });
            Ok(())
        }))
    }

    async fn acquire_permit(
        &self,
        session_id: SessionId,
    ) -> Result<CostPersistPermit, CostPersistError> {
        let pin = self.retention_pin()?;
        if session_id != self.state.session_id {
            return Err(CostPersistError::Rejected(
                "cost permit belongs to a different session".into(),
            ));
        }
        self.ensure_admission_open()?;
        if let Some(reason) = self.state.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let permit = self
            .queue_tx
            .clone()
            .reserve_owned()
            .await
            .map_err(|_| CostPersistError::Rejected("session state queue is closed".into()))?;
        self.ensure_admission_open()?;
        if let Some(reason) = self.state.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let gate = self.state.durability_gate.clone();
        Ok(CostPersistPermit::new(move |request| {
            if let Some(reason) = gate.frozen_reason() {
                return Err(CostPersistError::Frozen(reason));
            }
            permit.send(QueuedMutation {
                mutation: SessionMutation::Cost(request),
                _pin: Some(pin),
            });
            Ok(())
        }))
    }
}

#[async_trait]
impl CostHydrator for SessionStateCoordinator {
    async fn hydrate(&self, session_id: SessionId) -> Result<CostHydration, CostPersistError> {
        if session_id != self.state.session_id {
            return Err(CostPersistError::Rejected(
                "hydration belongs to a different session".into(),
            ));
        }
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || state.hydrate_blocking())
            .await
            .map_err(|error| CostPersistError::Storage(error.to_string()))?
    }
}

fn map_journal_error(error: JournalError) -> CostPersistError {
    CostPersistError::Storage(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::live_sessions::{SessionWriterLease, SharedSessionWriterLease};
    use lingxi_core::host::{FusionError, FusionResult, FusionRunFacts};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestLease(String);

    impl SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    fn lease(session_id: SessionId) -> SharedSessionWriterLease {
        Arc::new(TestLease(session_id.to_string()))
    }

    fn coordinator() -> (tempfile::TempDir, Arc<SessionStateCoordinator>, SessionId) {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let coordinator =
            SessionStateCoordinator::open(directory.path(), session_id, lease(session_id)).unwrap();
        (directory, coordinator, session_id)
    }

    fn state(session_id: SessionId, revision: u64, total: u64) -> CostState {
        CostState {
            session_id,
            cost_revision: revision,
            total_nano_usd: total,
            ..Default::default()
        }
    }

    fn fusion_identity(
        session_id: SessionId,
        origin: lingxi_core::host::FusionOrigin,
    ) -> FusionRunIdentity {
        FusionRunIdentity::new(
            lingxi_core::host::FusionRunId::generated(),
            Some(session_id),
            origin,
            None,
        )
    }

    fn fusion_outbox(
        identity: &FusionRunIdentity,
        payload: serde_json::Value,
    ) -> DurableFusionOutboxRecord {
        DurableFusionOutboxRecord {
            delivery_id: fusion_delivery_id(identity),
            session_id: identity.session_id.expect("test identity has a session"),
            message_uuid: "00000000-0000-0000-0000-000000000001".into(),
            payload,
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::queued(),
        }
    }

    fn fusion_terminal(
        identity: FusionRunIdentity,
        outbox: Option<DurableFusionOutboxRecord>,
    ) -> DurableFusionTerminalRecord {
        DurableFusionTerminalRecord {
            event_id: fusion_terminal_event_id(&identity),
            identity,
            result: Err(FusionError::Internal),
            facts: FusionRunFacts::default(),
            publication: if outbox.is_some() {
                FusionPublicationReceipt::queued()
            } else {
                FusionPublicationReceipt::not_required()
            },
            outbox,
        }
    }

    fn completed_result(run_id: &lingxi_core::host::FusionRunId) -> FusionResult {
        serde_json::from_value(serde_json::json!({
            "run_id": run_id.to_string(),
            "status": "analyzed",
            "responses": [{
                "panel_id": "P1",
                "summary": "summary",
                "candidate_answer": "done"
            }]
        }))
        .unwrap()
    }

    async fn persist_cost(
        coordinator: &SessionStateCoordinator,
        session_id: SessionId,
        revision: u64,
        total: u64,
    ) -> CostPersistAck {
        let mutation_id = CostMutationId::new(format!("cost-{revision}"));
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        coordinator.state.persist_request(CostPersistRequest {
            session_id,
            cost_revision: revision,
            mutation_id,
            state: CostStateVector::from(&state(session_id, revision, total)),
            source: CostMutationSource::ModelResponse,
            ack: ack_tx,
        });
        ack_rx.await.unwrap().unwrap()
    }

    #[test]
    fn constructor_rejects_a_foreign_writer_claim() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let other = SessionId::new();

        assert!(matches!(
            SessionStateCoordinator::open(directory.path(), session_id, lease(other)),
            Err(CostPersistError::Rejected(message)) if message.contains("claim")
        ));
    }

    /// P0-7: the cost ledger is derivative. One damaged byte in it must not
    /// make the conversation unresumable -- the transcript is a separate file
    /// and is untouched. The damaged ledger is set aside, not deleted, and the
    /// session starts a fresh one.
    #[test]
    fn corrupt_ledger_is_quarantined_and_the_session_still_hydrates() {
        let (directory, coordinator, session_id) = coordinator();
        let mutation_id = CostMutationId::new("cost-1");
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(CostMutationRecord {
                    cost_revision: 1,
                    mutation_id: mutation_id.clone(),
                    source: CostMutationSource::ModelResponse,
                    state: CostStateVector::from(&state(session_id, 1, 99)),
                }))
                .unwrap(),
            )
            .unwrap();
        let wal = coordinator.journal().root().join("ledger.v1.jsonl");
        let original = std::fs::read(&wal).unwrap();
        // An interior corrupt record: the reader must keep refusing to parse
        // this, so recovery cannot come from loosening the reader.
        let mut damaged = b"{not json\n".to_vec();
        damaged.extend_from_slice(&original);
        std::fs::write(&wal, &damaged).unwrap();

        let hydrated = coordinator
            .hydrate_blocking()
            .expect("a damaged ledger must not block the session");
        assert_eq!(
            hydrated.state.total_nano_usd, 0,
            "the fresh ledger starts from zero"
        );

        let quarantined: Vec<_> =
            std::fs::read_dir(coordinator.journal().root().join("quarantine"))
                .expect("a quarantine directory exists")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .collect();
        let ledger = quarantined
            .iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("ledger.v1."))
            })
            .expect("the damaged ledger was set aside");
        assert_eq!(
            std::fs::read(ledger).unwrap(),
            damaged,
            "the original bytes are preserved for inspection"
        );
        drop(directory);
    }

    /// A write that never reached the file is survivable: the journal only
    /// advances its revision after the write, its fsync and its fingerprint
    /// check all pass, so nothing was left behind and the session continues.
    #[test]
    fn a_write_that_left_nothing_behind_recovers_the_session() {
        let (_directory, coordinator, session_id) = coordinator();
        coordinator.hydrate_blocking().unwrap();
        let error = CostPersistError::Storage("transient volume error".into());

        freeze_then_try_recovery(&coordinator.state, &error);

        assert!(
            coordinator.durability_gate().frozen_reason().is_none(),
            "an intact ledger must not stay shut after a failed write"
        );
        // And it is still usable: the projection and the file agree, so the
        // next mutation is accepted rather than refused.
        assert_eq!(
            coordinator.hydrate_blocking().unwrap().state.session_id,
            session_id
        );
    }

    /// A record that landed while the projection never folded it is NOT
    /// survivable, even though the file replays perfectly: later work would
    /// build on a projection that is behind the disk.
    #[test]
    fn a_landed_record_the_projection_missed_keeps_the_session_shut() {
        let (_directory, coordinator, session_id) = coordinator();
        coordinator.hydrate_blocking().unwrap();
        let mutation_id = CostMutationId::new("landed-but-unfolded");
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(CostMutationRecord {
                    cost_revision: 1,
                    mutation_id: mutation_id.clone(),
                    source: CostMutationSource::ModelResponse,
                    state: CostStateVector::from(&state(session_id, 1, 99)),
                }))
                .unwrap(),
            )
            .unwrap();

        freeze_then_try_recovery(
            &coordinator.state,
            &CostPersistError::Storage("acknowledgement lost after the write".into()),
        );

        assert!(
            coordinator.durability_gate().frozen_reason().is_some(),
            "a projection behind the disk must not be treated as intact"
        );
    }

    /// A durability freeze is bounded to the session that hit it. Each
    /// coordinator mints its own gate, so starting a new session recovers
    /// without a restart -- which is what the user-facing notice promises.
    #[test]
    fn a_freeze_does_not_follow_the_process_into_a_new_session() {
        let directory = tempfile::tempdir().unwrap();
        let frozen_id = SessionId::new();
        let frozen =
            SessionStateCoordinator::open(directory.path(), frozen_id, lease(frozen_id)).unwrap();
        frozen.durability_gate().freeze("ledger volume went away");
        assert!(frozen.durability_gate().frozen_reason().is_some());

        let fresh_id = SessionId::new();
        let fresh =
            SessionStateCoordinator::open(directory.path(), fresh_id, lease(fresh_id)).unwrap();
        assert!(
            fresh.durability_gate().frozen_reason().is_none(),
            "a new session must not inherit another session's freeze"
        );
        assert!(fresh.hydrate_blocking().is_ok());
    }

    /// The fatal classes stay fatal, and leave no quarantine behind. A frozen
    /// gate means this process already knows its own writes are failing;
    /// rebuilding on top of that would be inventing a clean state.
    #[test]
    fn a_frozen_gate_is_not_quarantined() {
        let (_directory, coordinator, _session_id) = coordinator();
        coordinator
            .durability_gate()
            .freeze("this process cannot write");
        assert!(matches!(
            coordinator.hydrate_blocking(),
            Err(CostPersistError::Frozen(_))
        ));
        assert!(
            !coordinator.journal().root().join("quarantine").exists(),
            "a frozen gate must not move the ledger aside"
        );
    }

    #[test]
    fn snapshot_without_authoritative_wal_is_quarantined_not_trusted() {
        let (_directory, coordinator, session_id) = coordinator();
        coordinator
            .journal()
            .write_snapshot(1, &CostStateVector::from(&state(session_id, 1, 99)))
            .unwrap();

        // A snapshot is derivative of the WAL. Alone it has no authority, so
        // it is set aside; the totals it claimed are never adopted.
        let hydrated = coordinator.hydrate_blocking().unwrap();
        assert_eq!(hydrated.state.total_nano_usd, 0);
        assert!(coordinator.journal().root().join("quarantine").exists());
    }

    /// The projection snapshot has exactly one production reader: hydration
    /// treats a snapshot beside a missing WAL as proof that an authoritative
    /// ledger was lost. That reader needs the file to EXIST whenever the WAL
    /// does and never reads what is inside it. Rewriting the whole projection
    /// on every append therefore bought that reader nothing, while costing one
    /// fsync and one serialization of every terminal record -- each carrying a
    /// run's full answer text -- per model response.
    #[tokio::test]
    async fn appends_do_not_rewrite_the_snapshot_but_a_flush_does() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator = manager.ensure_coordinator(session_id).await.unwrap();
        let snapshot = coordinator
            .journal()
            .root()
            .join(session::jsonl::SNAPSHOT_FILE_NAME);
        assert!(snapshot.exists(), "hydration leaves a snapshot behind");
        std::fs::remove_file(&snapshot).unwrap();

        for revision in 2..=5 {
            persist_cost(&coordinator, session_id, revision, revision * 10).await;
        }

        assert!(
            !snapshot.exists(),
            "an append rewrote the whole projection snapshot"
        );

        manager.flush_all().await.unwrap();
        assert!(
            snapshot.exists(),
            "a flush barrier must leave a current snapshot behind"
        );
    }

    /// A lost ledger is detected by finding a snapshot with no WAL beside it.
    /// That inference only holds while every WAL has a snapshot, so the first
    /// append after hydration still writes one. This guards the fix above from
    /// overshooting into silence.
    #[tokio::test]
    async fn the_first_append_leaves_a_snapshot_so_a_lost_wal_stays_detectable() {
        let (_directory, coordinator, session_id) = coordinator();
        coordinator.hydrate_blocking().unwrap();
        let snapshot = coordinator
            .journal()
            .root()
            .join(session::jsonl::SNAPSHOT_FILE_NAME);
        assert!(!snapshot.exists(), "a session with no WAL has no snapshot");

        persist_cost(&coordinator, session_id, 1, 10).await;

        assert!(
            snapshot.exists(),
            "a WAL with no snapshot beside it hides its own loss"
        );
    }

    #[test]
    fn valid_wal_rebuilds_a_corrupt_snapshot() {
        let (_directory, coordinator, session_id) = coordinator();
        let state = state(session_id, 1, 99);
        let mutation_id = CostMutationId::new("cost-1");
        let record = CostMutationRecord {
            cost_revision: 1,
            mutation_id: mutation_id.clone(),
            source: CostMutationSource::ModelResponse,
            state: CostStateVector::from(&state),
        };
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(record)).unwrap(),
            )
            .unwrap();
        std::fs::write(
            coordinator
                .journal()
                .root()
                .join(session::jsonl::SNAPSHOT_FILE_NAME),
            b"not-json",
        )
        .unwrap();

        let hydration = coordinator.hydrate_blocking().unwrap();

        assert_eq!(hydration.state, state);
        let repaired = coordinator
            .journal()
            .read_snapshot::<serde_json::Value>()
            .unwrap()
            .unwrap();
        assert_eq!(repaired.last_journal_revision, 1);
        let repaired = decode_projection_snapshot(repaired.state).unwrap();
        assert_eq!(repaired.cost, CostStateVector::from(&state));
    }

    #[test]
    fn replay_rejects_outer_and_vector_revision_mismatch() {
        let (_directory, coordinator, session_id) = coordinator();
        let mutation_id = CostMutationId::new("cost-1");
        let record = CostMutationRecord {
            cost_revision: 1,
            mutation_id: mutation_id.clone(),
            source: CostMutationSource::ModelResponse,
            state: CostStateVector::from(&state(session_id, 2, 99)),
        };
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(record)).unwrap(),
            )
            .unwrap();

        assert!(matches!(
            coordinator.hydrate_blocking(),
            Err(CostPersistError::Storage(message)) if message.contains("revisions disagree")
        ));
    }

    #[test]
    fn exact_flat_legacy_import_marker_reuses_its_original_ack() {
        let (_directory, coordinator, session_id) = coordinator();
        let mutation_id = CostMutationId::new("legacy-import-evaluated:v1:test");
        let mut marker = state(session_id, 1, 0);
        marker.legacy_import_evaluated = true;
        let record = CostMutationRecord {
            cost_revision: 1,
            mutation_id: mutation_id.clone(),
            source: CostMutationSource::LegacyImportEvaluated,
            state: CostStateVector::from(&marker),
        };
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(record)).unwrap(),
            )
            .unwrap();

        let hydration = coordinator.hydrate_blocking().unwrap();
        assert!(hydration.state.legacy_import_evaluated);
        assert_eq!(
            coordinator.state.legacy_import_ack().unwrap(),
            Some(CostPersistAck {
                mutation_id,
                journal_revision: 1,
                cost_revision: 1,
            })
        );
    }

    #[tokio::test]
    async fn duplicate_mutation_id_compares_content_before_reusing_result() {
        let (_directory, coordinator, session_id) = coordinator();
        let mutation_id = CostMutationId::new("cost-1");
        let (first_ack, first_rx) = tokio::sync::oneshot::channel();
        coordinator.state.persist_request(CostPersistRequest {
            session_id,
            cost_revision: 1,
            mutation_id: mutation_id.clone(),
            state: CostStateVector::from(&state(session_id, 1, 10)),
            source: CostMutationSource::ModelResponse,
            ack: first_ack,
        });
        assert!(first_rx.await.unwrap().is_ok());

        let (conflict_ack, conflict_rx) = tokio::sync::oneshot::channel();
        coordinator.state.persist_request(CostPersistRequest {
            session_id,
            cost_revision: 1,
            mutation_id,
            state: CostStateVector::from(&state(session_id, 1, 11)),
            source: CostMutationSource::ModelResponse,
            ack: conflict_ack,
        });

        assert!(matches!(
            conflict_rx.await.unwrap(),
            Err(CostPersistError::Storage(message)) if message.contains("conflict")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 1);
        assert!(coordinator.durability_gate().frozen_reason().is_some());
    }

    #[tokio::test]
    async fn replayed_mutation_returns_its_original_ack_before_next_revision_validation() {
        let (_directory, coordinator, session_id) = coordinator();
        let mutation_id = CostMutationId::new("replayed-cost-1");
        let persisted = CostMutationRecord {
            cost_revision: 1,
            mutation_id: mutation_id.clone(),
            source: CostMutationSource::ModelResponse,
            state: CostStateVector::from(&state(session_id, 1, 10)),
        };
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(persisted.clone())).unwrap(),
            )
            .unwrap();
        let writer = coordinator.start().await.unwrap();
        assert!(
            coordinator
                .state
                .results
                .lock()
                .unwrap()
                .get(&mutation_id)
                .expect("hydration seeds a stable duplicate result")
                .record
                .is_none(),
            "successful replay cache must not retain a cumulative cost vector"
        );

        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let (same_ack, same_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: 1,
                mutation_id: mutation_id.clone(),
                state: persisted.state.clone(),
                source: persisted.source,
                ack: same_ack,
            })
            .unwrap();
        let ack = same_rx.await.unwrap().unwrap();
        assert_eq!(ack.mutation_id, mutation_id);
        assert_eq!(ack.journal_revision, 1);
        assert_eq!(ack.cost_revision, 1);

        // The replay cache compares the complete durable record, not only its
        // id/revision. A changed source or vector is a conflict and freezes.
        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let (conflict_ack, conflict_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: 1,
                mutation_id,
                state: CostStateVector::from(&state(session_id, 1, 11)),
                source: CostMutationSource::Administrative,
                ack: conflict_ack,
            })
            .unwrap();
        assert!(matches!(
            conflict_rx.await.unwrap(),
            Err(CostPersistError::Storage(message)) if message.contains("conflict")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 1);
        assert!(coordinator.durability_gate().frozen_reason().is_some());

        drop(coordinator);
        writer.await.unwrap();
    }

    #[test]
    fn invalid_fusion_terminal_semantics_are_rejected_before_the_wal() {
        let (_directory, coordinator, session_id) = coordinator();

        let agent = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Agent);
        let agent_outbox = fusion_outbox(&agent, serde_json::json!({"body": "not trusted"}));
        let record = fusion_terminal(agent, Some(agent_outbox));
        assert!(matches!(
            coordinator
                .state
                .persist_fusion_terminal(&record.event_id, &record),
            Err(CostPersistError::Rejected(message)) if message.contains("Slash")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().last_revision, 0);

        let slash = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let mut foreign = fusion_outbox(&slash, serde_json::json!({"body": "foreign"}));
        foreign.session_id = SessionId::new();
        let record = fusion_terminal(slash, Some(foreign));
        assert!(matches!(
            coordinator
                .state
                .persist_fusion_terminal(&record.event_id, &record),
            Err(CostPersistError::Rejected(message)) if message.contains("different session")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().last_revision, 0);

        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Agent);
        let mut record = fusion_terminal(identity, None);
        record.result = Ok(completed_result(
            &lingxi_core::host::FusionRunId::generated(),
        ));
        assert!(matches!(
            coordinator
                .state
                .persist_fusion_terminal(&record.event_id, &record),
            Err(CostPersistError::Rejected(message)) if message.contains("different run")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().last_revision, 0);

        let slash = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let mut invalid_cycle = fusion_outbox(&slash, serde_json::json!({"body": "answer"}));
        invalid_cycle.retry_cycle_end = 5;
        let record = fusion_terminal(slash, Some(invalid_cycle));
        assert!(matches!(
            coordinator
                .state
                .persist_fusion_terminal(&record.event_id, &record),
            Err(CostPersistError::Rejected(message)) if message.contains("initial queued outbox")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().last_revision, 0);
    }

    #[test]
    fn string_payload_fusion_errors_are_durable_terminal_records() {
        let (_directory, coordinator, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let mut record = fusion_terminal(identity, None);
        record.result = Err(FusionError::InvalidConfiguration(
            "a configuration value is invalid".into(),
        ));

        coordinator
            .state
            .persist_fusion_terminal(&record.event_id, &record)
            .expect("string-payload Fusion errors must serialize into the journal");

        assert_eq!(coordinator.fusion_terminal(&record.event_id), Some(record));
        assert!(coordinator.durability_gate().frozen_reason().is_none());
    }

    #[test]
    fn replay_reuses_the_same_terminal_semantic_validator() {
        let (_directory, coordinator, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Agent);
        let mut invalid = fusion_terminal(identity, None);
        invalid.result = Ok(completed_result(
            &lingxi_core::host::FusionRunId::generated(),
        ));
        let event = encode_session_event(&SessionEvent::FusionTerminal(invalid.clone())).unwrap();
        coordinator
            .journal()
            .append_once(invalid.event_id.clone(), &event)
            .unwrap();

        assert!(matches!(
            coordinator.hydrate_blocking(),
            Err(CostPersistError::Storage(message)) if message.contains("different run")
        ));
    }

    #[test]
    fn retry_cycle_semantics_are_rejected_before_wal_and_during_replay() {
        let (_directory, primary, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let queued = fusion_outbox(&identity, serde_json::json!({"body": "answer"}));
        let terminal = fusion_terminal(identity, Some(queued.clone()));
        primary
            .state
            .persist_fusion_terminal(&terminal.event_id, &terminal)
            .unwrap();

        let mut impossible = queued.clone();
        impossible.attempt = 5;
        assert!(matches!(
            primary
                .state
                .persist_fusion_outbox(&outbox_event_id(&impossible), &impossible),
            Err(CostPersistError::Rejected(message)) if message.contains("exceeds")
        ));
        assert_eq!(primary.journal().replay().unwrap().last_revision, 1);

        let (_replay_directory, replay, replay_session_id) = coordinator();
        let replay_identity =
            fusion_identity(replay_session_id, lingxi_core::host::FusionOrigin::Slash);
        let replay_queued = fusion_outbox(
            &replay_identity,
            serde_json::json!({"body": "replay answer"}),
        );
        let replay_terminal = fusion_terminal(replay_identity, Some(replay_queued.clone()));
        let terminal_event =
            encode_session_event(&SessionEvent::FusionTerminal(replay_terminal.clone())).unwrap();
        replay
            .journal()
            .append_once(&replay_terminal.event_id, &terminal_event)
            .unwrap();
        let mut replay_impossible = replay_queued;
        replay_impossible.attempt = 5;
        let replay_event =
            encode_session_event(&SessionEvent::FusionOutbox(replay_impossible.clone())).unwrap();
        replay
            .journal()
            .append_once(outbox_event_id(&replay_impossible), &replay_event)
            .unwrap();
        assert!(matches!(
            replay.hydrate_blocking(),
            Err(CostPersistError::Storage(message)) if message.contains("exceeds")
        ));
    }

    #[tokio::test]
    async fn noncost_duplicates_keep_their_original_ack_after_cost_and_outbox_progress() {
        let (_directory, coordinator, session_id) = coordinator();
        let cost_one = persist_cost(&coordinator, session_id, 1, 10).await;
        assert_eq!(cost_one.journal_revision, 1);

        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let queued = fusion_outbox(&identity, serde_json::json!({"body": "stable"}));
        let terminal = fusion_terminal(identity, Some(queued.clone()));
        let terminal_ack = coordinator
            .state
            .persist_fusion_terminal(&terminal.event_id, &terminal)
            .unwrap();
        assert_eq!(terminal_ack.journal_revision, 2);
        assert_eq!(terminal_ack.cost_revision, 1);

        let queued_event_id = outbox_event_id(&queued);
        let queued_ack = coordinator
            .state
            .persist_fusion_outbox(&queued_event_id, &queued)
            .unwrap();
        assert_eq!(queued_ack.journal_revision, 3);
        assert_eq!(queued_ack.cost_revision, 1);

        let mut published = queued.clone();
        published.receipt = FusionPublicationReceipt::published();
        let published_event_id = outbox_event_id(&published);
        let published_ack = coordinator
            .state
            .persist_fusion_outbox(&published_event_id, &published)
            .unwrap();
        assert_eq!(published_ack.journal_revision, 4);
        assert_eq!(published_ack.cost_revision, 1);

        let cost_two = persist_cost(&coordinator, session_id, 2, 20).await;
        assert_eq!(cost_two.journal_revision, 5);
        assert_eq!(
            coordinator
                .state
                .persist_fusion_terminal(&terminal.event_id, &terminal)
                .unwrap(),
            terminal_ack
        );
        assert_eq!(
            coordinator
                .state
                .persist_fusion_outbox(&published_event_id, &published)
                .unwrap(),
            published_ack
        );

        // An old exact queued status can also retry after Published without
        // changing the latest projection or receiving a newer acknowledgment.
        let replayed_queued_ack = coordinator
            .state
            .persist_fusion_outbox(&queued_event_id, &queued)
            .unwrap();
        assert_eq!(replayed_queued_ack, queued_ack);
        assert_eq!(
            coordinator.fusion_outbox(&published.delivery_id),
            Some(published.clone())
        );
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 5);
    }

    #[test]
    fn published_outbox_is_absorbing_and_retry_generations_are_wide_and_contiguous() {
        let (_directory, coordinator, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let queued = fusion_outbox(&identity, serde_json::json!({"body": "stable"}));
        let terminal = fusion_terminal(identity, Some(queued.clone()));
        coordinator
            .state
            .persist_fusion_terminal(&terminal.event_id, &terminal)
            .unwrap();

        let mut published = queued.clone();
        published.receipt = FusionPublicationReceipt::published();
        coordinator
            .state
            .persist_fusion_outbox(&outbox_event_id(&published), &published)
            .unwrap();
        let mut stale_retry = queued.clone();
        stale_retry.attempt = 1;
        assert!(matches!(
            coordinator
                .state
                .persist_fusion_outbox(&outbox_event_id(&stale_retry), &stale_retry),
            Err(CostPersistError::Rejected(message)) if message.contains("absorbing")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 2);
        assert_eq!(
            coordinator.fusion_outbox(&published.delivery_id),
            Some(published.clone())
        );
        let hydrated = hydrate_from_journal(&coordinator.journal(), session_id).unwrap();
        assert_eq!(
            hydrated.fusion_outbox.get(&published.delivery_id),
            Some(&published)
        );

        let mut failed = queued.clone();
        failed.attempt = u64::from(u8::MAX);
        failed.retry_cycle_end = u64::from(u8::MAX);
        failed.receipt = FusionPublicationReceipt::outbox_failed("retry");
        let wide = DurableFusionOutboxRecord {
            attempt: u64::from(u8::MAX) + 1,
            retry_cycle_end: u64::from(u8::MAX) + 5,
            ..queued
        };
        assert_eq!(wide.attempt, 256);
        assert_eq!(wide.checked_next_attempt(), Some(257));
        assert!(validate_outbox_transition(Some(&failed), &wide).is_ok());
        let exhausted = DurableFusionOutboxRecord {
            attempt: u64::MAX,
            retry_cycle_end: u64::MAX,
            receipt: FusionPublicationReceipt::outbox_failed("exhausted"),
            ..wide.clone()
        };
        assert_eq!(exhausted.checked_next_attempt(), None);
        let impossible_next = DurableFusionOutboxRecord {
            attempt: 0,
            receipt: FusionPublicationReceipt::queued(),
            ..wide
        };
        assert!(matches!(
            validate_outbox_transition(Some(&exhausted), &impossible_next),
            Err(message) if message.contains("absorbing") || message.contains("regressed")
        ));
    }

    #[test]
    fn outbox_retry_cycle_changes_only_at_an_exhausted_failed_boundary() {
        let (_directory, _coordinator, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let queued = fusion_outbox(&identity, serde_json::json!({"body": "stable"}));

        let mut changed_same_attempt = queued.clone();
        changed_same_attempt.retry_cycle_end = 9;
        assert!(matches!(
            validate_outbox_transition(Some(&queued), &changed_same_attempt),
            Err(message) if message.contains("within an attempt")
        ));

        let mut failed_early = queued.clone();
        failed_early.attempt = 1;
        failed_early.receipt = FusionPublicationReceipt::outbox_failed("retry");
        let mut premature_extension = failed_early.clone();
        premature_extension.attempt = 2;
        premature_extension.retry_cycle_end = 6;
        premature_extension.receipt = FusionPublicationReceipt::queued();
        assert!(matches!(
            validate_outbox_transition(Some(&failed_early), &premature_extension),
            Err(message) if message.contains("outside its boundary")
        ));

        let mut failed_at_boundary = queued.clone();
        failed_at_boundary.attempt = 4;
        failed_at_boundary.receipt = FusionPublicationReceipt::outbox_failed("cycle exhausted");
        let mut next_cycle = failed_at_boundary.clone();
        next_cycle.attempt = 5;
        next_cycle.retry_cycle_end = 9;
        next_cycle.receipt = FusionPublicationReceipt::queued();
        assert!(validate_outbox_transition(Some(&failed_at_boundary), &next_cycle).is_ok());

        let mut wrong_extension = next_cycle.clone();
        wrong_extension.retry_cycle_end = 10;
        assert!(matches!(
            validate_outbox_transition(Some(&failed_at_boundary), &wrong_extension),
            Err(message) if message.contains("outside its boundary")
        ));
    }

    #[test]
    fn terminal_outbox_conflict_cannot_poison_the_authoritative_wal() {
        let (_directory, coordinator, session_id) = coordinator();
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let original = fusion_outbox(&identity, serde_json::json!({"body": "original"}));
        coordinator
            .state
            .projection
            .lock()
            .unwrap()
            .fusion_outbox
            .insert(original.delivery_id.clone(), original.clone());
        let conflicting = fusion_outbox(&identity, serde_json::json!({"body": "changed"}));
        let terminal = fusion_terminal(identity, Some(conflicting));

        assert!(matches!(
            coordinator
                .state
                .persist_fusion_terminal(&terminal.event_id, &terminal),
            Err(CostPersistError::Rejected(message)) if message.contains("status history")
        ));
        assert_eq!(coordinator.journal().replay().unwrap().last_revision, 0);
        assert_eq!(
            coordinator.fusion_outbox(&original.delivery_id),
            Some(original)
        );
    }

    #[tokio::test]
    async fn mixed_replay_keeps_cost_and_journal_revisions_independent() {
        let (_directory, coordinator, session_id) = coordinator();
        persist_cost(&coordinator, session_id, 1, 10).await;
        let identity = fusion_identity(session_id, lingxi_core::host::FusionOrigin::Slash);
        let queued = fusion_outbox(&identity, serde_json::json!({"body": "stable"}));
        let terminal = fusion_terminal(identity, Some(queued.clone()));
        coordinator
            .state
            .persist_fusion_terminal(&terminal.event_id, &terminal)
            .unwrap();
        let mut published = queued.clone();
        published.receipt = FusionPublicationReceipt::published();
        coordinator
            .state
            .persist_fusion_outbox(&outbox_event_id(&published), &published)
            .unwrap();
        persist_cost(&coordinator, session_id, 2, 20).await;

        let hydrated = hydrate_from_journal(&coordinator.journal(), session_id).unwrap();
        assert_eq!(hydrated.hydration.journal_revision, 4);
        assert_eq!(hydrated.hydration.state.cost_revision, 2);
        assert_eq!(hydrated.hydration.state.total_nano_usd, 20);
        assert_eq!(
            hydrated.fusion_terminals.get(&terminal.event_id),
            Some(&terminal)
        );
        assert_eq!(
            hydrated.fusion_outbox.get(&published.delivery_id),
            Some(&published)
        );
        assert_eq!(
            hydrated
                .fusion_acks
                .get(&terminal.event_id)
                .expect("terminal ack is rebuilt")
                .cost_revision,
            1
        );
    }

    #[tokio::test]
    async fn manager_imports_a_captured_legacy_decision_once_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let manager = SessionStateManager::new_with_legacy_shadow(
            directory.path(),
            Some(Arc::new(move |candidate| {
                observed.fetch_add(1, Ordering::SeqCst);
                (candidate == session_id).then_some(75)
            })),
        );

        let first = manager.ensure_coordinator(session_id).await.unwrap();
        let projection = first
            .projection()
            .expect("new coordinator is hydrated before publication")
            .0;
        assert_eq!(projection.total_nano_usd, 75);
        assert_eq!(projection.legacy_opening_balance_nano_usd, 75);
        assert!(projection.legacy_import_evaluated);
        assert_eq!(first.journal().replay().unwrap().entries.len(), 1);
        assert_eq!(manager.session_ids(), vec![session_id]);

        let reused = manager.ensure_coordinator(session_id).await.unwrap();
        assert!(first.shares_authority(&reused));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first.journal().replay().unwrap().entries.len(), 1);
        manager.flush_all().await.unwrap();
    }

    #[tokio::test]
    async fn manager_records_a_none_legacy_marker_and_fifo_flushes_accepted_work() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator = manager.ensure_coordinator(session_id).await.unwrap();
        let current = coordinator.projection().unwrap().0;
        assert!(current.legacy_import_evaluated);
        assert_eq!(current.legacy_opening_balance_nano_usd, 0);

        let mut next = current;
        next.cost_revision = 2;
        next.total_nano_usd = 9;
        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: 2,
                mutation_id: CostMutationId::new("accepted-before-manager-flush"),
                state: CostStateVector::from(&next),
                source: CostMutationSource::ModelResponse,
                ack: ack_tx,
            })
            .unwrap();

        manager.flush_all().await.unwrap();
        let ack = ack_rx
            .await
            .expect("FIFO barrier cannot pass an accepted mutation")
            .unwrap();
        assert_eq!(ack.cost_revision, 2);
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 2);

        coordinator.durability_gate().freeze("test freeze");
        manager
            .flush_all()
            .await
            .expect("draining accepted work bypasses the paid-work freeze gate");
    }

    #[tokio::test]
    async fn code_change_ack_progresses_past_256_with_a_stale_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator = manager.ensure_coordinator(session_id).await.unwrap();
        let hydration = coordinator.hydrate(session_id).await.unwrap();
        let (legacy_tx, _legacy_rx) = tokio::sync::mpsc::channel(1);
        let tracker = CostTracker::new(
            session_id,
            Arc::new(cost::PricingCatalog::builtin_reference()),
            legacy_tx,
        )
        .try_with_durable_persistence(
            hydration,
            coordinator.clone() as Arc<dyn CostPersistence>,
            coordinator.writer_lease(),
            coordinator.durability_gate(),
        )
        .unwrap();
        let snapshot_path = coordinator
            .journal()
            .root()
            .join(session::jsonl::SNAPSHOT_FILE_NAME);
        let initial_snapshot = std::fs::read(&snapshot_path).unwrap();
        for _ in 0..258 {
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                tracker.record_code_change(1, 1),
            )
            .await
            .expect("an acknowledged edit must allow the tool turn to continue");
        }
        let state = tracker.snapshot().await;
        assert_eq!(state.total_lines_added, 258);
        assert_eq!(state.total_lines_removed, 258);
        assert!(coordinator.durability_gate().frozen_reason().is_none());
        let replay = coordinator.journal().replay().unwrap();
        assert!(replay.entries.last().unwrap().journal_revision > 257);
        assert_eq!(std::fs::read(&snapshot_path).unwrap(), initial_snapshot);
        manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn manager_prepares_b_without_moving_the_active_cost_projection() {
        let directory = tempfile::tempdir().unwrap();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator_a = manager.ensure_coordinator(session_a).await.unwrap();
        let hydration_a = coordinator_a.hydrate(session_a).await.unwrap();
        let (legacy_tx, _legacy_rx) = tokio::sync::mpsc::channel(1);
        let tracker = Arc::new(
            CostTracker::new(
                session_a,
                Arc::new(cost::PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration_a,
                coordinator_a.clone() as Arc<dyn CostPersistence>,
                coordinator_a.writer_lease(),
                coordinator_a.durability_gate(),
            )
            .unwrap(),
        );

        let prepared =
            <SessionStateManager as orchestrator::conversation::CostSessionSwitcher>::prepare_session(
                manager.as_ref(),
                tracker.clone(),
                session_b,
            )
            .await
            .unwrap();

        assert_eq!(tracker.session_id().await, session_a);
        assert_eq!(tracker.snapshot().await.session_id, session_a);
        assert!(manager.coordinator(session_b).is_some());
        drop(prepared);
        assert_eq!(tracker.session_id().await, session_a);
        manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn final_owner_drop_closes_the_queue_after_draining_accepted_mutations() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let concrete_lease = Arc::new(TestLease(session_id.to_string()));
        let weak_lease = Arc::downgrade(&concrete_lease);
        let shared_lease: SharedSessionWriterLease = concrete_lease.clone();
        let coordinator =
            SessionStateCoordinator::open(directory.path(), session_id, shared_lease).unwrap();
        drop(concrete_lease);
        let writer = coordinator.start().await.unwrap();
        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let mutation_id = CostMutationId::new("accepted-before-owner-drop");
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: 1,
                mutation_id: mutation_id.clone(),
                state: CostStateVector::from(&state(session_id, 1, 41)),
                source: CostMutationSource::ModelResponse,
                ack: ack_tx,
            })
            .unwrap();

        // No worker-owned sender remains. Dropping the final external owner
        // closes the channel, but the accepted request is drained first.
        drop(coordinator);
        let ack = ack_rx.await.unwrap().unwrap();
        assert_eq!(ack.mutation_id, mutation_id);
        writer.await.unwrap();
        assert!(
            weak_lease.upgrade().is_none(),
            "writer claim is released after the closed queue is drained"
        );

        let relative = PathBuf::from("session-state").join(session_id.as_uuid().to_string());
        let journal = DurableJournal::open_under(directory.path(), &relative).unwrap();
        assert_eq!(journal.replay().unwrap().entries.len(), 1);
    }

    #[tokio::test]
    async fn explicit_close_waits_for_an_issued_claim_and_survives_a_dropped_start_handle() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let concrete_lease = Arc::new(TestLease(session_id.to_string()));
        let weak_lease = Arc::downgrade(&concrete_lease);
        let shared_lease: SharedSessionWriterLease = concrete_lease.clone();
        let coordinator =
            SessionStateCoordinator::open(directory.path(), session_id, shared_lease).unwrap();
        drop(concrete_lease);
        drop(coordinator.start().await.unwrap());

        // This permit is an already-issued queue claim. Closing admission must
        // wait for it rather than letting Receiver::close discard its mutation.
        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let closing_coordinator = coordinator.clone();
        let close = tokio::spawn(async move { closing_coordinator.close_and_drain().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());

        let mutation_id = CostMutationId::new("claimed-before-explicit-close");
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: 1,
                mutation_id: mutation_id.clone(),
                state: CostStateVector::from(&state(session_id, 1, 43)),
                source: CostMutationSource::ModelResponse,
                ack: ack_tx,
            })
            .unwrap();
        assert_eq!(ack_rx.await.unwrap().unwrap().mutation_id, mutation_id);
        close.await.unwrap().unwrap();
        assert!(matches!(
            coordinator.acquire_permit(session_id).await,
            Err(CostPersistError::Rejected(message)) if message.contains("closed")
        ));
        assert!(weak_lease.upgrade().is_some());
        drop(coordinator);
        assert!(
            weak_lease.upgrade().is_none(),
            "worker completion must not retain the writer lease"
        );
    }

    #[tokio::test]
    async fn dropped_start_during_blocked_hydration_is_owned_until_close_completes() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let concrete_lease = Arc::new(TestLease(session_id.to_string()));
        let weak_lease = Arc::downgrade(&concrete_lease);
        let shared_lease: SharedSessionWriterLease = concrete_lease.clone();
        let coordinator =
            SessionStateCoordinator::open(directory.path(), session_id, shared_lease).unwrap();
        drop(concrete_lease);
        let (block, entered) = TestHydrationBlock::new();
        coordinator.block_start_hydration(block.clone());

        let starting_coordinator = coordinator.clone();
        let start = tokio::spawn(async move { starting_coordinator.start().await });
        entered.await.unwrap();
        start.abort();
        let start_was_cancelled = start.await.is_err_and(|error| error.is_cancelled());

        let closing_coordinator = coordinator.clone();
        let close = tokio::spawn(async move { closing_coordinator.close_and_drain().await });
        tokio::task::yield_now().await;
        let close_finished_while_blocked = close.is_finished();
        block.release();
        assert!(start_was_cancelled);
        assert!(
            !close_finished_while_blocked,
            "close cannot publish completion while hydration still owns state"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), close)
            .await
            .expect("owned hydration and writer should finish")
            .unwrap()
            .unwrap();
        drop(coordinator);
        assert!(
            weak_lease.upgrade().is_none(),
            "the completion latch must cover the non-cancellable hydration owner"
        );
    }

    #[tokio::test]
    async fn manager_close_drains_then_releases_only_its_session_owners() {
        let directory = tempfile::tempdir().unwrap();
        let session_id = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator = manager.ensure_coordinator(session_id).await.unwrap();
        let mut next = coordinator.projection().unwrap().0;
        next.cost_revision += 1;
        next.total_nano_usd = 51;
        let permit = coordinator.acquire_permit(session_id).await.unwrap();
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit
            .enqueue(CostPersistRequest {
                session_id,
                cost_revision: next.cost_revision,
                mutation_id: CostMutationId::new("accepted-before-manager-close"),
                state: CostStateVector::from(&next),
                source: CostMutationSource::ModelResponse,
                ack: ack_tx,
            })
            .unwrap();

        manager.close_and_drain().await.unwrap();
        assert!(ack_rx.await.unwrap().is_ok());
        assert!(manager.session_ids().is_empty());
        assert!(coordinator.acquire_permit(session_id).await.is_err());

        let live = lingxi_core::host::live_sessions::LiveSessionDir::at_live(
            directory.path().join("sessions"),
        );
        assert!(
            live.claim_session_id(&session_id.to_string(), std::process::id())
                .is_err(),
            "an external live scope must retain the same writer claim"
        );
        drop(coordinator);
        live.claim_session_id(&session_id.to_string(), std::process::id())
            .expect("claim is synchronously reusable after the final scope drops");
    }

    #[tokio::test]
    async fn dropped_hot_mount_never_publishes_and_next_claim_waits_for_retirement() {
        let directory = tempfile::tempdir().unwrap();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let manager = SessionStateManager::new(directory.path());
        let coordinator_a = manager.ensure_coordinator(session_a).await.unwrap();
        let (block, entered) = TestHydrationBlock::new();
        manager.block_next_start_hydration(block.clone());

        let mounting_manager = manager.clone();
        let mount =
            tokio::spawn(async move { mounting_manager.ensure_coordinator(session_b).await });
        entered.await.unwrap();
        mount.abort();
        let mount_was_cancelled = mount.await.is_err_and(|error| error.is_cancelled());
        let cancelled_mount_was_not_published = manager.coordinator(session_b).is_none();

        let retry_manager = manager.clone();
        let retry = tokio::spawn(async move { retry_manager.ensure_coordinator(session_b).await });
        tokio::task::yield_now().await;
        let retry_finished_before_retirement = retry.is_finished();
        block.release();
        assert!(mount_was_cancelled);
        assert!(cancelled_mount_was_not_published);
        assert!(
            !retry_finished_before_retirement,
            "the next B claim must wait behind the owned retiring mount"
        );
        let coordinator_b = tokio::time::timeout(std::time::Duration::from_secs(2), retry)
            .await
            .expect("retirement should unblock a fresh B mount")
            .unwrap()
            .unwrap();
        assert!(coordinator_b.shares_authority(
            &manager
                .coordinator(session_b)
                .expect("only retry publishes B")
        ));
        assert!(
            coordinator_a.shares_authority(&manager.ensure_coordinator(session_a).await.unwrap())
        );
        manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn failed_hot_session_initialization_joins_b_without_disturbing_a() {
        let directory = tempfile::tempdir().unwrap();
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let home = directory.path().to_path_buf();
        let sabotage_home = home.clone();
        let manager = SessionStateManager::new_with_legacy_shadow(
            &home,
            Some(Arc::new(move |candidate| {
                if candidate == session_b {
                    let ledger = sabotage_home
                        .join("session-state")
                        .join(candidate.as_uuid().to_string())
                        .join("ledger.v1.jsonl");
                    let _ = std::fs::remove_file(&ledger);
                    std::fs::create_dir(&ledger).unwrap();
                }
                None
            })),
        );
        let coordinator_a = manager.ensure_coordinator(session_a).await.unwrap();
        assert!(manager.ensure_coordinator(session_b).await.is_err());
        assert_eq!(manager.session_ids(), vec![session_a]);
        assert!(
            coordinator_a.shares_authority(&manager.ensure_coordinator(session_a).await.unwrap())
        );

        let live = lingxi_core::host::live_sessions::LiveSessionDir::at_live(home.join("sessions"));
        live.claim_session_id(&session_b.to_string(), std::process::id())
            .expect("failed B initialization must synchronously release B's claim");
        manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn first_boot_import_keeps_totals_only_legacy_separate_from_new_aggregates() {
        let (_directory, coordinator, session_id) = coordinator();
        let writer = coordinator.start().await.unwrap();

        coordinator
            .import_legacy_opening_balance(Some(75))
            .await
            .unwrap();
        let hydration = coordinator.hydrate(session_id).await.unwrap();
        assert_eq!(hydration.state.total_nano_usd, 75);
        assert_eq!(hydration.state.legacy_opening_balance_nano_usd, 75);
        assert_eq!(hydration.state.external_nano_usd, 0);
        assert!(hydration.state.legacy_import_evaluated);
        let replay = coordinator.journal().replay().unwrap();
        let event = decode_session_event(replay.entries.last().unwrap().event.clone()).unwrap();
        let SessionEvent::Cost(event) = event else {
            panic!("legacy import must append a cost event");
        };
        assert_eq!(event.source, CostMutationSource::LegacyOpeningBalance);

        drop(coordinator);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn an_existing_v1_wal_records_evaluation_without_importing_a_legacy_shadow() {
        let (_directory, coordinator, session_id) = coordinator();
        let initial = state(session_id, 1, 30);
        let mutation_id = CostMutationId::new("existing-v1");
        coordinator
            .journal()
            .append_once(
                mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(CostMutationRecord {
                    cost_revision: 1,
                    mutation_id: mutation_id.clone(),
                    source: CostMutationSource::ModelResponse,
                    state: CostStateVector::from(&initial),
                }))
                .unwrap(),
            )
            .unwrap();
        let writer = coordinator.start().await.unwrap();

        coordinator
            .import_legacy_opening_balance(Some(30))
            .await
            .unwrap();
        let hydration = coordinator.hydrate(session_id).await.unwrap();
        assert_eq!(hydration.state.total_nano_usd, 30);
        assert_eq!(hydration.state.legacy_opening_balance_nano_usd, 0);
        assert_eq!(hydration.state.external_nano_usd, 0);
        assert!(hydration.state.legacy_import_evaluated);
        assert_eq!(hydration.state.cost_revision, 2);
        let replay = coordinator.journal().replay().unwrap();
        let event = decode_session_event(replay.entries.last().unwrap().event.clone()).unwrap();
        let SessionEvent::Cost(event) = event else {
            panic!("legacy import evaluation must append a cost event");
        };
        assert_eq!(event.source, CostMutationSource::LegacyImportEvaluated);

        drop(coordinator);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn no_match_is_durable_and_a_later_legacy_shadow_cannot_be_imported() {
        let (_directory, coordinator, session_id) = coordinator();
        let writer = coordinator.start().await.unwrap();

        coordinator
            .import_legacy_opening_balance(None)
            .await
            .unwrap();
        assert!(coordinator
            .import_legacy_opening_balance(Some(99))
            .await
            .is_ok());
        let hydration = coordinator.hydrate(session_id).await.unwrap();
        assert_eq!(hydration.state.total_nano_usd, 0);
        assert_eq!(hydration.state.legacy_opening_balance_nano_usd, 0);
        assert!(hydration.state.legacy_import_evaluated);

        drop(coordinator);
        writer.await.unwrap();
    }
}
