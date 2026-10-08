//! Budget enforcement — pre-call gate + post-call latch.
//!
//! [`BudgetEnforcer`] is consulted before every API call (with an estimated
//! cost) and again after the call returns (with the realized cost). The
//! pre-call gate may emit warnings, ask the host to confirm, or block;
//! the post-call latch sets a one-way "realized exceeded" flag that causes
//! every subsequent pre-call gate to halt regardless of the per-call estimate.

use crate::tracker::CostTracker;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock};

mod attempt_lifecycle;
mod attempts;
pub use attempt_lifecycle::CostBudgetAttempt;
pub(crate) use attempts::AttemptAdmissionError;
mod output;
pub(crate) use attempts::BoundAttemptBudget;
#[cfg(test)]
mod settlement_tests;

/// Basis-points threshold at which `tengu_cost_budget_warning` fires.
///
/// 8000 bps = 80% of the configured session limit. Locked by spec §7 line 735
/// and the M3-05 brief.
pub const BUDGET_WARNING_THRESHOLD_BPS: u32 = 8000;

/// Basis-points threshold at which `tengu_cost_budget_exceeded` fires.
///
/// 10000 bps = 100% of the configured session limit. Locked by spec §7 line 736
/// and the M3-05 brief.
pub const BUDGET_EXCEEDED_THRESHOLD_BPS: u32 = 10000;

/// Configuration controlling [`BudgetEnforcer`] behaviour.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetConfig {
    /// Hard limit on total session cost, in nano-USD. `None` disables.
    pub max_session_nano_usd: Option<u64>,
    /// Hard limit on a single turn's cost, in nano-USD. `None` disables.
    pub max_turn_nano_usd: Option<u64>,
    /// Hard limit on a single turn's total tokens. `None` disables.
    pub max_turn_tokens: Option<u64>,
    /// Fractional thresholds (e.g. `[0.5, 0.8, 0.95]`) at which a one-shot
    /// warning event is emitted as the session approaches its budget.
    pub warning_thresholds: Vec<f64>,
    /// What to do when the budget is exceeded (or projected to be).
    pub on_exceed: BudgetExceedPolicy,
}

/// Policy for handling a budget exceedance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BudgetExceedPolicy {
    /// Stop immediately; no further API calls.
    Halt,
    /// Ask the host (and via the host, the user) whether to continue.
    AskUser,
    /// Emit a warning but allow the call to proceed.
    WarnOnly,
}

/// Cost tracker + atomic latch for realized-exceeded budget enforcement.
pub struct BudgetEnforcer {
    config: BudgetConfig,
    cost_tracker: Arc<CostTracker>,
    /// Per-session warning/latch/hold state. The active enforcer follows the
    /// tracker projection; scoped views stay pinned to their origin session.
    sessions: Arc<BudgetSessionLedger>,
    session_scope: Option<lingxi_core::types::SessionId>,
}

struct BudgetSessionLedger {
    sessions: Mutex<HashMap<lingxi_core::types::SessionId, Arc<BudgetSessionState>>>,
    next_reservation_id: AtomicU64,
    owners: std::sync::Mutex<HashMap<u64, ReservationOwner>>,
    settlements: std::sync::Mutex<HashMap<u64, Arc<SettlementSlot>>>,
    output_publication: Mutex<()>,
    current_outputs:
        std::sync::Mutex<HashMap<lingxi_core::types::SessionId, Arc<output::BudgetOutputAccount>>>,
}

mod retirement;
pub(crate) use retirement::BudgetCacheRetirement;

/// Shared state for an owned settlement.  A receipt can be dropped while its
/// finalizer is waiting for a persistence permit; a later retry must wait for
/// that same owned operation instead of treating the consumed token as an
/// unrelated no-op.
struct SettlementSlot {
    session_id: lingxi_core::types::SessionId,
    started: AtomicBool,
    actual_nano_usd: u64,
    result: std::sync::Mutex<Option<Result<(), lingxi_core::host::BudgetError>>>,
    notify: tokio::sync::Notify,
}

#[derive(Clone)]
struct ReservationOwner {
    session_id: lingxi_core::types::SessionId,
    tracker: Arc<CostTracker>,
    session: Arc<BudgetSessionState>,
}

impl SettlementSlot {
    fn new(session_id: lingxi_core::types::SessionId, actual_nano_usd: u64) -> Self {
        Self {
            session_id,
            started: AtomicBool::new(false),
            actual_nano_usd,
            result: std::sync::Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        }
    }

    async fn wait_result(&self) -> Result<(), lingxi_core::host::BudgetError> {
        loop {
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return result;
            }
            let notified = self.notify.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

/// Owned production settlement transfer. The receipt captures the same
/// session ledger/tracker as the original reservation, so Fusion may disarm
/// its RAII hold before the owned finalizer waits on queue capacity or WAL ack.
struct CostBudgetCommitReceipt {
    slot: Arc<SettlementSlot>,
}

#[async_trait::async_trait]
impl lingxi_core::host::BudgetSettlementReceipt for CostBudgetCommitReceipt {
    async fn finish(self: Box<Self>) -> Result<(), lingxi_core::host::BudgetError> {
        self.slot.wait_result().await
    }
}

struct BudgetSessionState {
    warnings_fired: RwLock<HashSet<u32>>,
    realized_exceeded: AtomicBool,
    /// Active Fusion (and future) holds. Occupancy is
    /// `realized + sum(reservations)`.
    // Lock order: cost state -> book -> owners. No await, I/O or external
    // callback while holding this short accounting critical section.
    reservations: std::sync::Mutex<ReservationBook>,
}

impl BudgetSessionState {
    fn new() -> Self {
        Self {
            warnings_fired: RwLock::new(HashSet::new()),
            realized_exceeded: AtomicBool::new(false),
            reservations: std::sync::Mutex::new(ReservationBook::new()),
        }
    }

    fn lock_reservations(
        &self,
        gate: &crate::CostDurabilityGate,
    ) -> Result<std::sync::MutexGuard<'_, ReservationBook>, lingxi_core::host::BudgetError> {
        self.reservations.lock().map_err(|error| {
            drop(error);
            let reason = "reservation accounting book was poisoned";
            gate.freeze(reason);
            lingxi_core::host::BudgetError::Internal(reason.into())
        })
    }
}

struct ReservationBook {
    active: HashMap<u64, u64>,
    attempt_holds: HashMap<String, attempts::AttemptHold>,
    attempt_runs: HashMap<String, attempts::AttemptRunBudget>,
    attempt_origins: HashMap<String, lingxi_core::types::MessageId>,
    output_scopes: HashMap<lingxi_core::types::MessageId, output::OutputScopeState>,
    next_output_generation: u64,
    output_recovery_loaded: bool,
}

impl ReservationBook {
    fn new() -> Self {
        Self {
            active: HashMap::new(),
            attempt_holds: HashMap::new(),
            attempt_runs: HashMap::new(),
            attempt_origins: HashMap::new(),
            output_scopes: HashMap::new(),
            next_output_generation: 0,
            output_recovery_loaded: false,
        }
    }

    fn held(&self) -> u64 {
        self.active
            .values()
            .copied()
            .chain(self.attempt_holds.values().map(|hold| hold.nano_usd))
            .fold(0_u64, u64::saturating_add)
    }

