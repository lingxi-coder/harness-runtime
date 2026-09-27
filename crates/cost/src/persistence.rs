//! Durable cost-state contracts shared by the cost ledger and app-tier
//! session coordinator.
//!
//! This module deliberately contains no filesystem or engine dependency. The
//! coordinator owns WAL/snapshot IO and implements [`CostPersistence`]; the
//! cost crate owns mutation identity, vector DTO conversion, queue ordering,
//! and the per-session durability/freeze latch.

use crate::tracker::{CostState, ModelUsage};
use crate::ModelRef;
use async_trait::async_trait;
use protocol::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::Arc;
use tokio::sync::oneshot;

/// Stable identity retained across queue retries and durable acknowledgements.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CostMutationId(String);

impl CostMutationId {
    /// Construct an id supplied by the owner of the mutation.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the stable wire value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Why a cost vector changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostMutationSource {
    /// A normal model response.
    ModelResponse,
    /// A Fusion aggregate/legacy settlement retained until PR06.
    FusionAggregate,
    /// The one-time legacy matching decision when no balance is imported.
    LegacyImportEvaluated,
    /// The one-time legacy opening balance import.
    LegacyOpeningBalance,
    /// A reset or line/tool counter mutation.
    Administrative,
}

/// JSON-safe cost state. `per_model_usage` is a vector specifically so
/// structured provider/model identities round-trip without map-key coercion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostStateVector {
    /// Owning session.
    pub session_id: SessionId,
    /// Monotonic cost mutation revision.
    pub cost_revision: u64,
    /// Cumulative total.
    pub total_nano_usd: u64,
    /// Per-model vector.
    pub per_model_usage: Vec<ModelUsage>,
    /// Aggregate timing/counters.
    pub total_api_duration_ms: u64,
    /// Aggregate non-retry timing.
    pub total_api_duration_without_retries_ms: u64,
    /// Aggregate client tool timing.
    pub total_tool_duration_ms: u64,
    /// Models without a pricing row.
    pub unpriced_models: Vec<ModelRef>,
    /// Server-side search count.
    pub total_web_search_requests: u32,
    /// Code-line counters.
    pub total_lines_added: u64,
    /// Code-line counters.
    pub total_lines_removed: u64,
    /// Last usage, when present.
    pub last_usage: Option<crate::Usage>,
    /// Original completion revision of last_usage, not a later correction's
    /// revision. Absent in legacy snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage_revision: Option<u64>,
    /// Last request cache-read count.
    pub last_cache_read_input_tokens: u64,
    /// Last request cache-creation count.
    pub last_cache_creation_input_tokens: u64,
    /// External aggregate spend.
    #[serde(default)]
    pub external_nano_usd: u64,
    /// Authorized-but-unaccounted spend, disclosed beside the realized total.
    #[serde(default)]
    pub unverified_nano_usd: u64,
    /// Pre-V1 totals-only opening balance, kept distinct from new rollups.
    #[serde(default)]
    pub legacy_opening_balance_nano_usd: u64,
    /// Whether legacy matching was evaluated.
    #[serde(default)]
    pub legacy_import_evaluated: bool,
}

/// Canonical payload persisted for one accepted cost mutation. Keeping this
/// DTO in the cost crate lets the app coordinator validate and replay the same
/// typed identity/revision tuple without defining an engine-local serde shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostMutationRecord {
    /// Cost revision repeated outside the full state vector for validation.
    pub cost_revision: u64,
    /// Stable idempotency identity, equal to the journal envelope event id.
    pub mutation_id: CostMutationId,
    /// Mutation classification retained for audit/replay.
    pub source: CostMutationSource,
    /// Complete post-mutation state.
    pub state: CostStateVector,
}