    fn checked_held(&self) -> Option<u64> {
        self.active
            .values()
            .copied()
            .chain(self.attempt_holds.values().map(|hold| hold.nano_usd))
            .try_fold(0_u64, u64::checked_add)
    }
}

fn reservation_id_seed() -> u64 {
    let session = lingxi_core::types::SessionId::new();
    let uuid = session.as_uuid();
    let bytes = uuid.as_bytes();
    u64::from_le_bytes(
        bytes[..8]
            .try_into()
            .expect("UUID prefix is exactly eight bytes"),
    )
    .max(1)
}

/// Result of a [`BudgetEnforcer::check_pre_api_call`].
#[derive(Debug, Clone)]
pub enum BudgetCheckResult {
    /// Call is within budget; proceed without surfacing anything.
    Ok,
    /// Crossed a fractional warning threshold (e.g. 50%) for the first time.
    ThresholdWarning {
        /// Threshold percent that fired (e.g. `50`, `80`, `95`).
        pct: u32,
        /// Current cumulative cost in nano-USD.
        current: u64,
        /// Configured maximum session cost in nano-USD.
        limit: u64,
    },
    /// Over-budget but policy is [`BudgetExceedPolicy::WarnOnly`].
    Warn {
        /// Current cumulative cost in nano-USD.
        current: u64,
        /// Configured maximum session cost in nano-USD.
        limit: u64,
    },
    /// Over-budget; policy is [`BudgetExceedPolicy::AskUser`] — host must
    /// prompt the user before continuing.
    AskUser {
        /// Current cumulative cost in nano-USD.
        current: u64,
        /// Configured maximum session cost in nano-USD.
        limit: u64,
    },
    /// Over-budget; policy is [`BudgetExceedPolicy::Halt`] — stop now.
    Halt {
        /// Current cumulative cost in nano-USD.
        current: u64,
        /// Configured maximum session cost in nano-USD.
        limit: u64,
    },
    /// Durable cost state is unavailable or frozen. Paid work must not start.
    Unavailable {
        /// Stable storage/authority diagnostic.
        reason: String,
    },
}

impl BudgetEnforcer {
    /// Borrow the underlying [`CostTracker`] (used by the
    /// `BudgetEnforcerHandle` trait impl in `handle.rs`).
    #[must_use]
    pub fn cost_tracker_arc(&self) -> Arc<CostTracker> {
        self.cost_tracker.clone()
    }

    pub fn model_safety_observer(&self) -> lingxi_core::host::model_safety::ModelSafetyObserver {
        self.cost_tracker.model_safety_observer()
    }

    /// Configured session-wide ceiling, if budget enforcement is enabled.
    #[must_use]
    pub fn max_session_nano_usd(&self) -> Option<u64> {
        self.config.max_session_nano_usd
    }

    /// Construct a new enforcer bound to `cost_tracker`.
    #[must_use]
    pub fn new(config: BudgetConfig, cost_tracker: Arc<CostTracker>) -> Self {
        Self {
            config,
            cost_tracker,
            sessions: Arc::new(BudgetSessionLedger {
                sessions: Mutex::new(HashMap::new()),
                next_reservation_id: AtomicU64::new(reservation_id_seed()),
                owners: std::sync::Mutex::new(HashMap::new()),
                settlements: std::sync::Mutex::new(HashMap::new()),
                output_publication: Mutex::new(()),
                current_outputs: std::sync::Mutex::new(HashMap::new()),
            }),
            session_scope: None,
        }
    }

    /// Create a budget view pinned to one originating session. Its holds,
    /// warning thresholds, and realized-exceeded latch are independent from
    /// the active projection used by the parent session.
    #[must_use]
    pub fn scoped_for_session(&self, session_id: lingxi_core::types::SessionId) -> Arc<Self> {
        Arc::new(Self {
            config: self.config.clone(),
            cost_tracker: self.cost_tracker.scoped(session_id),
            sessions: self.sessions.clone(),
            session_scope: Some(session_id),
        })
    }

    async fn session_id(&self) -> lingxi_core::types::SessionId {
        match self.session_scope {
            Some(session_id) => session_id,
            None => self.cost_tracker.session_id().await,
        }
    }

    async fn session_state_for(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> Arc<BudgetSessionState> {
        let mut sessions = self.sessions.sessions.lock().await;
        sessions
            .entry(session_id)
            .or_insert_with(|| Arc::new(BudgetSessionState::new()))
            .clone()
    }

    /// Capture the budget book and tracker cell for one session before any
    /// subsequent await. The active projection may switch concurrently, but
    /// this operation remains pinned to the id selected at its start.
    async fn session_context(
        &self,
    ) -> (
        Arc<BudgetSessionState>,
        Arc<tokio::sync::RwLock<crate::tracker::CostState>>,
    ) {
        self.session_context_for(self.session_id().await).await
    }

    async fn session_context_for(
        &self,
        session_id: lingxi_core::types::SessionId,
    ) -> (
        Arc<BudgetSessionState>,
        Arc<tokio::sync::RwLock<crate::tracker::CostState>>,
    ) {
        let session = self.session_state_for(session_id).await;
        let tracker = self.cost_tracker.scoped(session_id);
        let state = tracker.selected_state_cell().await;
        (session, state)
    }

    fn owner_for(&self, id: lingxi_core::host::BudgetReservationId) -> Option<ReservationOwner> {
        self.sessions
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id.raw())
            .cloned()
    }

    fn settled_result(
        &self,
        id: lingxi_core::host::BudgetReservationId,
    ) -> Option<Result<(), lingxi_core::host::BudgetError>> {
        self.sessions
            .settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id.raw())
            .and_then(|slot| {
                slot.result
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
            })
    }

    /// Sum of active reservation holds.
    pub async fn active_reservation_nano_usd(&self) -> u64 {
        let session_id = self.session_id().await;
        let session = self.session_state_for(session_id).await;
        let gate = self.cost_tracker.scoped(session_id).durability_gate();
        let held = match session.lock_reservations(&gate) {
            Ok(book) => book.held(),
            // This legacy observation API cannot return an error. Freeze
            // first, then expose inspectable occupancy; no authorization or
            // mutation ever recovers a poisoned accounting book.
            Err(_) => session
                .reservations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .held(),
        };
        held
    }