impl From<&CostState> for CostStateVector {
    fn from(state: &CostState) -> Self {
        let mut per_model_usage = state.per_model_usage.values().cloned().collect::<Vec<_>>();
        per_model_usage.sort_by_key(|entry| {
            format!(
                "{}:{}",
                serde_json::to_string(&entry.model_ref.provider).unwrap_or_default(),
                entry.model_ref.model
            )
        });
        let mut unpriced_models = state.unpriced_models.iter().cloned().collect::<Vec<_>>();
        unpriced_models.sort_by_key(|model| {
            format!(
                "{}:{}",
                serde_json::to_string(&model.provider).unwrap_or_default(),
                model.model
            )
        });
        Self {
            session_id: state.session_id,
            cost_revision: state.cost_revision,
            total_nano_usd: state.total_nano_usd,
            per_model_usage,
            total_api_duration_ms: state.total_api_duration_ms,
            total_api_duration_without_retries_ms: state.total_api_duration_without_retries_ms,
            total_tool_duration_ms: state.total_tool_duration_ms,
            unpriced_models,
            total_web_search_requests: state.total_web_search_requests,
            total_lines_added: state.total_lines_added,
            total_lines_removed: state.total_lines_removed,
            last_usage: state.last_usage,
            last_usage_revision: state.last_usage_revision,
            last_cache_read_input_tokens: state.last_cache_read_input_tokens,
            last_cache_creation_input_tokens: state.last_cache_creation_input_tokens,
            external_nano_usd: state.external_nano_usd,
            unverified_nano_usd: state.unverified_nano_usd,
            legacy_opening_balance_nano_usd: state.legacy_opening_balance_nano_usd,
            legacy_import_evaluated: state.legacy_import_evaluated,
        }
    }
}

impl CostStateVector {
    /// Validate and fold the vector back into the in-memory map
    /// representation.  A vector is deliberately not allowed to silently
    /// overwrite duplicate structured keys, and each row must repeat the same
    /// model identity carried by its usage payload.
    pub fn try_into_state(self) -> Result<CostState, CostPersistError> {
        if self.last_usage_revision.is_some_and(|revision| {
            revision == 0 || revision > self.cost_revision || self.last_usage.is_none()
        }) {
            return Err(CostPersistError::Storage(
                "cost vector contains an invalid usage completion revision".into(),
            ));
        }
        let mut per_model_usage = indexmap::IndexMap::new();
        for entry in self.per_model_usage {
            if per_model_usage
                .insert(entry.model_ref.clone(), entry)
                .is_some()
            {
                return Err(CostPersistError::Storage(
                    "cost vector contains a duplicate model identity".into(),
                ));
            }
        }
        Ok(CostState {
            session_id: self.session_id,
            cost_revision: self.cost_revision,
            total_nano_usd: self.total_nano_usd,
            per_model_usage,
            total_api_duration_ms: self.total_api_duration_ms,
            total_api_duration_without_retries_ms: self.total_api_duration_without_retries_ms,
            total_tool_duration_ms: self.total_tool_duration_ms,
            unpriced_models: self.unpriced_models.into_iter().collect(),
            total_web_search_requests: self.total_web_search_requests,
            total_lines_added: self.total_lines_added,
            total_lines_removed: self.total_lines_removed,
            last_usage: self.last_usage,
            last_usage_revision: self.last_usage_revision,
            last_cache_read_input_tokens: self.last_cache_read_input_tokens,
            last_cache_creation_input_tokens: self.last_cache_creation_input_tokens,
            external_nano_usd: self.external_nano_usd,
            unverified_nano_usd: self.unverified_nano_usd,
            legacy_opening_balance_nano_usd: self.legacy_opening_balance_nano_usd,
            legacy_import_evaluated: self.legacy_import_evaluated,
        })
    }

    /// Fold a validated vector into a state.  Production hydration should use
    /// [`Self::try_into_state`] and surface a storage failure; this convenience
    /// method is retained for trusted in-memory callers and tests.
    pub fn into_state(self) -> CostState {
        self.try_into_state()
            .expect("invalid CostStateVector supplied to trusted caller")
    }
}

/// Durable acknowledgment returned by an app coordinator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostPersistAck {
    /// Mutation this acknowledgment settles.
    pub mutation_id: CostMutationId,
    /// Journal revision assigned by the coordinator.
    pub journal_revision: u64,
    /// Cost revision acknowledged by the WAL.
    pub cost_revision: u64,
}

/// Persistence failures are retained by the caller and freeze only the
/// affected session; they never become a silent successful charge.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CostPersistError {
    /// Queue rejected the mutation before it entered the ledger.
    #[error("cost persistence queue rejected mutation: {0}")]
    Rejected(String),
    /// Durable state is frozen after a WAL/recovery failure.
    #[error("cost persistence is frozen: {0}")]
    Frozen(String),
    /// The coordinator could not append or acknowledge the request.
    #[error("cost persistence failed: {0}")]
    Storage(String),
}

/// Result sent to the mutation owner. The mutation id remains stable on every
/// retry, including a failed durable acknowledgment.
pub type CostPersistResult = Result<CostPersistAck, CostPersistError>;

/// Distinct journal operations for registered model attempts. An intent does
/// not advance the cost revision; a receipt folds one checked contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptPersistMutation {
    /// Immutable authorization retained before dispatch.
    Intent(crate::AttemptIntent),
    /// Actual or conservatively incomplete observation.
    Receipt(crate::AttemptReceipt),
}

/// Acknowledgment of an attempt mutation. Original mutation acknowledgments
/// remain stable on retry even when the current cost projection is newer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptPersistAck {
    /// Original journal/cost revision for this mutation identity.
    pub persistence: CostPersistAck,
    /// Current authoritative projection, not an old duplicate's stale vector.
    pub state: CostStateVector,
    /// A receipt's checked contribution; absent for an intent.
    pub receipt: Option<crate::AttemptFoldAck>,
    /// False for an idempotent duplicate. Never reapply its old contribution.
    pub applied: bool,
}

/// Queue request transferred synchronously after budget authorization.
pub struct AttemptPersistRequest {
    /// Originating session, checked independently of the payload's identity.
    pub session_id: SessionId,
    /// Intent or receipt to validate and append.
    pub mutation: AttemptPersistMutation,
    /// Retained completion result. Dropping a receiver does not cancel I/O.
    pub ack: oneshot::Sender<Result<AttemptPersistAck, CostPersistError>>,
}

/// Capacity in the same bounded coordinator queue used by ordinary cost
/// mutations. Acquire before taking monetary/output authorization locks.
pub struct AttemptPersistPermit {
    enqueue: Option<Box<dyn FnOnce(AttemptPersistRequest) -> Result<(), CostPersistError> + Send>>,
}

impl AttemptPersistPermit {
    /// Bind one owned coordinator queue slot.
    #[must_use]
    pub fn new<F>(enqueue: F) -> Self
    where
        F: FnOnce(AttemptPersistRequest) -> Result<(), CostPersistError> + Send + 'static,
    {
        Self {
            enqueue: Some(Box::new(enqueue)),
        }
    }

    /// Consume capacity with a synchronous enqueue, without a cancellation gap.
    pub fn enqueue(mut self, request: AttemptPersistRequest) -> Result<(), CostPersistError> {
        self.enqueue
            .take()
            .expect("attempt permit is consumed once")(request)
    }
}

/// Request transferred to the app-owned WAL coordinator.
pub struct CostPersistRequest {
    /// Canonical owning session.
    pub session_id: SessionId,
    /// Monotonic cost revision.
    pub cost_revision: u64,
    /// Stable mutation identity.
    pub mutation_id: CostMutationId,
    /// Complete vector snapshot after this mutation.
    pub state: CostStateVector,
    /// Mutation source for replay/audit.
    pub source: CostMutationSource,
    /// One-shot acknowledgment channel. The coordinator must retain the
    /// outcome even if the receiver is dropped, using its mutation table.
    pub ack: oneshot::Sender<CostPersistResult>,
}

/// A bounded queue permit. Capacity is acquired before the caller takes any
/// cost/reservation lock; enqueue itself is synchronous once those locks are
/// held, closing the cancellation gap between mutation and WAL submission.
pub struct CostPersistPermit {
    enqueue: Option<Box<dyn FnOnce(CostPersistRequest) -> Result<(), CostPersistError> + Send>>,
}

impl CostPersistPermit {
    /// Build a permit around a synchronous enqueue closure.
    #[must_use]
    pub fn new<F>(enqueue: F) -> Self
    where
        F: FnOnce(CostPersistRequest) -> Result<(), CostPersistError> + Send + 'static,
    {
        Self {
            enqueue: Some(Box::new(enqueue)),
        }
    }