    /// Hold `nano_usd` so concurrent work cannot spend it.
    ///
    /// # Errors
    ///
    /// [`lingxi_core::host::budget::BudgetError::Exceeded`] when
    /// `realized + held + nano_usd` would pass the session cap.
    pub async fn reserve_nano_usd(
        &self,
        nano_usd: u64,
    ) -> Result<lingxi_core::host::BudgetReservationId, lingxi_core::host::budget::BudgetError>
    {
        use lingxi_core::host::budget::{BudgetError, BudgetReservationId};
        let session_id = self.session_id().await;
        let pinned_tracker = self.cost_tracker.scoped(session_id);
        let _durability_turn = pinned_tracker
            .acquire_durable_preflight()
            .await
            .map_err(|error| BudgetError::Internal(error.to_string()))?;
        let (session, state_cell) = self.session_context_for(session_id).await;
        // Lock the realized state before the reservation book. Commit uses
        // this same order, so cancellation cannot consume a token while an
        // awaited state lock is still pending.
        let state = state_cell.read().await;
        let mut book = session.lock_reservations(&pinned_tracker.durability_gate())?;
        pinned_tracker
            .preflight_durable()
            .map_err(|error| BudgetError::Internal(error.to_string()))?;
        let realized = state.total_nano_usd;
        let held = book
            .checked_held()
            .ok_or_else(|| BudgetError::Internal("money occupancy overflow".into()))?;
        if let Some(max) = self.config.max_session_nano_usd {
            let occupancy = realized
                .checked_add(held)
                .and_then(|current| current.checked_add(nano_usd))
                .ok_or_else(|| BudgetError::Internal("money occupancy overflow".into()))?;
            if occupancy > max {
                return Err(BudgetError::Exceeded {
                    current_nano_usd: realized.saturating_add(held),
                });
            }
        }
        // Reservation ids are process-global, not per-session. This keeps a
        // late token unambiguous even when two sessions both have active
        // zero-hold (uncapped) settlements.
        let id = loop {
            let candidate = self
                .sessions
                .next_reservation_id
                .fetch_add(1, Ordering::Relaxed);
            if candidate != 0
                && !self
                    .sessions
                    .owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&candidate)
            {
                break candidate;
            }
        };
        // An uncapped run still needs a unique settlement token, but it does
        // not occupy a finite-capacity hold.
        let held_amount = self.config.max_session_nano_usd.map_or(0, |_| nano_usd);
        book.active.insert(id, held_amount);
        self.sessions
            .owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id,
                ReservationOwner {
                    session_id,
                    tracker: pinned_tracker,
                    session: session.clone(),
                },
            );
        Ok(BudgetReservationId::from_raw(id))
    }

    /// Drop a hold. Unknown and noop ids are ignored.
    pub async fn release_reservation(&self, id: lingxi_core::host::BudgetReservationId) {
        if id.is_noop() {
            return;
        }
        if self
            .sessions
            .settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id.raw())
            .is_some_and(|slot| slot.started.load(Ordering::Acquire))
        {
            return;
        }
        let Some(owner) = self.owner_for(id) else {
            return;
        };
        let removed = match owner
            .session
            .lock_reservations(&owner.tracker.durability_gate())
        {
            Ok(mut book) => book.active.remove(&id.raw()).is_some(),
            Err(_) => return,
        };
        if removed {
            self.sessions
                .owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id.raw());
        }
    }

    async fn commit_owned(
        sessions: &Arc<BudgetSessionLedger>,
        owner: ReservationOwner,
        id: lingxi_core::host::BudgetReservationId,
        actual_nano_usd: u64,
        durability_turn: Result<
            Option<crate::persistence::CostDurabilityTurn>,
            crate::CostPersistError,
        >,
    ) -> Result<(), lingxi_core::host::BudgetError> {
        // Queue capacity is acquired by the owned finalizer, before either
        // the cost state lock or reservation-book lock. If this fails, receipt
        // ownership still consumes the token, but unaccepted state is not
        // published and the exact error remains in the settlement slot.
        let tracker = owner.tracker;
        let mut durability_turn = match durability_turn {
            Ok(turn) => turn,
            Err(error) => {
                tracker.durability_gate().freeze(error.to_string());
                owner
                    .session
                    .lock_reservations(&tracker.durability_gate())?
                    .active
                    .remove(&id.raw());
                sessions
                    .owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id.raw());
                return Err(lingxi_core::host::BudgetError::Internal(error.to_string()));
            }
        };
        if let Some(turn) = durability_turn.as_mut() {
            turn.wait().await;
        }
        let permit = match tracker.acquire_persist_permit(owner.session_id).await {
            Ok(permit) => permit,
            Err(error) => {
                tracker.durability_gate().freeze(error.to_string());
                owner
                    .session
                    .lock_reservations(&tracker.durability_gate())?
                    .active
                    .remove(&id.raw());
                sessions
                    .owners
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&id.raw());
                return Err(lingxi_core::host::BudgetError::Internal(error.to_string()));
            }
        };
        let state_cell = tracker.selected_state_cell().await;
        let mut state = state_cell.write().await;
        let (snapshot, enqueue) = {
            let mut book = owner
                .session
                .lock_reservations(&tracker.durability_gate())?;
            if !book.active.contains_key(&id.raw()) {
                let message = "accepted reservation token disappeared before settlement";
                tracker.durability_gate().freeze(message);
                return Err(lingxi_core::host::BudgetError::Internal(message.into()));
            }
            let snapshot = match CostTracker::record_external_cost_in_state(&state, actual_nano_usd)
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracker.durability_gate().freeze(error.to_string());
                    book.active.remove(&id.raw());
                    sessions
                        .owners
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&id.raw());
                    return Err(lingxi_core::host::BudgetError::Internal(error.to_string()));
                }
            };
            let enqueue = match (snapshot.as_ref(), permit) {
                (Some(snapshot), Some(permit)) => Some(tracker.enqueue_snapshot_locked_with_id(
                    snapshot,
                    permit,
                    crate::CostMutationSource::FusionAggregate,
                    crate::CostMutationId::new(format!(
                        "fusion-reservation:v1:{}:{}",
                        owner.session_id,
                        id.raw()
                    )),
                )),
                _ => None,
            };
            match &enqueue {
                Some(Ok(_)) | None => {
                    if let Some(snapshot) = &snapshot {
                        *state = snapshot.clone();
                    }
                }
                Some(Err(_)) => {}
            }
            book.active.remove(&id.raw());
            sessions
                .owners
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id.raw());
            (snapshot, enqueue)
        };
        drop(state);

        let result = if let Some(snapshot) = snapshot {
            match enqueue {
                Some(Ok((mutation_id, revision, ack_rx))) => tracker
                    .await_persistence_ack(owner.session_id, mutation_id, revision, ack_rx)
                    .await
                    .map(|_| ())
                    .map_err(|error| lingxi_core::host::BudgetError::Internal(error.to_string())),
                Some(Err(error)) => {
                    tracker.durability_gate().freeze(error.to_string());
                    Err(lingxi_core::host::BudgetError::Internal(error.to_string()))
                }
                None => {
                    // Ephemeral/test trackers retain their legacy snapshot
                    // channel. Durable scopes never reach this branch: a
                    // missing permit is surfaced as `preflight_error` above.
                    tracker.persist_snapshot(snapshot).await;
                    Ok(())
                }
            }
        } else {
            Ok(())
        };
        if let Some(turn) = durability_turn {
            turn.finish();
        }
        result
    }

    /// Transfer ownership of a known Fusion settlement before waiting for a
    /// durable queue permit.  `None` is retained for legacy/ephemeral callers.
    pub fn begin_commit_reservation(
        &self,
        id: lingxi_core::host::BudgetReservationId,
        actual_nano_usd: u64,
    ) -> Result<Option<lingxi_core::host::BudgetCommitReceipt>, lingxi_core::host::BudgetError>
    {
        if id.is_noop() {
            return Ok(None);
        }
        let existing = self
            .sessions
            .settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id.raw())
            .cloned();
        if let Some(slot) = existing {
            if slot.actual_nano_usd != actual_nano_usd {
                return Err(lingxi_core::host::BudgetError::Internal(
                    "reservation retry changed the realized amount".into(),
                ));
            }
            return Ok(Some(Box::new(CostBudgetCommitReceipt { slot })));
        }
        let Some(owner) = self.owner_for(id) else {
            return Ok(None);
        };
        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            lingxi_core::host::BudgetError::Internal(
                "budget settlement requires an async runtime".into(),
            )
        })?;
        let (slot, durability_turn) = {
            let mut settlements = self
                .sessions
                .settlements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(slot) = settlements.get(&id.raw()) {
                if slot.actual_nano_usd != actual_nano_usd {
                    return Err(lingxi_core::host::BudgetError::Internal(
                        "reservation retry changed the realized amount".into(),
                    ));
                }
                return Ok(Some(Box::new(CostBudgetCommitReceipt {
                    slot: slot.clone(),
                })));
            }
            let durability_turn = owner.tracker.register_durable_mutation();
            let slot = Arc::new(SettlementSlot::new(owner.session_id, actual_nano_usd));
            slot.started.store(true, Ordering::Release);
            settlements.insert(id.raw(), slot.clone());
            (slot, durability_turn)
        };
        let sessions = self.sessions.clone();
        let worker_slot = slot.clone();
        let panic_gate = owner.tracker.durability_gate();
        handle.spawn(async move {
            let worker = tokio::spawn(async move {
                Self::commit_owned(&sessions, owner, id, actual_nano_usd, durability_turn).await
            });
            let result = match worker.await {
                Ok(result) => result,
                Err(error) => {
                    let message = format!("budget settlement worker failed: {error}");
                    panic_gate.freeze(message.clone());
                    Err(lingxi_core::host::BudgetError::Internal(message))
                }
            };
            *worker_slot
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
            worker_slot.notify.notify_waiters();
        });
        Ok(Some(Box::new(CostBudgetCommitReceipt { slot })))
    }

    /// Release the hold after work completed AND record `actual_nano_usd`
    /// onto the cost tracker.
    ///
    /// Every other API-call path (`record_api_response_v2`, driven from the
    /// main turn loop's response handling) already charges the tracker
    /// itself, which is why the ordinary per-call budget check
    /// (`check_and_charge` / `check_pre_api_call`) never double-adds here.
    /// Fusion is the one caller of this reservation seam, and its panel /
    /// analyst / synthesizer calls run through `ProviderApiAdapter` and
    /// `ProviderSideQueryClient`, neither of which ever calls
    /// `record_api_response_v2` — so `actual_nano_usd` (priced by the fusion
    /// crate's own `FusionPriceBook` from usage the tracker never saw) is the
    /// ONLY place that spend reaches the session total. Without this the
    /// hold simply vanished on commit and a capped session could spend an
    /// unbounded amount on Fusion beyond its `--max-budget`.
    ///
    /// # Errors
    ///
    /// Never — unknown ids succeed so commit is idempotent.
    pub async fn commit_reservation(
        &self,
        id: lingxi_core::host::BudgetReservationId,
        actual_nano_usd: u64,
    ) -> Result<(), lingxi_core::host::budget::BudgetError> {
        if id.is_noop() {
            return Ok(());
        }
        if let Some(receipt) = self.begin_commit_reservation(id, actual_nano_usd)? {
            return receipt.finish().await;
        }
        self.settled_result(id).unwrap_or(Ok(()))
    }

    /// Pre-API call gate. After the call returns, call
    /// [`Self::check_post_api_call`] to latch the realized-exceeded flag if
    /// the actual cost overran.
    pub async fn check_pre_api_call(&self, estimated_cost_nano_usd: u64) -> BudgetCheckResult {
        let session_id = self.session_id().await;
        let tracker = self.cost_tracker.scoped(session_id);
        let _durability_turn = match tracker.acquire_durable_preflight().await {
            Ok(turn) => turn,
            Err(error) => {
                return BudgetCheckResult::Unavailable {
                    reason: error.to_string(),
                };
            }
        };
        let (session, state_cell) = self.session_context_for(session_id).await;
        if let Err(error) = tracker.preflight_durable() {
            return BudgetCheckResult::Unavailable {
                reason: error.to_string(),
            };
        }
        if session.realized_exceeded.load(Ordering::Acquire) {
            let current = state_cell.read().await.total_nano_usd;
            let limit = self.config.max_session_nano_usd.unwrap_or(0);
            return BudgetCheckResult::Halt { current, limit };
        }
        let state = state_cell.read().await;
        let held = match session.lock_reservations(&tracker.durability_gate()) {
            Ok(book) => book.held(),
            Err(error) => {
                return BudgetCheckResult::Unavailable {
                    reason: error.to_string(),
                };
            }
        };
        let realized = state.total_nano_usd;
        // `check_and_charge(0)` (Agent / subagent turn gate) means "already
        // over", which is realized spend — a Fusion hold is future capacity
        // and must not freeze the reserved child itself. Positive estimates
        // and occupancy warnings do see reservations.
        let current = if estimated_cost_nano_usd == 0 {
            realized
        } else {
            realized.saturating_add(held)
        };
        let after = if estimated_cost_nano_usd == 0 {
            realized
        } else {
            realized
                .saturating_add(held)
                .saturating_add(estimated_cost_nano_usd)
        };
        if let Some(max) = self.config.max_session_nano_usd {
            if after >= max {
                return match self.config.on_exceed {
                    BudgetExceedPolicy::Halt => BudgetCheckResult::Halt {
                        current,
                        limit: max,
                    },
                    BudgetExceedPolicy::AskUser => BudgetCheckResult::AskUser {
                        current,
                        limit: max,
                    },
                    BudgetExceedPolicy::WarnOnly => BudgetCheckResult::Warn {
                        current,
                        limit: max,
                    },
                };
            }
            // Threshold warning — occupancy includes holds so a large Fusion
            // reservation can warn before the first panel token is billed.
            let occupancy = realized
                .saturating_add(held)
                .saturating_add(estimated_cost_nano_usd);
            // Threshold warning — cast to f64 only for the ratio comparison.
            #[allow(clippy::cast_precision_loss)]
            let ratio = occupancy as f64 / max as f64;
            for &threshold in &self.config.warning_thresholds {
                if ratio >= threshold {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    let pct = (threshold * 100.0) as u32;
                    let mut fired = session.warnings_fired.write().await;
                    if fired.insert(pct) {
                        return BudgetCheckResult::ThresholdWarning {
                            pct,
                            current,
                            limit: max,
                        };
                    }
                }
            }
        }
        BudgetCheckResult::Ok
    }

    /// Latch the realized-exceeded flag if cumulative cost now exceeds the
    /// configured session limit. Subsequent [`Self::check_pre_api_call`]
    /// returns [`BudgetCheckResult::Halt`].
    pub async fn check_post_api_call(&self, _realized_cost: u64) {
        let (session, state_cell) = self.session_context().await;
        let total = state_cell.read().await.total_nano_usd;
        if let Some(max) = self.config.max_session_nano_usd {
            if total >= max {
                session.realized_exceeded.store(true, Ordering::Release);
            }
        }
    }

    /// M3-05 entry point: latch the realized-exceeded flag AND emit budget
    /// alarm events if thresholds were crossed.
    ///
    /// - Fires `tengu_cost_budget_warning` at 80% (basis-points threshold
    ///   `BUDGET_WARNING_THRESHOLD_BPS`); guarded by `warnings_fired` so each
    ///   threshold fires at most once per session.
    /// - Fires `tengu_cost_budget_exceeded` at 100% (basis-points threshold
    ///   `BUDGET_EXCEEDED_THRESHOLD_BPS`); guarded by the existing
    ///   `realized_exceeded` `AtomicBool` so it fires at most once per session.
    /// - Without a bus (`None`), behaves identically to
    ///   [`Self::check_post_api_call`] (latches the flag, emits nothing).
    ///
    /// Existing M1/M2 callers should keep using [`Self::check_post_api_call`];
    /// new M3-05 callers (e.g. api-client integration in Task 7) pass
    /// `Some(&bus)`.
    pub async fn check_post_api_call_with_bus(
        &self,
        _realized_cost: u64,
        bus: Option<&Arc<telemetry::AnalyticsBus>>,
    ) {
        let (session, state_cell) = self.session_context().await;
        let total = state_cell.read().await.total_nano_usd;
        let Some(max) = self.config.max_session_nano_usd else {
            // No limit configured — nothing to alarm on.
            return;
        };

        // ----- compute percent in basis points (no f64 in the threshold path) -----
        // Use u128 to avoid intermediate overflow: `total * 10_000` could exceed
        // u64 when total is near u64::MAX. Saturate on the way back down.
        let percent_bps_u128: u128 =
            (u128::from(total)).saturating_mul(10_000) / (u128::from(max).max(1));
        #[allow(clippy::cast_possible_truncation)]
        let percent_bps: u32 = if percent_bps_u128 > u128::from(u32::MAX) {
            u32::MAX
        } else {
            percent_bps_u128 as u32
        };

        // ----- 100% exceeded -----
        if percent_bps >= BUDGET_EXCEEDED_THRESHOLD_BPS {
            // Atomic-latch on realized_exceeded ensures idempotency.
            if !session.realized_exceeded.swap(true, Ordering::AcqRel) {
                if let Some(bus) = bus {
                    emit_budget_exceeded(bus, max, total).await;
                }
            }
        } else if percent_bps >= BUDGET_WARNING_THRESHOLD_BPS {
            // ----- 80% warning (fires once per session) -----
            // Reuse warnings_fired with a synthetic pct value of 80 so the same
            // dedupe set guards both M1 thresholds and the M3-05 BPS warning.
            let mut fired = session.warnings_fired.write().await;
            if fired.insert(80) {
                if let Some(bus) = bus {
                    emit_budget_warning(bus, max, total, percent_bps).await;
                }
            }
        }
    }
}