    /// Enqueue exactly once while the caller still owns its state locks.
    pub fn enqueue(mut self, request: CostPersistRequest) -> Result<(), CostPersistError> {
        self.enqueue
            .take()
            .expect("cost persistence permit can enqueue only once")(request)
    }
}

/// App-tier implementation seam. The cost crate owns the contract; engine
/// desktop supplies the WAL/snapshot coordinator without a dependency cycle.
#[async_trait]
pub trait CostPersistence: Send + Sync {
    /// Reserve bounded queue capacity before state/reservation locks.
    async fn acquire_permit(
        &self,
        session_id: SessionId,
    ) -> Result<CostPersistPermit, CostPersistError>;

    /// Reserve an attempt journal slot. Legacy/mobile hosts remain compatible
    /// but cannot silently authorize unjournaled registered model attempts.
    async fn acquire_attempt_permit(
        &self,
        _session_id: SessionId,
    ) -> Result<AttemptPersistPermit, CostPersistError> {
        Err(CostPersistError::Rejected(
            "registered model-attempt persistence is unavailable".into(),
        ))
    }
}

/// Hydration result installed before a session becomes active.
#[derive(Debug, Clone)]
pub struct CostHydration {
    /// Fully validated state.
    pub state: CostState,
    /// Last authoritative journal revision.
    pub journal_revision: u64,
    /// Original-generation attempt occupancy, derived from validated receipts.
    pub attempt_outputs: Vec<crate::AttemptOutputRecovery>,
}

/// App-tier state loader used during boot and hot session switches.
#[async_trait]
pub trait CostHydrator: Send + Sync {
    /// Load and validate the complete durable state for `session_id`.
    async fn hydrate(&self, session_id: SessionId) -> Result<CostHydration, CostPersistError>;
}

/// Shared per-session durability authority. A WAL/recovery failure freezes
/// paid operations for this session while leaving other sessions healthy;
/// the FIFO turnstile also keeps provisional mutations from authorizing
/// dependent paid work before their durable acknowledgement.
#[derive(Clone)]
pub struct CostDurabilityGate {
    inner: Arc<CostDurabilityGateInner>,
}

struct CostDurabilityGateInner {
    retention: platform_api::SessionRetentionGate,
    state: std::sync::Mutex<CostDurabilityGateState>,
    changed: tokio::sync::Notify,
}

#[derive(Default)]
struct CostDurabilityGateState {
    frozen_reason: Option<String>,
    /// Bumped by every `freeze`, including one whose reason is discarded
    /// because an earlier one is kept. A validated thaw compares this rather
    /// than the reason, so a failure that arrives while the proof is running
    /// cannot be cleared by it.
    freeze_epoch: u64,
    next_ticket: u64,
    serving_ticket: u64,
    abandoned_tickets: BTreeSet<u64>,
}

/// One FIFO position in a session's durability authority. Mutation turns
/// freeze on unexpected drop; preflight turns simply yield their position.
pub(crate) struct CostDurabilityTurn {
    _retention_pin: Option<platform_api::SessionRetentionPin>,
    inner: Arc<CostDurabilityGateInner>,
    ticket: u64,
    freeze_on_drop: bool,
    released: bool,
}

impl Default for CostDurabilityGate {
    fn default() -> Self {
        Self {
            inner: Arc::new(CostDurabilityGateInner {
                retention: platform_api::SessionRetentionGate::default(),
                state: std::sync::Mutex::new(CostDurabilityGateState::default()),
                changed: tokio::sync::Notify::new(),
            }),
        }
    }
}

impl CostDurabilityGate {
    /// Shared lifetime authority, identical for tracker, output and Desktop
    /// coordinator views. This is not a separate accounting ledger.
    pub fn retention_gate(&self) -> platform_api::SessionRetentionGate {
        self.inner.retention.clone()
    }

    pub(crate) fn is_idle(&self) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.frozen_reason.is_none()
            && state.next_ticket == state.serving_ticket
            && state.abandoned_tickets.is_empty()
    }
    /// Return the current freeze reason, if any.
    pub fn frozen_reason(&self) -> Option<String> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .frozen_reason
            .clone()
    }

    /// Clear a freeze when the caller can prove the ledger is still intact.
    ///
    /// A failed append does not by itself corrupt anything: the in-memory
    /// revision only advances after the write, its fsync and its fingerprint
    /// check all succeed, so the next append reuses the same revision. What
    /// makes continuing unsafe is the one case where bytes reached the file
    /// but the fsync did not -- appending again would then write that revision
    /// twice. Only a full replay can tell those apart, so `validate` performs
    /// it and this method does nothing else on its own.
    ///
    /// Returns whether the gate is now open. A freeze that arrived while
    /// `validate` was running is preserved: the reason is compared, not just
    /// its presence, so a newer failure is never cleared by an older proof.
    pub fn thaw_if_intact(&self, validate: impl FnOnce() -> bool) -> bool {
        let epoch = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.frozen_reason.is_none() {
                return true;
            }
            state.freeze_epoch
        };
        if !validate() {
            return false;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.freeze_epoch != epoch {
            return false;
        }
        state.frozen_reason = None;
        drop(state);
        self.inner.changed.notify_waiters();
        true
    }

    /// Freeze once; later failures preserve the first actionable reason.
    pub fn freeze(&self, reason: impl Into<String>) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.freeze_epoch = state.freeze_epoch.saturating_add(1);
        if state.frozen_reason.is_none() {
            state.frozen_reason = Some(reason.into());
        }
        drop(state);
        self.inner.changed.notify_waiters();
    }

    /// Whether two handles refer to the exact same session authority.
    pub(crate) fn shares_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Reserve a mutation's FIFO position synchronously. Callers use this at
    /// the provider-response/settlement ownership boundary, before spawning a
    /// worker, so a newly dependent preflight cannot overtake known usage.
    pub(crate) fn register_mutation(&self) -> Result<CostDurabilityTurn, CostPersistError> {
        self.reserve_turn(true)
    }

    /// Wait until every earlier mutation is durably acknowledged, then retain
    /// an exclusive turn while the caller evaluates its paid authorization.
    pub(crate) async fn acquire_preflight(&self) -> Result<CostDurabilityTurn, CostPersistError> {
        let mut turn = self.reserve_turn(false)?;
        turn.wait().await;
        if let Some(reason) = self.frozen_reason() {
            turn.finish();
            return Err(CostPersistError::Frozen(reason));
        }
        Ok(turn)
    }

    /// Place a shutdown-only FIFO fence behind every mutation registered
    /// before this call. Unlike a paid-work preflight this deliberately
    /// reserves its position even after the gate is frozen: storage failure
    /// must stop new dispatch, but it must not let teardown abandon earlier
    /// response owners that still hold known usage.
    pub(crate) async fn drain_registered(&self) -> Result<(), CostPersistError> {
        let mut turn = self.reserve_drain_turn()?;
        turn.wait().await;
        let frozen = self.frozen_reason();
        turn.finish();
        frozen.map_or(Ok(()), |reason| Err(CostPersistError::Frozen(reason)))
    }

    fn reserve_turn(&self, freeze_on_drop: bool) -> Result<CostDurabilityTurn, CostPersistError> {
        let retention_pin = self
            .inner
            .retention
            .try_pin()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(reason) = &state.frozen_reason {
            return Err(CostPersistError::Frozen(reason.clone()));
        }
        let ticket = state.next_ticket;
        let Some(next_ticket) = state.next_ticket.checked_add(1) else {
            let message = "cost durability sequence overflow".to_string();
            state.frozen_reason = Some(message.clone());
            drop(state);
            self.inner.changed.notify_waiters();
            return Err(CostPersistError::Storage(message));
        };
        state.next_ticket = next_ticket;
        Ok(CostDurabilityTurn {
            inner: self.inner.clone(),
            ticket,
            freeze_on_drop,
            released: false,
            _retention_pin: Some(retention_pin),
        })
    }

    fn reserve_drain_turn(&self) -> Result<CostDurabilityTurn, CostPersistError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ticket = state.next_ticket;
        let Some(next_ticket) = state.next_ticket.checked_add(1) else {
            let message = "cost durability sequence overflow".to_string();
            if state.frozen_reason.is_none() {
                state.frozen_reason = Some(message.clone());
            }
            drop(state);
            self.inner.changed.notify_waiters();
            return Err(CostPersistError::Storage(message));
        };
        state.next_ticket = next_ticket;
        Ok(CostDurabilityTurn {
            inner: self.inner.clone(),
            ticket,
            freeze_on_drop: false,
            released: false,
            _retention_pin: None,
        })
    }
}