/// Emit `tengu_cost_budget_warning` with the 3-key spec-locked payload.
///
/// `percent_bps` is computed as `u32` here (basis points, max `10_000` in
/// practice) but M3-06's `BudgetWarningPayload::percent_bps: u64` is the
/// authoritative schema type. The `AnalyticsValue::Int(_ as i64)` cast at the
/// bus boundary is the documented payload-encoding convention shared by all
/// `tengu_*` numeric fields (basis points are always non-negative and well
/// below `i64::MAX`, so the cast is exact and round-trips losslessly back to
/// the `u64` schema field at the `StatsigSink` wire-encode site).
async fn emit_budget_warning(
    bus: &Arc<telemetry::AnalyticsBus>,
    limit_nano_usd: u64,
    current_nano_usd: u64,
    percent_bps: u32,
) {
    use telemetry::{AnalyticsValue, LogEventMetadata};
    let mut m = LogEventMetadata::new();
    m.insert(
        "limit_usd".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(limit_nano_usd)),
    );
    m.insert(
        "current_usd".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(current_nano_usd)),
    );
    m.insert(
        "percent_bps".into(),
        // i64::from(u32) is infallible; the bus-side i64 stays non-negative
        // and re-decodes to BudgetWarningPayload::percent_bps: u64 cleanly.
        AnalyticsValue::Int(i64::from(percent_bps)),
    );
    bus.log_event("tengu_cost_budget_warning", m).await;
}

/// Emit `tengu_cost_budget_exceeded` with the 2-key spec-locked payload.
async fn emit_budget_exceeded(
    bus: &Arc<telemetry::AnalyticsBus>,
    limit_nano_usd: u64,
    current_nano_usd: u64,
) {
    use telemetry::{AnalyticsValue, LogEventMetadata};
    let mut m = LogEventMetadata::new();
    m.insert(
        "limit_usd".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(limit_nano_usd)),
    );
    m.insert(
        "current_usd".into(),
        AnalyticsValue::Int(i64_from_u64_saturating(current_nano_usd)),
    );
    bus.log_event("tengu_cost_budget_exceeded", m).await;
}

#[inline]
#[allow(clippy::cast_possible_wrap)]
const fn i64_from_u64_saturating(v: u64) -> i64 {
    if v > i64::MAX as u64 {
        i64::MAX
    } else {
        v as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{CostError, PricingCatalog, ProviderId, nano_usd_to_dollars_format};
    use crate::usage::{TokenUsage, Usage};
    use crate::{
        CostDurabilityGate, CostHydration, CostPersistError, CostPersistPermit, CostPersistRequest,
        CostPersistence, CostState, ModelRef,
    };
    use async_trait::async_trait;
    use lingxi_core::host::live_sessions::{SessionWriterLease, SharedSessionWriterLease};
    use lingxi_core::types::SessionId;
    use std::sync::Mutex;
    use std::time::Duration;
    use telemetry::{AnalyticsBus, AnalyticsSink, AnalyticsValue, LogEventMetadata};
    use tokio::sync::mpsc;

    fn make_tracker() -> Arc<CostTracker> {
        let (tx, _rx) = mpsc::channel(8);
        Arc::new(CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ))
    }

    struct TestLease(String);

    impl SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    struct RequestPersistence {
        requests: tokio::sync::mpsc::UnboundedSender<CostPersistRequest>,
    }

    #[async_trait]
    impl CostPersistence for RequestPersistence {
        async fn acquire_permit(
            &self,
            _session_id: SessionId,
        ) -> Result<CostPersistPermit, CostPersistError> {
            let requests = self.requests.clone();
            Ok(CostPersistPermit::new(move |request| {
                requests
                    .send(request)
                    .map_err(|_| CostPersistError::Rejected("test receiver dropped".into()))
            }))
        }
    }

    fn make_durable_tracker(
        session_id: SessionId,
        requests: tokio::sync::mpsc::UnboundedSender<CostPersistRequest>,
        gate: CostDurabilityGate,
    ) -> Arc<CostTracker> {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                CostHydration {
                    state: CostState {
                        session_id,
                        ..Default::default()
                    },
                    journal_revision: 0,
                    attempt_outputs: Vec::new(),
                },
                Arc::new(RequestPersistence { requests }),
                lease,
                gate,
            )
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn poisoned_reservation_book_freezes_without_releasing_or_charging_a_hold() {
        let tracker = make_tracker();
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        );
        let hold = enforcer.reserve_nano_usd(400).await.unwrap();
        let session = enforcer.session_state_for(tracker.session_id().await).await;
        assert!(
            std::thread::spawn(move || {
                let _book = session.reservations.lock().unwrap();
                panic!("test-only accounting critical-section panic");
            })
            .join()
            .is_err()
        );
        assert_eq!(enforcer.active_reservation_nano_usd().await, 400);
        assert!(tracker.durability_gate().frozen_reason().is_some());
        assert!(enforcer.reserve_nano_usd(1).await.is_err());
        assert!(matches!(
            enforcer.check_pre_api_call(1).await,
            BudgetCheckResult::Unavailable { .. }
        ));
        enforcer.release_reservation(hold).await;
        assert!(enforcer.commit_reservation(hold, 100).await.is_err());
        assert_eq!(enforcer.active_reservation_nano_usd().await, 400);
        assert_eq!(
            tracker
                .selected_state_cell()
                .await
                .read()
                .await
                .total_nano_usd,
            0
        );
    }

    #[tokio::test]
    async fn under_budget_returns_ok() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000_000_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![0.5, 0.8, 0.95],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, make_tracker());
        assert!(matches!(
            e.check_pre_api_call(10_000_000).await,
            BudgetCheckResult::Ok
        ));
    }

    #[tokio::test]
    async fn over_budget_halts_with_halt_policy() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, make_tracker());
        assert!(matches!(
            e.check_pre_api_call(10_000).await,
            BudgetCheckResult::Halt { .. }
        ));
    }

    #[tokio::test]
    async fn reaching_budget_exactly_halts() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, make_tracker());
        assert!(matches!(
            e.check_pre_api_call(1_000).await,
            BudgetCheckResult::Halt {
                current: 0,
                limit: 1_000
            }
        ));
    }

    // -----------------------------------------------------------------
    // M3-05 Task 4: budget alarm emission at 80% / 100% thresholds.
    // -----------------------------------------------------------------

    #[derive(Default)]
    struct CaptureSink {
        events: Mutex<Vec<(String, LogEventMetadata)>>,
    }

    #[async_trait]
    impl AnalyticsSink for CaptureSink {
        async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
            self.events.lock().unwrap().push((name.into(), metadata));
        }
        async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
            self.events.lock().unwrap().push((name.into(), metadata));
        }
        fn name(&self) -> &str {
            "capture"
        }
    }

    async fn make_setup(
        limit_nano_usd: u64,
    ) -> (
        Arc<CostTracker>,
        BudgetEnforcer,
        Arc<AnalyticsBus>,
        Arc<CaptureSink>,
    ) {
        let (tx, _rx) = mpsc::channel(8);
        let tracker = Arc::new(CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ));
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(limit_nano_usd),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![], // M3-05 uses BPS thresholds, not the M1 f64 list
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let enforcer = BudgetEnforcer::new(cfg, tracker.clone());
        let bus = Arc::new(AnalyticsBus::new());
        let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
        bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
            .await;
        (tracker, enforcer, bus, sink)
    }

    #[test]
    fn alarm_threshold_constants_match_spec() {
        assert_eq!(
            BUDGET_WARNING_THRESHOLD_BPS, 8000_u32,
            "80% in basis points"
        );
        assert_eq!(
            BUDGET_EXCEEDED_THRESHOLD_BPS, 10000_u32,
            "100% in basis points"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alarm_warning_fires_at_80_percent_with_locked_payload() {
        let (tracker, enforcer, bus, sink) = make_setup(1_000_000_000).await; // $1.00 limit

        // 160_000 input tokens * 5000 nano-USD/tok = 800_000_000 nano-USD = 80%.
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        tracker
            .record_api_response_v2(
                mr,
                Usage {
                    tokens: TokenUsage {
                        input: 160_000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
                0,
                0,
                false,
                None,
            )
            .await;

        enforcer.check_post_api_call_with_bus(0, Some(&bus)).await;

        let events = sink.events.lock().unwrap();
        let warnings: Vec<_> = events
            .iter()
            .filter(|(n, _)| n == "tengu_cost_budget_warning")
            .collect();
        assert_eq!(warnings.len(), 1, "exactly one warning fires at 80%");

        let payload = &warnings[0].1;
        assert_eq!(payload.len(), 3, "warning payload has exactly 3 keys");
        assert!(payload.contains_key("limit_usd"));
        assert!(payload.contains_key("current_usd"));
        assert!(payload.contains_key("percent_bps"));

        match &payload["limit_usd"] {
            AnalyticsValue::Int(n) => assert_eq!(*n, 1_000_000_000),
            other => panic!("limit_usd must be Int, got {other:?}"),
        }
        match &payload["current_usd"] {
            AnalyticsValue::Int(n) => assert_eq!(*n, 800_000_000),
            other => panic!("current_usd must be Int, got {other:?}"),
        }
        match &payload["percent_bps"] {
            AnalyticsValue::Int(n) => assert_eq!(*n, 8000, "exactly 80% in basis points"),
            other => panic!("percent_bps must be Int, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alarm_warning_does_not_double_emit_within_session() {
        let (tracker, enforcer, bus, sink) = make_setup(1_000_000_000).await;

        // Trip to 80% twice (each post-call check is a separate invocation).
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        for _ in 0..2 {
            tracker
                .record_api_response_v2(
                    mr.clone(),
                    Usage {
                        tokens: TokenUsage {
                            input: 80_000,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    Duration::from_millis(10),
                    0,
                    0,
                    0,
                    false,
                    None,
                )
                .await;
            enforcer.check_post_api_call_with_bus(0, Some(&bus)).await;
        }

        let events = sink.events.lock().unwrap();
        let warnings = events
            .iter()
            .filter(|(n, _)| n == "tengu_cost_budget_warning")
            .count();
        assert_eq!(warnings, 1, "warning fires exactly once per session");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alarm_exceeded_fires_at_100_percent_with_locked_payload() {
        let (tracker, enforcer, bus, sink) = make_setup(1_000_000_000).await;

        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        // 200_000 input tokens * 5000 = 1_000_000_000 nano-USD = 100%.
        tracker
            .record_api_response_v2(
                mr,
                Usage {
                    tokens: TokenUsage {
                        input: 200_000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
                0,
                0,
                false,
                None,
            )
            .await;
        enforcer.check_post_api_call_with_bus(0, Some(&bus)).await;

        let events = sink.events.lock().unwrap();
        let exceeded: Vec<_> = events
            .iter()
            .filter(|(n, _)| n == "tengu_cost_budget_exceeded")
            .collect();
        assert_eq!(exceeded.len(), 1, "exactly one exceeded fires at 100%");

        let payload = &exceeded[0].1;
        assert_eq!(payload.len(), 2, "exceeded payload has exactly 2 keys");
        assert!(payload.contains_key("limit_usd"));
        assert!(payload.contains_key("current_usd"));

        match &payload["limit_usd"] {
            AnalyticsValue::Int(n) => assert_eq!(*n, 1_000_000_000),
            other => panic!("limit_usd must be Int, got {other:?}"),
        }
        match &payload["current_usd"] {
            AnalyticsValue::Int(n) => assert_eq!(*n, 1_000_000_000),
            other => panic!("current_usd must be Int, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn alarm_exceeded_idempotent_no_double_emit() {
        let (tracker, enforcer, bus, sink) = make_setup(1_000_000_000).await;

        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        // Two over-budget calls back-to-back. Exceeded must still fire exactly once.
        for _ in 0..2 {
            tracker
                .record_api_response_v2(
                    mr.clone(),
                    Usage {
                        tokens: TokenUsage {
                            input: 200_000,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    Duration::from_millis(10),
                    0,
                    0,
                    0,
                    false,
                    None,
                )
                .await;
            enforcer.check_post_api_call_with_bus(0, Some(&bus)).await;
        }

        let events = sink.events.lock().unwrap();
        let exceeded = events
            .iter()
            .filter(|(n, _)| n == "tengu_cost_budget_exceeded")
            .count();
        assert_eq!(
            exceeded, 1,
            "exceeded fires exactly once thanks to atomic latch"
        );
    }

    #[test]
    fn alarm_budget_exceeded_error_string_byte_for_byte() {
        let e = CostError::BudgetExceeded {
            limit: 100.00,
            current: 150.75,
        };
        assert_eq!(
            e.to_string(),
            "Budget exceeded ($150.75); stopped.",
            "claude-code parity: spec §5 line 498",
        );
    }

    #[test]
    fn alarm_nano_usd_to_dollars_format_matches_spec_examples() {
        assert_eq!(nano_usd_to_dollars_format(1_500_000_000), "$1.50");
        assert_eq!(nano_usd_to_dollars_format(12_345_678_901), "$12.35");
        assert_eq!(nano_usd_to_dollars_format(0), "$0.00");
        assert_eq!(
            nano_usd_to_dollars_format(999_999_999),
            "$1.00",
            "rounding edge"
        );
    }

    #[tokio::test]
    async fn reservation_holds_capacity_against_a_second_reserve() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, make_tracker());
        let first = e.reserve_nano_usd(800).await.expect("first hold");
        assert!(!first.is_noop());
        assert_eq!(e.active_reservation_nano_usd().await, 800);
        let err = e.reserve_nano_usd(800).await.unwrap_err();
        assert!(matches!(
            err,
            lingxi_core::host::budget::BudgetError::Exceeded {
                current_nano_usd: 800
            }
        ));
        e.release_reservation(first).await;
        assert_eq!(e.active_reservation_nano_usd().await, 0);
        e.reserve_nano_usd(800).await.expect("hold after release");
    }

    #[tokio::test]
    async fn frozen_durable_session_blocks_checks_and_new_reservations() {
        let session_id = SessionId::new();
        let (requests, _requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = CostDurabilityGate::default();
        let tracker = make_durable_tracker(session_id, requests, gate.clone());
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker,
        );
        gate.freeze("synthetic WAL failure");

        assert!(matches!(
            enforcer.check_pre_api_call(1).await,
            BudgetCheckResult::Unavailable { reason } if reason.contains("synthetic WAL failure")
        ));
        assert!(matches!(
            enforcer.reserve_nano_usd(1).await,
            Err(lingxi_core::host::BudgetError::Internal(reason)) if reason.contains("synthetic WAL failure")
        ));
    }

    #[tokio::test]
    async fn unacked_ordinary_charge_blocks_a_dependent_fusion_reservation() {
        let session_id = SessionId::new();
        let (requests, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = make_durable_tracker(session_id, requests, CostDurabilityGate::default());
        let scope = tracker.session_scope(session_id);
        let response = scope.submit_model_response(crate::CostModelResponse {
            model_ref: ModelRef {
                provider: ProviderId::Anthropic,
                model: "claude-opus-4-6".into(),
            },
            usage: Usage {
                tokens: TokenUsage {
                    input: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
            duration: Duration::from_millis(1),
            retries: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            is_batch_request: false,
            bus: None,
        });
        let request = requests_rx.recv().await.expect("ordinary durable charge");
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000_000_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker,
        );
        let reservation = enforcer.reserve_nano_usd(10);
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            result = &mut reservation => panic!("reservation bypassed an unacked charge: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        request
            .ack
            .send(Ok(crate::CostPersistAck {
                mutation_id: request.mutation_id.clone(),
                journal_revision: 1,
                cost_revision: request.cost_revision,
            }))
            .unwrap();
        let reservation_id = reservation.await.expect("ack releases reservation");
        assert!(response.settle().await.persistence_result().is_ok());
        enforcer.release_reservation(reservation_id).await;
    }

    #[tokio::test]
    async fn failed_ordinary_ack_rejects_the_waiting_fusion_reservation() {
        let session_id = SessionId::new();
        let (requests, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = CostDurabilityGate::default();
        let tracker = make_durable_tracker(session_id, requests, gate.clone());
        let response =
            tracker
                .session_scope(session_id)
                .submit_model_response(crate::CostModelResponse {
                    model_ref: ModelRef {
                        provider: ProviderId::Anthropic,
                        model: "claude-opus-4-6".into(),
                    },
                    usage: Usage {
                        tokens: TokenUsage {
                            input: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    duration: Duration::from_millis(1),
                    retries: 0,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    is_batch_request: false,
                    bus: None,
                });
        let request = requests_rx.recv().await.expect("ordinary durable charge");
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000_000_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker,
        );
        let reservation = enforcer.reserve_nano_usd(10);
        tokio::pin!(reservation);
        tokio::select! {
            biased;
            result = &mut reservation => panic!("reservation bypassed an unacked charge: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        request
            .ack
            .send(Err(CostPersistError::Storage("append failed".into())))
            .unwrap();
        assert!(matches!(
            reservation.await,
            Err(lingxi_core::host::BudgetError::Internal(message)) if message.contains("append failed")
        ));
        assert!(response.settle().await.persistence_result().is_err());
        assert!(gate.frozen_reason().is_some());
        assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn accepted_fusion_handoff_synchronously_blocks_ordinary_preflight_until_ack() {
        let session_id = SessionId::new();
        let (requests, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = make_durable_tracker(session_id, requests, CostDurabilityGate::default());
        let scope = tracker.session_scope(session_id);
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker,
        );
        let reservation_id = enforcer.reserve_nano_usd(800).await.unwrap();
        let receipt = enforcer
            .begin_commit_reservation(reservation_id, 400)
            .unwrap()
            .expect("known Fusion cost transfers to an owned receipt");
        let preflight = scope.preflight();
        tokio::pin!(preflight);
        tokio::select! {
            biased;
            result = &mut preflight => panic!("preflight bypassed synchronous settlement transfer: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        let request = requests_rx.recv().await.expect("Fusion durable charge");
        request
            .ack
            .send(Ok(crate::CostPersistAck {
                mutation_id: request.mutation_id.clone(),
                journal_revision: 1,
                cost_revision: request.cost_revision,
            }))
            .unwrap();
        preflight.await.expect("ack releases ordinary preflight");
        receipt.finish().await.unwrap();
        assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn dropped_commit_receipt_still_owns_and_finishes_the_charge() {
        let tracker = make_tracker();
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        );
        let id = enforcer.reserve_nano_usd(800).await.unwrap();
        let receipt = enforcer
            .begin_commit_reservation(id, 400)
            .unwrap()
            .expect("production cost enforcer returns an owned receipt");
        drop(receipt);

        for _ in 0..100 {
            if tracker.total_nano_usd().await == 400 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(tracker.total_nano_usd().await, 400);
        assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
        enforcer.commit_reservation(id, 400).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 400);
    }

    #[tokio::test]
    async fn settlement_retry_rejects_a_changed_realized_amount() {
        let tracker = make_tracker();
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker,
        );
        let id = enforcer.reserve_nano_usd(800).await.unwrap();
        let receipt = enforcer.begin_commit_reservation(id, 400).unwrap().unwrap();

        assert!(matches!(
            enforcer.begin_commit_reservation(id, 401),
            Err(lingxi_core::host::BudgetError::Internal(message)) if message.contains("changed")
        ));
        receipt.finish().await.unwrap();
    }

    #[tokio::test]
    async fn accepted_wal_failure_is_cached_and_never_releases_or_rebills() {
        let session_id = SessionId::new();
        let (requests, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let gate = CostDurabilityGate::default();
        let tracker = make_durable_tracker(session_id, requests, gate.clone());
        let enforcer = BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(1_000),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        );
        let id = enforcer.reserve_nano_usd(800).await.unwrap();
        let receipt = enforcer.begin_commit_reservation(id, 400).unwrap().unwrap();
        let request = requests_rx.recv().await.unwrap();
        request
            .ack
            .send(Err(CostPersistError::Storage("fsync failed".into())))
            .unwrap();

        let first = receipt.finish().await.unwrap_err().to_string();
        let retry = enforcer
            .commit_reservation(id, 400)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(first, retry);
        assert_eq!(tracker.total_nano_usd().await, 400);
        assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
        assert!(gate.frozen_reason().is_some());
    }

    #[tokio::test]
    async fn reservation_is_visible_to_positive_pre_api_estimates() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, make_tracker());
        let _id = e.reserve_nano_usd(800).await.unwrap();
        // Charge-0 turn gate (subagent) must still run — the hold is future work.
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::Ok
        ));
        // A new unreserved estimate must see the hold.
        assert!(matches!(
            e.check_pre_api_call(300).await,
            BudgetCheckResult::Halt {
                current: 800,
                limit: 1_000
            }
        ));
    }

    #[tokio::test]
    async fn commit_reservation_records_actual_onto_the_tracker() {
        // Fusion never calls `record_api_response_v2` for its panel / analyst /
        // synthesizer spend (see `commit_reservation`'s doc comment) — this
        // reservation seam is the ONLY place that money reaches the session
        // total, so a discarded `actual_nano_usd` would let a capped session
        // spend Fusion's whole bill for free.
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(10_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let tracker = make_tracker();
        let e = BudgetEnforcer::new(cfg, tracker.clone());
        let id = e.reserve_nano_usd(5_000).await.unwrap();
        e.commit_reservation(id, 1_000).await.unwrap();
        assert_eq!(e.active_reservation_nano_usd().await, 0, "hold released");
        assert_eq!(
            tracker.total_nano_usd().await,
            1_000,
            "commit must record the actual realized spend onto the tracker"
        );
        // Settlement ids are consumed exactly once. A retry after the first
        // successful commit must not charge the actual amount again.
        e.commit_reservation(id, 1_000)
            .await
            .expect("id-not-found is not an error");
        assert_eq!(
            tracker.total_nano_usd().await,
            1_000,
            "a second commit call must be an accounting no-op"
        );
    }

    #[tokio::test]
    async fn unlimited_reserve_mints_a_zero_hold_settlement_token() {
        let unlimited = BudgetConfig {
            max_session_nano_usd: None,
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(unlimited, make_tracker());
        let id = e.reserve_nano_usd(9_000_000).await.unwrap();
        assert!(!id.is_noop());
        assert_eq!(e.active_reservation_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn commit_transition_is_atomic_to_concurrent_reserve() {
        let (tx, mut rx) = mpsc::channel(1);
        let tracker = Arc::new(CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ));
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = Arc::new(BudgetEnforcer::new(cfg, tracker.clone()));
        tracker.record_external_cost(1).await;
        let first = e.reserve_nano_usd(800).await.unwrap();

        let committing = e.clone();
        let commit = tokio::spawn(async move {
            committing.commit_reservation(first, 400).await.unwrap();
        });
        while tracker.total_nano_usd().await == 1 {
            tokio::task::yield_now().await;
        }

        // The commit has charged the tracker and released the hold before its
        // bounded persistence send completes. The second reserve sees the
        // post-state (401 + 100), never the transient 401 + 800 + 100.
        let second = e.reserve_nano_usd(100).await.unwrap();
        e.release_reservation(second).await;
        assert_eq!(e.active_reservation_nano_usd().await, 0);

        rx.recv().await.unwrap();
        commit.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_commit_keeps_token_for_a_safe_retry() {
        let tracker = make_tracker();
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = Arc::new(BudgetEnforcer::new(cfg, tracker.clone()));
        let id = e.reserve_nano_usd(800).await.unwrap();
        let state_cell = tracker.selected_state_cell().await;
        let state_guard = state_cell.write().await;

        let committing = e.clone();
        let task = tokio::spawn(async move {
            committing.commit_reservation(id, 400).await.unwrap();
        });
        tokio::task::yield_now().await;
        task.abort();
        let _ = task.await;
        drop(state_guard);

        e.commit_reservation(id, 400).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 400);
        assert_eq!(e.active_reservation_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn uncapped_commit_accounts_actual_once() {
        let tracker = make_tracker();
        let cfg = BudgetConfig {
            max_session_nano_usd: None,
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, tracker.clone());
        let id = e.reserve_nano_usd(0).await.unwrap();
        assert!(!id.is_noop());
        e.commit_reservation(id, 125).await.unwrap();
        e.commit_reservation(id, 125).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 125);
    }

    #[tokio::test]
    async fn scoped_budget_keeps_holds_and_spend_on_origin_session() {
        let (tx, _rx) = mpsc::channel(16);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let tracker = Arc::new(CostTracker::new(
            session_a,
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ));
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let active = BudgetEnforcer::new(cfg, tracker.clone());
        let origin = active.scoped_for_session(session_a);
        let origin_id = origin.reserve_nano_usd(800).await.unwrap();

        tracker.switch_session(session_b).await.unwrap();
        let b_id = active.reserve_nano_usd(800).await.unwrap();
        assert_ne!(origin_id, b_id, "settlement tokens are globally unique");
        active.release_reservation(b_id).await;
        origin.commit_reservation(origin_id, 400).await.unwrap();

        assert_eq!(tracker.total_nano_usd().await, 0);
        tracker.switch_session(session_a).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 400);
        assert_eq!(origin.active_reservation_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn budget_warning_and_latch_are_isolated_per_session() {
        let (tx, _rx) = mpsc::channel(16);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let tracker = Arc::new(CostTracker::new(
            session_a,
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ));
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![0.8],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = BudgetEnforcer::new(cfg, tracker.clone());
        tracker.record_external_cost(800).await;
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::ThresholdWarning { pct: 80, .. }
        ));
        tracker.record_external_cost(200).await;
        e.check_post_api_call(0).await;
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::Halt { .. }
        ));

        tracker.switch_session(session_b).await.unwrap();
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::Ok
        ));
        tracker.record_external_cost(800).await;
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::ThresholdWarning { pct: 80, .. }
        ));

        tracker.switch_session(session_a).await.unwrap();
        assert!(matches!(
            e.check_pre_api_call(0).await,
            BudgetCheckResult::Halt { .. }
        ));
    }

    #[tokio::test]
    async fn two_concurrent_reserves_cannot_both_cross_the_cap() {
        let cfg = BudgetConfig {
            max_session_nano_usd: Some(1_000),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        };
        let e = std::sync::Arc::new(BudgetEnforcer::new(cfg, make_tracker()));
        let a = e.clone();
        let b = e.clone();
        let (ra, rb) = tokio::join!(a.reserve_nano_usd(800), b.reserve_nano_usd(800));
        let wins = u8::from(ra.is_ok()) + u8::from(rb.is_ok());
        assert_eq!(wins, 1, "exactly one of two 800-holds on a 1000 cap");
        assert_eq!(e.active_reservation_nano_usd().await, 800);
    }
}