impl CostDurabilityTurn {
    /// Wait for this exact FIFO position. `Notify::enable` closes the gap
    /// between inspecting the synchronous ticket state and awaiting a wakeup.
    pub(crate) async fn wait(&mut self) {
        loop {
            let notified = self.inner.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .serving_ticket
                == self.ticket
            {
                return;
            }
            notified.await;
        }
    }

    /// Release a successfully or explicitly failed turn. Error paths freeze
    /// the gate before calling this; unexpected unwinding freezes in `Drop`.
    pub(crate) fn finish(mut self) {
        self.release(false);
    }

    fn release(&mut self, freeze: bool) {
        if self.released {
            return;
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if freeze && state.frozen_reason.is_none() {
            state.frozen_reason =
                Some("durable cost mutation owner dropped before acknowledgement".to_string());
        }
        if self.ticket == state.serving_ticket {
            state.serving_ticket = state
                .serving_ticket
                .checked_add(1)
                .expect("allocated cost durability ticket can advance");
            loop {
                let serving_ticket = state.serving_ticket;
                if !state.abandoned_tickets.remove(&serving_ticket) {
                    break;
                }
                state.serving_ticket = state
                    .serving_ticket
                    .checked_add(1)
                    .expect("allocated cost durability ticket can advance");
            }
        } else if self.ticket > state.serving_ticket {
            state.abandoned_tickets.insert(self.ticket);
        }
        self.released = true;
        drop(state);
        self.inner.changed.notify_waiters();
    }
}

impl Drop for CostDurabilityTurn {
    fn drop(&mut self) {
        if !self.released {
            self.release(self.freeze_on_drop);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::{ModelRef, ProviderId};

    #[test]
    fn vector_round_trip_preserves_structured_model_keys() {
        let mut state = CostState::default();
        state.session_id = SessionId::new();
        let model = ModelRef {
            provider: ProviderId::OpenAICompatible {
                name: "gateway".into(),
            },
            model: "model-a".into(),
        };
        state.per_model_usage.insert(
            model.clone(),
            ModelUsage {
                model_ref: model.clone(),
                usage: crate::Usage::default(),
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_nano_usd: 7,
            },
        );
        let vector = CostStateVector::from(&state);
        let json = serde_json::to_string(&vector).expect("vector serializes");
        assert!(json.contains("model_ref"));
        assert_eq!(vector.into_state().per_model_usage[&model].cost_nano_usd, 7);
    }

    #[test]
    fn vector_rejects_duplicate_structured_model_keys() {
        let mut state = CostState::default();
        let model = ModelRef {
            provider: ProviderId::Anthropic,
            model: "same".into(),
        };
        state.per_model_usage.insert(
            model.clone(),
            ModelUsage {
                model_ref: model.clone(),
                usage: crate::Usage::default(),
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_nano_usd: 1,
            },
        );
        let mut vector = CostStateVector::from(&state);
        let duplicate = vector.per_model_usage[0].clone();
        vector.per_model_usage.push(duplicate);
        assert!(matches!(
            vector.try_into_state(),
            Err(CostPersistError::Storage(message)) if message.contains("duplicate")
        ));
    }

    #[test]
    fn usage_completion_revision_round_trips_and_rejects_invalid_order() {
        let state = CostState {
            cost_revision: 4,
            last_usage: Some(crate::Usage::default()),
            last_usage_revision: Some(2),
            ..Default::default()
        };
        let vector = CostStateVector::from(&state);
        assert_eq!(vector.clone().try_into_state().unwrap(), state);
        for revision in [0, 5] {
            let mut invalid = vector.clone();
            invalid.last_usage_revision = Some(revision);
            assert!(invalid.try_into_state().is_err());
        }
        let mut missing_usage = vector.clone();
        missing_usage.last_usage = None;
        assert!(missing_usage.try_into_state().is_err());
        let mut legacy = serde_json::to_value(&vector).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("last_usage_revision");
        let restored: CostStateVector = serde_json::from_value(legacy).unwrap();
        assert!(restored.last_usage_revision.is_none());
        assert!(restored.try_into_state().is_ok());
    }

    #[test]
    fn older_v1_vector_without_legacy_attribution_fields_defaults_safely() {
        let session_id = SessionId::new();
        let mut value = serde_json::to_value(CostStateVector::from(&CostState {
            session_id,
            total_nano_usd: 17,
            ..Default::default()
        }))
        .unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("legacy_opening_balance_nano_usd");
        object.remove("legacy_import_evaluated");
        let restored: CostStateVector = serde_json::from_value(value).unwrap();

        assert_eq!(restored.session_id, session_id);
        assert_eq!(restored.total_nano_usd, 17);
        assert_eq!(restored.legacy_opening_balance_nano_usd, 0);
        assert!(!restored.legacy_import_evaluated);
    }

    #[tokio::test]
    async fn dropped_mutation_freezes_before_releasing_a_queued_preflight() {
        let gate = CostDurabilityGate::default();
        let mut mutation = gate.register_mutation().unwrap();
        mutation.wait().await;
        let mut preflight = Box::pin(gate.acquire_preflight());
        tokio::select! {
            biased;
            _ = &mut preflight => panic!("preflight bypassed active mutation"),
            () = tokio::task::yield_now() => {}
        }

        drop(mutation);

        assert!(matches!(
            preflight.await,
            Err(CostPersistError::Frozen(message)) if message.contains("dropped before acknowledgement")
        ));
    }

    #[tokio::test]
    async fn cancelled_queued_preflight_does_not_wedge_the_fifo_turnstile() {
        let gate = CostDurabilityGate::default();
        let mut mutation = gate.register_mutation().unwrap();
        mutation.wait().await;
        let mut cancelled = Box::pin(gate.acquire_preflight());
        tokio::select! {
            biased;
            _ = &mut cancelled => panic!("preflight bypassed active mutation"),
            () = tokio::task::yield_now() => {}
        }
        drop(cancelled);
        mutation.finish();

        let next = gate.acquire_preflight().await.unwrap();
        next.finish();
        assert!(gate.frozen_reason().is_none());
    }

    #[tokio::test]
    async fn shutdown_drain_waits_for_registered_mutation_even_after_freeze() {
        let gate = CostDurabilityGate::default();
        let mut mutation = gate.register_mutation().unwrap();
        mutation.wait().await;
        gate.freeze("first append failed");

        let mut drain = Box::pin(gate.drain_registered());
        tokio::select! {
            biased;
            _ = &mut drain => panic!("shutdown drain bypassed active mutation"),
            () = tokio::task::yield_now() => {}
        }
        mutation.finish();

        assert!(matches!(
            drain.await,
            Err(CostPersistError::Frozen(message)) if message == "first append failed"
        ));
    }
}

#[cfg(test)]
mod thaw_tests {
    use super::CostDurabilityGate;

    #[test]
    fn a_validated_thaw_reopens_the_gate_and_an_unvalidated_one_does_not() {
        let gate = CostDurabilityGate::default();
        assert!(gate.frozen_reason().is_none());
        // Nothing frozen: the gate is already open and validation is not run.
        assert!(gate.thaw_if_intact(|| panic!("must not validate an open gate")));

        gate.freeze("write failed");
        assert!(
            !gate.thaw_if_intact(|| false),
            "an unproven ledger stays shut"
        );
        assert_eq!(gate.frozen_reason().as_deref(), Some("write failed"));

        assert!(gate.thaw_if_intact(|| true));
        assert!(gate.frozen_reason().is_none());
    }

    #[test]
    fn a_freeze_that_arrives_during_validation_is_not_cleared() {
        let gate = CostDurabilityGate::default();
        gate.freeze("first failure");
        let during = gate.clone();
        // The proof concerns the state before this newer failure, so it must
        // not clear it. Comparing the reason, not just its presence, is what
        // makes that hold.
        assert!(!gate.thaw_if_intact(|| {
            during.freeze("second failure");
            true
        }));
        assert_eq!(gate.frozen_reason().as_deref(), Some("first failure"));
    }
}
