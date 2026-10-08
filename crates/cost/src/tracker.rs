//! Cost tracking — single-writer task ensures consistent persisted state.
//!
//! [`CostTracker`] accumulates per-session API usage and money totals as the
//! engine runs. Every mutation snapshots the new state and forwards it to a
//! single-writer `mpsc` channel; a background task drains that channel to
//! disk. Using one channel as the persistence boundary avoids interleaved
//! writes corrupting the on-disk snapshot.

use crate::{
    calculator::CostCalculator,
    persistence::{
        CostDurabilityGate, CostDurabilityTurn, CostHydration, CostHydrator, CostMutationId,
        CostMutationSource, CostPersistAck, CostPersistError, CostPersistRequest, CostPersistence,
    },
    pricing::{PricingCatalog, PricingResolution},
    usage::Usage,
    ModelRef,
};
use indexmap::IndexMap;
use lingxi_core::types::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use telemetry::AnalyticsBus;
use tokio::sync::{mpsc, Mutex, OwnedMutexGuard, RwLock};

mod attempts;
pub(crate) use attempts::AttemptLifecycle;
pub use attempts::{CostAttemptReceipt, CostAttemptSettlement};

/// Persisted snapshot of one session's cumulative cost and usage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostState {
    /// Owning session.
    pub session_id: SessionId,
    /// Monotonic revision assigned to each accepted mutation.
    #[serde(default)]
    pub cost_revision: u64,
    /// Cumulative cost across all models, in nano-USD.
    pub total_nano_usd: u64,
    /// Per-model usage and cost breakdown.
    pub per_model_usage: IndexMap<ModelRef, ModelUsage>,
    /// Total wall-clock spent in API calls (including retries), in ms.
    pub total_api_duration_ms: u64,
    /// Total wall-clock spent in API calls excluding retried attempts, in ms.
    pub total_api_duration_without_retries_ms: u64,
    /// Total wall-clock spent in client-side tool execution, in ms.
    pub total_tool_duration_ms: u64,
    /// Models for which no pricing was found; cost recorded as `0` and the
    /// model is added to this set so the host can surface a warning.
    pub unpriced_models: HashSet<ModelRef>,
    /// Cumulative server-side web search request count.
    pub total_web_search_requests: u32,
    /// Cumulative lines added across all edits this session (claude-code
    /// `Pt.totalLinesAdded`).
    pub total_lines_added: u64,
    /// Cumulative lines removed across all edits this session (claude-code
    /// `Pt.totalLinesRemoved`).
    pub total_lines_removed: u64,
    /// Most recent request usage for status-line `current_usage`.
    #[serde(default)]
    pub last_usage: Option<Usage>,
    /// Cost revision at which the latest usage originally completed. A late
    /// attempt correction must not replace a newer ordinary response's usage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_usage_revision: Option<u64>,
    /// Cache-read tokens associated with [`Self::last_usage`].
    #[serde(default)]
    pub last_cache_read_input_tokens: u64,
    /// Cache-creation tokens associated with [`Self::last_usage`].
    #[serde(default)]
    pub last_cache_creation_input_tokens: u64,
    /// Cumulative externally-priced spend recorded via
    /// [`CostTracker::record_external_cost`] (Fusion panel/analyst/synth
    /// settlement) — a SUBSET already included in [`Self::total_nano_usd`],
    /// tracked separately so a summary can reconcile
    /// `sum(per_model_usage[..].cost_nano_usd) + external_nano_usd ==
    /// total_nano_usd` instead of `total_nano_usd` silently outrunning the
    /// per-model breakdown with no attributable source.
    #[serde(default)]
    pub external_nano_usd: u64,
    /// Authorized-but-unaccounted spend from attempts whose provider usage
    /// report never arrived or came back incomplete. NOT included in
    /// [`Self::total_nano_usd`] — the realized total stays honest — but the
    /// session halt is evaluated against the sum of the two, so an unpriced
    /// or silently-failing run still cannot outrun its ceiling.
    #[serde(default)]
    pub unverified_nano_usd: u64,
    /// One-time `lastCost` opening balance imported from the pre-V1 aggregate
    /// store. This is included in [`Self::total_nano_usd`] but deliberately
    /// kept separate from both new per-model rows and Fusion aggregates.
    #[serde(default)]
    pub legacy_opening_balance_nano_usd: u64,
    /// Durable marker that legacy `lastCost` matching has been evaluated,
    /// including the no-match case. This prevents a later shadow edit from
    /// importing the same opening balance twice.
    #[serde(default)]
    pub legacy_import_evaluated: bool,
}

/// Provider usage transferred to an owned cost finalizer immediately after a
/// response is observed.  Keeping this DTO independent from provider/client
/// crates lets streaming, vision, and compaction paths use the same seam.
#[derive(Clone)]
pub struct CostModelResponse {
    /// Provider model identity.
    pub model_ref: ModelRef,
    /// Exact usage observed from the provider.
    pub usage: Usage,
    /// End-to-end provider duration.
    pub duration: Duration,
    /// Retry count preceding the successful response.
    pub retries: u32,
    /// Prompt-cache read tokens.
    pub cache_read_input_tokens: u64,
    /// Prompt-cache creation tokens.
    pub cache_creation_input_tokens: u64,
    /// Batch request marker retained for compatibility telemetry.
    pub is_batch_request: bool,
    /// Optional shared analytics bus.
    pub bus: Option<Arc<AnalyticsBus>>,
}

/// Captured price policy for one physical model response.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CostResponsePricing {
    /// Price aggregate usage using the captured host catalog.
    #[default]
    Catalog,
    /// Frozen token quote; hosted tool fees still come from the host catalog.
    TokenQuote(u64),
    /// Frozen complete quote, including hosted tools. Do not add other fees.
    CompleteQuote(u64),
    /// The physical response requires a quote that could not be completed.
    /// Preserve usage and mark its model unpriced without inventing a charge.
    Unpriced,
}

/// Whether an SDK usage measurement accompanies the price facts. An observed
/// partial report does not imply every aggregate counter was supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostResponseMeasurement {
    Observed,
    Missing,
}

/// Immutable provider facts transferred to the session accounting owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostResponseObservation {
    /// Provider model identity.
    pub model_ref: ModelRef,
    /// Exact provider usage.
    pub usage: Usage,
    /// Price from a frozen client quote or the captured catalog fallback.
    pub observed_nano_usd: u64,
    /// Pricing policy retained with the observed amount, including unknown
    /// prices so an absent charge cannot be interpreted as a free model.
    pub pricing: CostResponsePricing,
    pub measurement: CostResponseMeasurement,
    /// Provider wall time.
    pub duration: Duration,
    /// Adapter retry count.
    pub retries: u32,
    /// Prompt-cache read tokens.
    pub cache_read_input_tokens: u64,
    /// Prompt-cache creation tokens.
    pub cache_creation_input_tokens: u64,
    /// Batch marker retained for compatibility.
    pub is_batch_request: bool,
}

/// Session-owned observation and its stable settlement result.
#[derive(Debug, Clone)]
pub struct RetainedCostResponse {
    /// Stable id used for durable enqueue/retry.
    pub mutation_id: CostMutationId,
    /// Facts captured synchronously at provider-response handoff.
    pub observation: CostResponseObservation,
    /// `None` while the owned finalizer is pending.
    pub settlement: Option<Result<Option<CostPersistAck>, CostPersistError>>,
}

struct CostResponseSlot {
    mutation_id: CostMutationId,
    observation: CostResponseObservation,
    result: std::sync::Mutex<Option<Result<Option<CostPersistAck>, CostPersistError>>>,
    notify: tokio::sync::Notify,
}

impl CostResponseSlot {
    async fn wait(&self) -> Result<Option<CostPersistAck>, CostPersistError> {
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

    fn complete(&self, result: Result<Option<CostPersistAck>, CostPersistError>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result);
        self.notify.notify_waiters();
    }

    fn retained(&self) -> RetainedCostResponse {
        RetainedCostResponse {
            mutation_id: self.mutation_id.clone(),
            observation: self.observation.clone(),
            settlement: self
                .result
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }
}

/// Exact outcome of one observed provider response. The observed price remains
/// available even when durability fails; callers must not reinterpret a
/// storage failure as a zero-cost response.
#[derive(Debug, Clone)]
pub struct CostResponseSettlement {
    observed_nano_usd: u64,
    persistence: Result<Option<CostPersistAck>, CostPersistError>,
}

impl CostResponseSettlement {
    /// Price calculated from the response, independent of persistence status.
    #[must_use]
    pub fn observed_nano_usd(&self) -> u64 {
        self.observed_nano_usd
    }

    /// Mutation-specific durable result. `Ok(None)` is the explicit ephemeral
    /// compatibility path; durable sessions return `Ok(Some(ack))` or the exact
    /// retained failure for this mutation.
    #[must_use]
    pub fn persistence_result(&self) -> &Result<Option<CostPersistAck>, CostPersistError> {
        &self.persistence
    }

    /// Consume the outcome while preserving the observed price on failure.
    pub fn into_parts(self) -> (u64, Result<Option<CostPersistAck>, CostPersistError>) {
        (self.observed_nano_usd, self.persistence)
    }
}

/// Owned response-settlement handle. Dropping the handle detaches the
/// supervisor; it does not cancel already-observed provider usage.
pub struct CostResponseReceipt {
    slot: Arc<CostResponseSlot>,
}

impl CostResponseReceipt {
    /// Wait for this exact finalizer. A worker panic freezes the captured
    /// session and returns a mutation-specific failure with the known price.
    pub async fn settle(self) -> CostResponseSettlement {
        let persistence = self.slot.wait().await;
        CostResponseSettlement {
            observed_nano_usd: self.slot.observation.observed_nano_usd,
            persistence,
        }
    }

    /// Durable-aware settlement result. A storage/freeze failure is returned
    /// separately from the observed charge; it is never represented as a
    /// successful zero-cost bill.
    pub async fn settle_checked(self) -> Result<u64, crate::persistence::CostPersistError> {
        let outcome = self.settle().await;
        match outcome.persistence {
            Ok(_) => Ok(outcome.observed_nano_usd),
            Err(error) => Err(error),
        }
    }

    /// Whether the finalizer has already completed.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.slot
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    /// Stable mutation identity retained by the owning session.
    #[must_use]
    pub fn mutation_id(&self) -> &CostMutationId {
        &self.slot.mutation_id
    }
}

/// Session-pinned accounting scope passed into provider/streaming paths.
#[derive(Clone)]
pub struct CostSessionScope {
    tracker: Arc<CostTracker>,
}

/// Fully validated session authority awaiting one synchronous activation.
///
/// Preparation may perform storage I/O, but it never changes the tracker's
/// active projection. The token pins the exact hydrated entry and owns the
/// session-switch bookkeeping lock, so consuming it cannot await, reread, or
/// accidentally resolve a replacement authority.
#[must_use = "a prepared cost session has no effect until it is activated"]
pub struct PreparedCostSession {
    retention_pin: lingxi_core::host::SessionRetentionPin,
    ledger: Arc<SessionLedger>,
    entry: Arc<SessionEntry>,
    catalog: Arc<PricingCatalog>,
    persist_tx: mpsc::Sender<CostState>,
    hydration_bookkeeping: OwnedMutexGuard<HashMap<SessionId, u64>>,
    activates_durable_mode: bool,
}

impl PreparedCostSession {
    /// Canonical destination captured by this preparation.
    #[must_use]
    pub fn session_id(&self) -> SessionId {
        self.entry.session_id
    }

    /// Atomically publish the prepared entry and return its pinned provider
    /// accounting scope. All fallible hydration and authority validation has
    /// already completed, so activation is deliberately synchronous.
    #[must_use]
    pub fn activate(self) -> CostSessionScope {
        let Self {
            retention_pin,
            ledger,
            entry,
            catalog,
            persist_tx,
            mut hydration_bookkeeping,
            activates_durable_mode,
        } = self;
        let previous = *ledger
            .active_session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if previous != entry.session_id {
            hydration_bookkeeping.entry(previous).or_insert(0);
        }
        if activates_durable_mode {
            ledger
                .requires_durable
                .store(true, std::sync::atomic::Ordering::Release);
        }
        *ledger
            .active_session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = entry.session_id;
        drop(hydration_bookkeeping);

        CostSessionScope {
            tracker: Arc::new(CostTracker {
                _retention_pin: Some(retention_pin),
                ledger,
                scope: Some(entry),
                catalog,
                persist_tx,
            }),
        }
    }
}

impl std::fmt::Debug for PreparedCostSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedCostSession")
            .field("session_id", &self.entry.session_id)
            .finish_non_exhaustive()
    }
}

impl CostSessionScope {
    /// Pin one tracker view to its canonical originating session.
    #[must_use]
    pub fn new(tracker: Arc<CostTracker>) -> Self {
        let authority = tracker.selected_entry();
        Self {
            tracker: tracker.scoped(authority.session_id),
        }
    }

    /// Check that a production durable scope is hydrated and writable before
    /// dispatching a provider request. Ephemeral `CostTracker::new` scopes
    /// remain available for tests/legacy embedders.
    pub async fn preflight(&self) -> Result<(), crate::persistence::CostPersistError> {
        let _turn = self.tracker.acquire_durable_preflight().await?;
        Ok(())
    }

    /// Submit observed usage synchronously to an owned finalizer. The first
    /// operation after this call is task transfer; queue capacity/state locks
    /// are awaited only by the detached owner.
    pub fn submit_model_response(&self, response: CostModelResponse) -> CostResponseReceipt {
        self.submit_model_response_with_pricing(
            response,
            CostResponsePricing::Catalog,
            CostResponseMeasurement::Observed,
        )
    }

    /// Settle one response using its explicit frozen price policy. Complete
    /// quotes include hosted tools; unpriced responses never fall back to a
    /// different aggregate tariff.
    pub fn submit_model_response_with_pricing(
        &self,
        mut response: CostModelResponse,
        response_pricing: CostResponsePricing,
        measurement: CostResponseMeasurement,
    ) -> CostResponseReceipt {
        if measurement == CostResponseMeasurement::Missing {
            response.usage = Usage::default();
            response.cache_read_input_tokens = 0;
            response.cache_creation_input_tokens = 0;
        }
        let tracker = self.tracker.clone();
        let authority = tracker.selected_entry();
        let durability_turn = tracker.register_durable_mutation_for(&authority);
        let (pricing, _) = tracker.resolve_pricing_with_default(&response.model_ref);
        let quoted_nano_usd = match response_pricing {
            CostResponsePricing::Catalog => None,
            CostResponsePricing::TokenQuote(quoted) => Some(quoted.saturating_add(
                CostCalculator::non_token_nano_usd(&response.usage, &pricing),
            )),
            CostResponsePricing::CompleteQuote(quoted) => Some(quoted),
            CostResponsePricing::Unpriced => Some(0),
        };
        let observed_nano_usd = quoted_nano_usd
            .unwrap_or_else(|| CostCalculator::calculate_nano_usd(&response.usage, &pricing));
        let gate = authority.durability_gate.clone();
        let observation = CostResponseObservation {
            model_ref: response.model_ref.clone(),
            usage: response.usage,
            observed_nano_usd,
            pricing: response_pricing,
            measurement,
            duration: response.duration,
            retries: response.retries,
            cache_read_input_tokens: response.cache_read_input_tokens,
            cache_creation_input_tokens: response.cache_creation_input_tokens,
            is_batch_request: response.is_batch_request,
        };
        let slot = loop {
            let mutation_id = CostMutationId::new(format!(
                "model-response:v1:{}:{}",
                authority.session_id,
                lingxi_core::types::SessionId::new().as_uuid()
            ));
            let mut retained = authority
                .response_settlements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // One hash lookup instead of contains_key + insert. The key is cloned
            // into `entry` so the original can move into the slot it names;
            // `clippy --fix` suggests the reverse order, which does not compile.
            if let std::collections::hash_map::Entry::Vacant(vacant) =
                retained.entry(mutation_id.clone())
            {
                let slot = Arc::new(CostResponseSlot {
                    mutation_id,
                    observation: observation.clone(),
                    result: std::sync::Mutex::new(None),
                    notify: tokio::sync::Notify::new(),
                });
                vacant.insert(slot.clone());
                break slot;
            }
        };
        let worker_slot = slot.clone();
        let mutation_id = slot.mutation_id.clone();
        let durability_turn = match durability_turn {
            Ok(turn) => turn,
            Err(error) => {
                gate.freeze(error.to_string());
                slot.complete(Err(error));
                return CostResponseReceipt { slot };
            }
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            let error = CostPersistError::Rejected(
                "cost response ownership requires an async runtime".into(),
            );
            gate.freeze(error.to_string());
            slot.complete(Err(error));
            return CostResponseReceipt { slot };
        };
        handle.spawn(async move {
            let inner = tokio::spawn(async move {
                tracker
                    .record_api_response_v2_inner_checked(
                        mutation_id,
                        response.model_ref,
                        response.usage,
                        response.duration,
                        response.retries,
                        response.cache_read_input_tokens,
                        response.cache_creation_input_tokens,
                        response.is_batch_request,
                        response.bus,
                        quoted_nano_usd,
                        response_pricing,
                        measurement,
                        durability_turn,
                    )
                    .await
            });
            let persistence = match inner.await {
                Ok(outcome) => outcome.persistence,
                Err(error) => {
                    let error =
                        CostPersistError::Storage(format!("cost finalizer worker failed: {error}"));
                    gate.freeze(error.to_string());
                    Err(error)
                }
            };
            worker_slot.complete(persistence);
        });
        CostResponseReceipt { slot }
    }

    /// Retrieve the session-owned observation/result after a waiter drops.
    #[must_use]
    pub fn retained_response(&self, mutation_id: &CostMutationId) -> Option<RetainedCostResponse> {
        self.tracker
            .selected_entry()
            .response_settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(mutation_id)
            .map(|slot| slot.retained())
    }
}

/// Per-model usage and cost slice of a [`CostState`].
///
/// M3-05 adds `cache_read_input_tokens` and `cache_creation_input_tokens`
/// (declaration position locked AFTER `usage` and BEFORE `cost_nano_usd`).
/// These are Anthropic-specific prompt-caching counters surfaced in the
/// `tengu_api_success` payload; they default to `0` for non-Anthropic
/// providers.
///
/// `#[serde(default)]` on the new fields preserves on-disk compatibility:
/// previously persisted [`CostState`] JSON without these fields deserializes
/// with zeros, so an upgrade does not invalidate existing session files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Which model this slice belongs to.
    pub model_ref: ModelRef,
    /// Cumulative usage counters for this model.
    pub usage: Usage,
    /// Cumulative tokens read from the prompt cache (Anthropic-specific).
    #[serde(default)]
    pub cache_read_input_tokens: u64,
    /// Cumulative tokens written into a fresh cache entry (Anthropic-specific).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// Cumulative cost in nano-USD for this model.
    pub cost_nano_usd: u64,
}

/// Shared in-memory session ledger.
///
/// The process owns one active projection, but background work must retain a
/// stable view of the session that originated it. Keeping the state cells in
/// this map means a clear/resume can move the active projection without
/// destroying a previous session's unfinished or completed settlement.
struct SessionLedger {
    active_session: std::sync::RwLock<SessionId>,
    /// One ref-counted authority per canonical session. State, persistence,
    /// freeze latch, and writer lease move together so a scoped view cannot
    /// accidentally pin A's authority while reading B's state.
    entries: std::sync::Mutex<HashMap<SessionId, Arc<SessionEntry>>>,
    /// Once production durability is installed, unknown scoped sessions are
    /// represented as missing-authority entries and fail closed rather than
    /// lazily falling back to the legacy snapshot channel.
    requires_durable: std::sync::atomic::AtomicBool,
    /// Persisted resume baselines already merged into each session. The
    /// marker is separate from the state cell because a cell can receive
    /// late in-memory settlement before the resume loader supplies its saved
    /// baseline; that baseline must be added exactly once to the delta.
    hydrated_sessions: Arc<Mutex<HashMap<SessionId, u64>>>,
}

struct SessionEntry {
    session_id: SessionId,
    state: Arc<RwLock<CostState>>,
    persistence: Option<Arc<dyn CostPersistence>>,
    writer_lease: Option<lingxi_core::host::live_sessions::SharedSessionWriterLease>,
    durability_gate: CostDurabilityGate,
    missing_durable_authority: bool,
    response_settlements: std::sync::Mutex<HashMap<CostMutationId, Arc<CostResponseSlot>>>,
    attempt_settlements: std::sync::Mutex<attempts::AttemptRegistry>,
    attempt_outputs: Arc<Vec<crate::AttemptOutputRecovery>>,
}

impl SessionEntry {
    fn ephemeral(session_id: SessionId) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            state: Arc::new(RwLock::new(CostState {
                session_id,
                ..Default::default()
            })),
            persistence: None,
            writer_lease: None,
            durability_gate: CostDurabilityGate::default(),
            missing_durable_authority: false,
            response_settlements: std::sync::Mutex::new(HashMap::new()),
            attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
            attempt_outputs: Arc::new(Vec::new()),
        })
    }

    fn missing(session_id: SessionId) -> Arc<Self> {
        Arc::new(Self {
            session_id,
            state: Arc::new(RwLock::new(CostState {
                session_id,
                ..Default::default()
            })),
            persistence: None,
            writer_lease: None,
            durability_gate: CostDurabilityGate::default(),
            missing_durable_authority: true,
            response_settlements: std::sync::Mutex::new(HashMap::new()),
            attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
            attempt_outputs: Arc::new(Vec::new()),
        })
    }
}

impl SessionLedger {
    fn new(session_id: SessionId, state: CostState) -> Self {
        let mut states = HashMap::new();
        states.insert(
            session_id,
            Arc::new(SessionEntry {
                session_id,
                state: Arc::new(RwLock::new(state)),
                persistence: None,
                writer_lease: None,
                durability_gate: CostDurabilityGate::default(),
                missing_durable_authority: false,
                response_settlements: std::sync::Mutex::new(HashMap::new()),
                attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
                attempt_outputs: Arc::new(Vec::new()),
            }),
        );
        Self {
            active_session: std::sync::RwLock::new(session_id),
            entries: std::sync::Mutex::new(states),
            requires_durable: std::sync::atomic::AtomicBool::new(false),
            hydrated_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn entry_for(&self, session_id: SessionId) -> Arc<SessionEntry> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .entry(session_id)
            .or_insert_with(|| {
                if self
                    .requires_durable
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    SessionEntry::missing(session_id)
                } else {
                    SessionEntry::ephemeral(session_id)
                }
            })
            .clone()
    }

    fn install_entry(&self, entry: Arc<SessionEntry>) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(entry.session_id, entry);
    }

    fn existing_entry(&self, session_id: SessionId) -> Option<Arc<SessionEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&session_id)
            .cloned()
    }

    async fn active_entry(&self) -> Arc<SessionEntry> {
        let session_id = *self
            .active_session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.entry_for(session_id)
    }
}

/// In-memory accumulator + persistence channel for the active session.
///
/// `CostTracker::scoped` creates a fixed-origin view over the same ledger for
/// delayed work such as Fusion. The ordinary tracker follows the active
/// session projection switched by the orchestrator at clear/resume boundaries.
#[derive(Clone)]
pub struct CostTracker {
    _retention_pin: Option<lingxi_core::host::SessionRetentionPin>,
    ledger: Arc<SessionLedger>,
    scope: Option<Arc<SessionEntry>>,
    catalog: Arc<PricingCatalog>,
    persist_tx: mpsc::Sender<CostState>,
}

mod retirement;
pub use retirement::CostSessionRetirement;

impl CostTracker {
    pub(crate) fn shares_ledger(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.ledger, &other.ledger)
    }
    /// Construct a fresh tracker for `session_id`.
    ///
    /// `persist_tx` is a single-writer channel that drains to disk in a
    /// background task. This prevents concurrent writers from corrupting
    /// the persisted file.
    #[must_use]
    pub fn new(
        session_id: SessionId,
        catalog: Arc<PricingCatalog>,
        persist_tx: mpsc::Sender<CostState>,
    ) -> Self {
        let state = CostState {
            session_id,
            ..Default::default()
        };
        Self {
            ledger: Arc::new(SessionLedger::new(session_id, state)),
            _retention_pin: None,
            scope: None,
            catalog,
            persist_tx,
        }
    }

    /// Install the app-owned durable coordinator and shared writer lease.
    /// Existing `new` callers remain ephemeral and keep their snapshot
    /// channel behavior.
    #[must_use]
    pub fn with_durable_persistence(
        self,
        hydration: CostHydration,
        persistence: Arc<dyn CostPersistence>,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: CostDurabilityGate,
    ) -> Self {
        self.try_with_durable_persistence(hydration, persistence, writer_lease, durability_gate)
            .expect("durable cost authority must use a canonical matching session claim")
    }

    /// Fallible durable constructor for production composition roots.  A
    /// malformed/bare claim that does not parse to the tracker session is
    /// rejected before any persistence authority is installed.
    pub fn try_with_durable_persistence(
        self,
        hydration: CostHydration,
        persistence: Arc<dyn CostPersistence>,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: CostDurabilityGate,
    ) -> Result<Self, crate::persistence::CostPersistError> {
        let session_id = *self
            .ledger
            .active_session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if writer_lease.canonical_session_id() != Some(session_id) {
            return Err(crate::persistence::CostPersistError::Rejected(
                "durable cost authority claim does not match canonical session".into(),
            ));
        }
        if hydration.state.session_id != session_id {
            return Err(crate::persistence::CostPersistError::Rejected(
                "durable cost hydration does not match canonical session".into(),
            ));
        }
        self.ledger
            .requires_durable
            .store(true, std::sync::atomic::Ordering::Release);
        self.ledger.install_entry(Arc::new(SessionEntry {
            session_id,
            state: Arc::new(RwLock::new(hydration.state)),
            persistence: Some(persistence),
            writer_lease: Some(writer_lease),
            durability_gate,
            missing_durable_authority: false,
            response_settlements: std::sync::Mutex::new(HashMap::new()),
            attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
            attempt_outputs: Arc::new(hydration.attempt_outputs),
        }));
        self.ledger
            .hydrated_sessions
            .try_lock()
            .expect("durable construction precedes concurrent turns")
            .insert(session_id, 0);
        Ok(self)
    }

    /// Return a view permanently bound to `session_id`.
    ///
    /// Unlike the active tracker, this view never follows a later clear or
    /// resume. Existing and late Fusion settlement therefore remains charged
    /// to the originating session.
    #[must_use]
    pub fn scoped(&self, session_id: SessionId) -> Arc<Self> {
        let mut view = self.scoped_unpinned(session_id);
        let inner = Arc::get_mut(&mut view).expect("fresh private tracker view");
        match inner
            .selected_entry()
            .durability_gate
            .retention_gate()
            .try_pin()
        {
            Ok(pin) => inner._retention_pin = Some(pin),
            Err(_) => inner.scope = Some(SessionEntry::missing(session_id)),
        }
        view
    }

    /// Cache-owned account core only. External users must receive a pinned
    /// tracker/scope view, never this private construction shortcut.
    pub(crate) fn scoped_unpinned(&self, session_id: SessionId) -> Arc<Self> {
        let entry = self.selected_entry_for(session_id).unwrap_or_else(|_| {
            if self
                .ledger
                .requires_durable
                .load(std::sync::atomic::Ordering::Acquire)
            {
                SessionEntry::missing(session_id)
            } else {
                SessionEntry::ephemeral(session_id)
            }
        });
        Arc::new(Self {
            ledger: self.ledger.clone(),
            _retention_pin: None,
            scope: Some(entry),
            catalog: self.catalog.clone(),
            persist_tx: self.persist_tx.clone(),
        })
    }

    /// Build the pinned response-accounting scope used by provider paths.
    #[must_use]
    pub fn session_scope(&self, session_id: SessionId) -> CostSessionScope {
        CostSessionScope::new(self.scoped(session_id))
    }

    /// Shared per-session durability latch used by ordinary and Fusion paid
    /// prechecks.
    #[must_use]
    pub fn durability_gate(&self) -> CostDurabilityGate {
        self.selected_entry().durability_gate.clone()
    }

    /// Drain every response settlement and durability ticket registered before
    /// this call across all live session entries. Composition must stop/join
    /// response producers first. The drain never authorizes paid work and does
    /// not short-circuit on a frozen session: it waits all known owners, then
    /// returns the first retained storage failure.
    pub async fn drain_owned_settlements(&self) -> Result<(), CostPersistError> {
        let entries = self
            .ledger
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for entry in entries {
            let slots = entry
                .response_settlements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            if let Err(error) = entry.durability_gate.drain_registered().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            for slot in slots {
                if let Err(error) = slot.wait().await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            // Accepted begin owners can create receipt slots after the first
            // fence. Wait their lease lifecycles, then fence those receipts.
            // Sample membership and monotonic registration history atomically:
            // settled-slot retirement cannot hide an equally sized new cohort.
            loop {
                let (generation, attempts) = entry
                    .attempt_settlements
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .snapshot();
                for slot in attempts {
                    if let Err(error) = slot.drain().await {
                        if first_error.is_none() {
                            first_error = Some(error);
                        }
                    }
                }
                if let Err(error) = entry.durability_gate.drain_registered().await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                if entry
                    .attempt_settlements
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .generation()
                    == generation
                {
                    break;
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Keep the claim alive for callers that need to prove the scope remains
    /// writable. The concrete lease deliberately exposes no filesystem API.
    #[must_use]
    pub fn writer_lease(
        &self,
    ) -> Option<lingxi_core::host::live_sessions::SharedSessionWriterLease> {
        let entry = self.selected_entry();
        let lease = entry.writer_lease.clone()?;
        let pin = self
            ._retention_pin
            .clone()
            .or_else(|| entry.durability_gate.retention_gate().try_pin().ok())?;
        Some(lingxi_core::host::live_sessions::pin_writer_lease(
            lease, pin,
        ))
    }

    fn scope_or_active(&self) -> SessionId {
        self.scope.as_ref().map_or_else(
            || {
                *self
                    .ledger
                    .active_session
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            },
            |entry| entry.session_id,
        )
    }

    pub(crate) fn preflight_durable(&self) -> Result<(), CostPersistError> {
        let authority = self.selected_entry();
        self.preflight_authority(&authority)
    }

    /// Validate a permanently captured, hydrated durable origin before a
    /// host registers model attempts. Unlike ordinary preflight, this rejects
    /// ephemeral trackers even when durable persistence is globally optional.
    /// It grants no hold or dispatch permission; every attempt rechecks later.
    pub fn validate_attempt_host_binding(
        &self,
        session_id: SessionId,
    ) -> Result<(), CostPersistError> {
        let authority = self.scope.as_ref().ok_or_else(|| {
            CostPersistError::Rejected("attempt host requires a captured session scope".into())
        })?;
        if authority.session_id != session_id || authority.persistence.is_none() {
            return Err(CostPersistError::Rejected(
                "attempt host requires its originating durable session".into(),
            ));
        }
        self.preflight_authority(authority)
    }

    pub(crate) fn recovered_attempt_outputs(&self) -> Arc<Vec<crate::AttemptOutputRecovery>> {
        self.selected_entry().attempt_outputs.clone()
    }

    fn preflight_authority(&self, authority: &SessionEntry) -> Result<(), CostPersistError> {
        self.validate_authority_shape(authority)?;
        if !authority.durability_gate.retention_gate().is_live() {
            return Err(CostPersistError::Rejected(
                "session authority is retiring or retired".into(),
            ));
        }
        if let Some(reason) = authority.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        Ok(())
    }

    fn validate_authority_shape(&self, authority: &SessionEntry) -> Result<(), CostPersistError> {
        if authority.missing_durable_authority {
            return Err(CostPersistError::Rejected(
                "durable session scope has not been hydrated".into(),
            ));
        }
        if self
            .ledger
            .requires_durable
            .load(std::sync::atomic::Ordering::Acquire)
            && authority.persistence.is_none()
        {
            return Err(CostPersistError::Rejected(
                "durable session scope has no persistence authority".into(),
            ));
        }
        Ok(())
    }

    pub(crate) async fn acquire_durable_preflight(
        &self,
    ) -> Result<Option<CostDurabilityTurn>, CostPersistError> {
        let authority = self.selected_entry();
        self.preflight_authority(&authority)?;
        if authority.persistence.is_none() {
            return Ok(None);
        }
        let turn = authority.durability_gate.acquire_preflight().await?;
        if let Err(error) = self.preflight_authority(&authority) {
            turn.finish();
            return Err(error);
        }
        Ok(Some(turn))
    }

    pub(crate) fn register_durable_mutation(
        &self,
    ) -> Result<Option<CostDurabilityTurn>, CostPersistError> {
        let authority = self.selected_entry();
        self.register_durable_mutation_for(&authority)
    }

    fn register_durable_mutation_for(
        &self,
        authority: &SessionEntry,
    ) -> Result<Option<CostDurabilityTurn>, CostPersistError> {
        self.validate_authority_shape(authority)?;
        if authority.persistence.is_none() {
            return Ok(None);
        }
        authority.durability_gate.register_mutation().map(Some)
    }

    async fn enter_durable_mutation(&self) -> Result<Option<CostDurabilityTurn>, CostPersistError> {
        let mut turn = self.register_durable_mutation()?;
        if let Some(turn) = turn.as_mut() {
            turn.wait().await;
        }
        Ok(turn)
    }

    /// Return the authority captured by this tracker view. A scoped tracker
    /// must never re-resolve its session id through the mutable ledger map:
    /// hot-switch hydration may replace that map entry while delayed work for
    /// the old session is still finishing.
    fn selected_entry(&self) -> Arc<SessionEntry> {
        self.scope
            .clone()
            .unwrap_or_else(|| self.ledger.entry_for(self.scope_or_active()))
    }

    fn selected_entry_for(
        &self,
        session_id: SessionId,
    ) -> Result<Arc<SessionEntry>, crate::persistence::CostPersistError> {
        if let Some(entry) = &self.scope {
            if entry.session_id != session_id {
                return Err(crate::persistence::CostPersistError::Rejected(
                    "scoped cost authority session mismatch".into(),
                ));
            }
            return Ok(entry.clone());
        }
        Ok(self.ledger.entry_for(session_id))
    }

    /// Current active session id. Scoped views return their fixed origin id.
    pub async fn session_id(&self) -> SessionId {
        if let Some(entry) = &self.scope {
            entry.session_id
        } else {
            *self
                .ledger
                .active_session
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    async fn selected_state(&self) -> Arc<RwLock<CostState>> {
        if let Some(entry) = &self.scope {
            entry.state.clone()
        } else {
            self.ledger.active_entry().await.state.clone()
        }
    }

    async fn selected_state_with_session(&self) -> (SessionId, Arc<RwLock<CostState>>) {
        let entry = self.selected_entry();
        (entry.session_id, entry.state.clone())
    }

    /// Resolve the state cell for a budget operation. The caller may acquire
    /// its lock before touching a reservation token so cancellation cannot
    /// consume a hold before the actual charge mutation is ready.
    pub(crate) async fn selected_state_cell(&self) -> Arc<RwLock<CostState>> {
        self.selected_state().await
    }

    fn prepared_session(
        &self,
        entry: Arc<SessionEntry>,
        hydration_bookkeeping: OwnedMutexGuard<HashMap<SessionId, u64>>,
        activates_durable_mode: bool,
    ) -> Result<PreparedCostSession, CostPersistError> {
        let retention_pin = entry
            .durability_gate
            .retention_gate()
            .try_pin()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        Ok(PreparedCostSession {
            retention_pin,
            ledger: self.ledger.clone(),
            entry,
            catalog: self.catalog.clone(),
            persist_tx: self.persist_tx.clone(),
            hydration_bookkeeping,
            activates_durable_mode,
        })
    }

    async fn prepare_existing_session(
        &self,
        session_id: SessionId,
    ) -> Result<PreparedCostSession, CostPersistError> {
        if self.scope.is_some() {
            return Err(CostPersistError::Rejected(
                "a scoped cost view cannot prepare a session switch".into(),
            ));
        }
        let bookkeeping = self.ledger.hydrated_sessions.clone().lock_owned().await;
        let entry = {
            let mut entries = self
                .ledger
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(entry) = entries.get(&session_id).cloned() {
                entry
            } else if self
                .ledger
                .requires_durable
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(CostPersistError::Rejected(
                    "durable destination has not been hydrated".into(),
                ));
            } else {
                let entry = SessionEntry::ephemeral(session_id);
                entries.insert(session_id, entry.clone());
                entry
            }
        };
        self.validate_authority_shape(&entry)?;
        let activates_durable_mode = entry.persistence.is_some();
        self.prepared_session(entry, bookkeeping, activates_durable_mode)
    }

    /// Validate an already-known destination without changing the active
    /// projection. The returned token serializes the later synchronous commit
    /// against every other session activation.
    pub async fn prepare_session(
        self: &Arc<Self>,
        session_id: SessionId,
    ) -> Result<PreparedCostSession, CostPersistError> {
        self.prepare_existing_session(session_id).await
    }

    /// Switch the active projection while preserving every session cell.
    pub async fn switch_session(&self, session_id: SessionId) -> Result<(), CostPersistError> {
        let prepared = self.prepare_existing_session(session_id).await?;
        let _scope = prepared.activate();
        Ok(())
    }

    /// Hydrate and validate a complete durable state before publishing a hot
    /// session switch. The active projection is not changed on load failure.
    pub async fn switch_session_hydrated(
        &self,
        session_id: SessionId,
        hydrator: &dyn CostHydrator,
    ) -> Result<(), crate::persistence::CostPersistError> {
        if self.scope.is_some() {
            return Err(crate::persistence::CostPersistError::Rejected(
                "a scoped cost view cannot switch sessions".into(),
            ));
        }
        let hydration = hydrator.hydrate(session_id).await?;
        if hydration.state.session_id != session_id {
            return Err(crate::persistence::CostPersistError::Storage(
                "hydrated cost state belongs to a different session".into(),
            ));
        }
        let mut bookkeeping = self.ledger.hydrated_sessions.clone().lock_owned().await;
        let replacement = {
            let mut entries = self
                .ledger
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let existing = entries.get(&session_id).cloned();
            if self
                .ledger
                .requires_durable
                .load(std::sync::atomic::Ordering::Acquire)
                && existing
                    .as_ref()
                    .is_none_or(|entry| entry.persistence.is_none())
            {
                return Err(crate::persistence::CostPersistError::Rejected(
                    "durable session switch has no hydrated persistence authority".into(),
                ));
            }
            let replacement = Arc::new(SessionEntry {
                session_id,
                state: Arc::new(RwLock::new(hydration.state)),
                persistence: existing
                    .as_ref()
                    .and_then(|entry| entry.persistence.clone()),
                writer_lease: existing
                    .as_ref()
                    .and_then(|entry| entry.writer_lease.clone()),
                durability_gate: existing
                    .as_ref()
                    .map_or_else(CostDurabilityGate::default, |entry| {
                        entry.durability_gate.clone()
                    }),
                missing_durable_authority: false,
                response_settlements: std::sync::Mutex::new(HashMap::new()),
                attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
                attempt_outputs: Arc::new(hydration.attempt_outputs),
            });
            entries.insert(session_id, replacement.clone());
            replacement
        };
        bookkeeping.insert(session_id, 0);
        let activates_durable_mode = replacement.persistence.is_some();
        let _scope = self
            .prepared_session(replacement, bookkeeping, activates_durable_mode)?
            .activate();
        Ok(())
    }

    fn reusable_durable_destination(
        &self,
        session_id: SessionId,
        existing: Option<&Arc<SessionEntry>>,
        persistence: &Arc<dyn CostPersistence>,
        writer_lease: &lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: &CostDurabilityGate,
    ) -> Result<Option<Arc<SessionEntry>>, CostPersistError> {
        let Some(existing) = existing else {
            return Ok(None);
        };
        if let Some(existing_persistence) = &existing.persistence {
            let same_persistence = Arc::ptr_eq(existing_persistence, persistence);
            let same_lease = existing
                .writer_lease
                .as_ref()
                .is_some_and(|existing_lease| {
                    lingxi_core::host::live_sessions::same_writer_lease_authority(
                        existing_lease,
                        writer_lease,
                    )
                });
            let same_gate = existing.durability_gate.shares_authority(durability_gate);
            if !same_persistence || !same_lease || !same_gate {
                return Err(CostPersistError::Rejected(
                    "live cost session must reuse its exact durable authority".into(),
                ));
            }
            if existing.missing_durable_authority {
                return Err(CostPersistError::Rejected(
                    "live cost session has an inconsistent durable authority".into(),
                ));
            }
            return Ok(Some(existing.clone()));
        }
        let active = *self
            .ledger
            .active_session
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active == session_id {
            return Err(CostPersistError::Rejected(
                "active cost session cannot replace its live durable authority during preparation"
                    .into(),
            ));
        }
        if self
            .ledger
            .requires_durable
            .load(std::sync::atomic::Ordering::Acquire)
            && !existing.missing_durable_authority
        {
            return Err(CostPersistError::Rejected(
                "live cost session is missing its durable persistence authority".into(),
            ));
        }
        Ok(None)
    }

    async fn prepare_hydrated_durable_session(
        &self,
        session_id: SessionId,
        hydrator: &dyn CostHydrator,
        persistence: Arc<dyn CostPersistence>,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: CostDurabilityGate,
    ) -> Result<PreparedCostSession, CostPersistError> {
        if self.scope.is_some() {
            return Err(CostPersistError::Rejected(
                "a scoped cost view cannot prepare a session switch".into(),
            ));
        }
        if writer_lease.canonical_session_id() != Some(session_id) {
            return Err(CostPersistError::Rejected(
                "durable hot-switch claim does not match canonical session".into(),
            ));
        }

        let mut bookkeeping = self.ledger.hydrated_sessions.clone().lock_owned().await;
        let existing = self.ledger.existing_entry(session_id);
        if let Some(entry) = self.reusable_durable_destination(
            session_id,
            existing.as_ref(),
            &persistence,
            &writer_lease,
            &durability_gate,
        )? {
            bookkeeping.entry(session_id).or_insert(0);
            return self.prepared_session(entry, bookkeeping, true);
        }
        drop(bookkeeping);

        let hydration = hydrator.hydrate(session_id).await?;
        if hydration.state.session_id != session_id {
            return Err(CostPersistError::Storage(
                "hydrated cost state belongs to a different session".into(),
            ));
        }

        let mut bookkeeping = self.ledger.hydrated_sessions.clone().lock_owned().await;
        let entry = {
            let mut entries = self
                .ledger
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let existing = entries.get(&session_id).cloned();
            if let Some(entry) = self.reusable_durable_destination(
                session_id,
                existing.as_ref(),
                &persistence,
                &writer_lease,
                &durability_gate,
            )? {
                entry
            } else {
                let entry = Arc::new(SessionEntry {
                    session_id,
                    state: Arc::new(RwLock::new(hydration.state)),
                    persistence: Some(persistence),
                    writer_lease: Some(writer_lease),
                    durability_gate,
                    missing_durable_authority: false,
                    response_settlements: std::sync::Mutex::new(HashMap::new()),
                    attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
                    attempt_outputs: Arc::new(hydration.attempt_outputs),
                });
                entries.insert(session_id, entry.clone());
                entry
            }
        };
        bookkeeping.entry(session_id).or_insert(0);
        self.prepared_session(entry, bookkeeping, true)
    }

    /// Hydrate and validate a destination's exact durable authority without
    /// changing the active cost projection. Repeated preparation reuses the
    /// live entry byte-for-byte and never rereads a stale persisted snapshot.
    pub async fn prepare_session_hydrated_with_durable(
        self: &Arc<Self>,
        session_id: SessionId,
        hydrator: &dyn CostHydrator,
        persistence: Arc<dyn CostPersistence>,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: CostDurabilityGate,
    ) -> Result<PreparedCostSession, CostPersistError> {
        self.prepare_hydrated_durable_session(
            session_id,
            hydrator,
            persistence,
            writer_lease,
            durability_gate,
        )
        .await
    }

    /// Hydrate a session and install its own queue/lease/freeze authority
    /// before publishing it active. This is the production hot-switch seam;
    /// the simpler method above remains for ephemeral hydrators that share
    /// one process authority.
    pub async fn switch_session_hydrated_with_durable(
        &self,
        session_id: SessionId,
        hydrator: &dyn CostHydrator,
        persistence: Arc<dyn CostPersistence>,
        writer_lease: lingxi_core::host::live_sessions::SharedSessionWriterLease,
        durability_gate: CostDurabilityGate,
    ) -> Result<(), crate::persistence::CostPersistError> {
        let prepared = self
            .prepare_hydrated_durable_session(
                session_id,
                hydrator,
                persistence,
                writer_lease,
                durability_gate,
            )
            .await?;
        let _scope = prepared.activate();
        Ok(())
    }

    /// Adopt the orchestrator's construction-time session id without an
    /// async boundary. Builders run before any turn and may attach a tracker
    /// created with a placeholder id (legacy tests/hosts); move that sole
    /// pre-seeded cell while making future active writes use the real id.
    /// No budget reservation may have been issued from the tracker before this
    /// construction step; reservation ownership is intentionally not remapped.
    ///
    /// This is deliberately a construction-only migration, not a general
    /// session switch. Reusing a multi-session tracker or one that has already
    /// published a scoped view would invalidate that view's stable origin, so
    /// those shapes are rejected instead of copying spend into two sessions.
    pub fn adopt_active_session_for_builder(&self, session_id: SessionId) {
        assert!(
            self.scope.is_none(),
            "cost tracker builder adoption requires an active tracker"
        );
        let mut active = self
            .ledger
            .active_session
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = *active;
        if previous == session_id {
            return;
        }
        assert!(
            !self
                .ledger
                .requires_durable
                .load(std::sync::atomic::Ordering::Acquire),
            "durable cost authority must be constructed with the canonical session id"
        );
        assert_eq!(
            Arc::strong_count(&self.ledger),
            1,
            "cost tracker builder adoption cannot remap published scoped views"
        );
        let mut entries = self
            .ledger
            .entries
            .try_lock()
            .expect("cost tracker builder adoption must run before concurrent turns");
        assert_eq!(
            entries.len(),
            1,
            "cost tracker builder adoption requires one construction-time session"
        );
        let source = entries
            .remove(&previous)
            .expect("active cost session state exists");
        {
            let mut state = source
                .state
                .try_write()
                .expect("cost tracker builder adoption source is not in use");
            state.session_id = session_id;
        }
        let replacement = Arc::new(SessionEntry {
            session_id,
            state: source.state.clone(),
            persistence: source.persistence.clone(),
            writer_lease: source.writer_lease.clone(),
            durability_gate: source.durability_gate.clone(),
            missing_durable_authority: source.missing_durable_authority,
            response_settlements: std::sync::Mutex::new(HashMap::new()),
            attempt_settlements: std::sync::Mutex::new(attempts::AttemptRegistry::default()),
            attempt_outputs: source.attempt_outputs.clone(),
        });
        assert!(entries.insert(session_id, replacement).is_none());
        let mut hydrated = self
            .ledger
            .hydrated_sessions
            .try_lock()
            .expect("cost tracker builder adoption must run before concurrent turns");
        if let Some(baseline) = hydrated.remove(&previous) {
            hydrated.insert(session_id, baseline);
        }
        *active = session_id;
    }

    /// Merge a persisted baseline exactly once for one session. A resumed
    /// session may already have late in-memory settlement, so the baseline is
    /// added to the current cell rather than replacing it. Repeated resume
    /// hydration is a no-op, including after switching away and back.
    pub async fn restore_total_for_session(&self, session_id: SessionId, nano_usd: u64) {
        if self
            .ledger
            .requires_durable
            .load(std::sync::atomic::Ordering::Acquire)
        {
            if let Ok(authority) = self.selected_entry_for(session_id) {
                authority
                    .durability_gate
                    .freeze("legacy total restore attempted after durable authority was installed");
            }
            return;
        }
        let state = self.ledger.entry_for(session_id).state.clone();
        let mut hydrated = self.ledger.hydrated_sessions.lock().await;
        if hydrated.contains_key(&session_id) {
            return;
        }
        let mut state = state.write().await;
        state.total_nano_usd = state.total_nano_usd.saturating_add(nano_usd);
        hydrated.insert(session_id, nano_usd);
    }

    /// Resolve the exact pricing catalog owned by this tracker, applying the
    /// same non-zero unknown-model fallback used by response accounting.
    /// Callers that surface estimates can therefore report the same price and
    /// provenance that will actually be charged into this session.
    #[must_use]
    pub fn resolve_pricing_with_default(
        &self,
        model_ref: &ModelRef,
    ) -> (crate::ModelPricing, PricingResolution) {
        self.catalog.resolve(model_ref).unwrap_or_else(|_| {
            (
                PricingCatalog::default_unknown_pricing(model_ref),
                PricingResolution::UnpricedModel {
                    requested: model_ref.clone(),
                },
            )
        })
    }

    /// Legacy M1 entry point — delegates to [`Self::record_api_response_v2`]
    /// with zero cache counters, `is_batch_request = false`, and no telemetry
    /// bus. Existing M1/M2 callers compile unchanged.
    ///
    /// `retries` is the number of retried attempts that preceded this final
    /// successful call; when `0`, the call's duration is also folded into
    /// the `without_retries` counter.
    pub async fn record_api_response(
        &self,
        model_ref: ModelRef,
        usage: Usage,
        duration: Duration,
        retries: u32,
    ) {
        let _ = self
            .record_api_response_v2(
                model_ref, usage, duration, retries, 0,     // cache_read_input_tokens
                0,     // cache_creation_input_tokens
                false, // is_batch_request — M3 always false
                None,  // bus — legacy callers don't emit
            )
            .await;
    }

    /// Record a provider response through an owned finalizer.  Once a durable
    /// scope exists, the finalizer task owns the observed usage before it
    /// waits for queue capacity, so cancellation of the caller cannot lose a
    /// known bill.
    #[allow(clippy::too_many_arguments)]
    pub async fn record_api_response_v2(
        &self,
        model_ref: ModelRef,
        usage: Usage,
        duration: Duration,
        retries: u32,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        is_batch_request: bool,
        bus: Option<&Arc<AnalyticsBus>>,
    ) -> u64 {
        CostSessionScope::new(Arc::new(self.clone()))
            .submit_model_response(CostModelResponse {
                model_ref,
                usage,
                duration,
                retries,
                cache_read_input_tokens,
                cache_creation_input_tokens,
                is_batch_request,
                bus: bus.cloned(),
            })
            .settle()
            .await
            .observed_nano_usd()
    }

    /// M3-05 entry point: record one successful API response. Returns the
    /// recorded cost in nano-USD for this single call.
    ///
    /// # Spec parity
    ///
    /// - Calls [`CostCalculator::calculate_nano_usd`] for the cost; saturating
    ///   arithmetic per v3 §17.
    /// - **NEVER applies the batches discount in M3** even if
    ///   `is_batch_request = true` (which it isn't in M3 because the
    ///   `/v1/messages/batches` endpoint is M4). M4 will multiply `cost` by
    ///   `(10000 - BATCH_DISCOUNT_BPS) / 10000` for true.
    /// - The per-request success telemetry (`tengu_api_success`) is fired from
    ///   the orchestrator success path, not here; the port-only
    ///   `tengu_cost_recorded` event was dropped under strict parity.
    ///
    /// # Returns
    ///
    /// The cost for **this single call** in nano-USD (not the cumulative
    /// session total). Callers that need the cumulative total can call
    /// [`Self::total_nano_usd`].
    #[allow(clippy::too_many_arguments)]
    async fn record_api_response_v2_inner_checked(
        &self,
        mutation_id: CostMutationId,
        model_ref: ModelRef,
        usage: Usage,
        duration: Duration,
        retries: u32,
        cache_read_input_tokens: u64,
        cache_creation_input_tokens: u64,
        is_batch_request: bool,
        bus: Option<Arc<AnalyticsBus>>,
        quoted_nano_usd: Option<u64>,
        response_pricing: CostResponsePricing,
        measurement: CostResponseMeasurement,
        mut durability_turn: Option<CostDurabilityTurn>,
    ) -> CostResponseSettlement {
        // A complete frozen USD quote from the physical call is authoritative
        // for token charges. Otherwise resolve the static catalog. On a miss
        // we do NOT bill zero: mirroring
        // claude-code's getModelCosts (`utils/modelCost.ts:155-163`), the
        // tokens are billed at the DEFAULT_UNKNOWN_MODEL_COST tier ($5/$25,
        // COST_TIER_5_25) instead of returning 0, and the model is flagged
        // unknown. The TS path records this via `setHasUnknownModelCost()`;
        // ours surfaces the model in `state.unpriced_models` (below). That set
        // is the signal a `/cost` summary renderer would read to append
        // " (costs may be inaccurate due to usage of unknown models)"
        // (`cost-tracker.ts:228-233`); no Rust caller renders that string yet,
        // and the renderer lives outside the cost crate, so the warning is
        // surfaced there — here we guarantee the non-zero billing + the flag.
        let (pricing, resolution) = self.resolve_pricing_with_default(&model_ref);

        let cost =
            quoted_nano_usd.unwrap_or_else(|| CostCalculator::calculate_nano_usd(&usage, &pricing));

        // ----- update in-memory state -----
        // Durable production callers reserve queue capacity before taking the
        // state lock.  The permit is then consumed synchronously while the
        // mutation lock is held, so cancellation cannot leave an accepted
        // in-memory charge with no WAL request.
        let authority = self.selected_entry();
        let session_id = authority.session_id;
        let state_cell = authority.state.clone();
        if let Some(turn) = durability_turn.as_mut() {
            turn.wait().await;
        }
        let preflight_permit = match self.acquire_persist_permit(session_id).await {
            Ok(permit) => permit,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return CostResponseSettlement {
                    observed_nano_usd: cost,
                    persistence: Err(error),
                };
            }
        };
        enum Transfer {
            Durable {
                mutation_id: CostMutationId,
                revision: u64,
                ack_rx: tokio::sync::oneshot::Receiver<crate::persistence::CostPersistResult>,
            },
            Ephemeral(CostState),
            Rejected(CostPersistError),
        }
        let transfer = {
            let mut state = state_cell.write().await;
            let Some(next_revision) = state.cost_revision.checked_add(1) else {
                let error = CostPersistError::Storage("cost revision overflow".into());
                authority.durability_gate.freeze(error.to_string());
                return CostResponseSettlement {
                    observed_nano_usd: cost,
                    persistence: Err(error),
                };
            };
            let mut staged = state.clone();
            staged.cost_revision = next_revision;
            staged.total_nano_usd = staged.total_nano_usd.saturating_add(cost);
            #[allow(clippy::cast_possible_truncation)]
            let dur_ms = u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
            staged.total_api_duration_ms = staged.total_api_duration_ms.saturating_add(dur_ms);
            if retries == 0 {
                staged.total_api_duration_without_retries_ms = staged
                    .total_api_duration_without_retries_ms
                    .saturating_add(dur_ms);
            }
            let entry = staged
                .per_model_usage
                .entry(model_ref.clone())
                .or_insert_with(|| ModelUsage {
                    model_ref: model_ref.clone(),
                    usage: Usage::default(),
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_nano_usd: 0,
                });
            entry.usage.add(&usage);
            entry.cost_nano_usd = entry.cost_nano_usd.saturating_add(cost);
            entry.cache_read_input_tokens = entry
                .cache_read_input_tokens
                .saturating_add(cache_read_input_tokens);
            entry.cache_creation_input_tokens = entry
                .cache_creation_input_tokens
                .saturating_add(cache_creation_input_tokens);
            let unknown_hosted_tool = usage.server_tool_use.is_some_and(|tools| {
                tools.web_search_requests > 0
                    && (matches!(&resolution, PricingResolution::UnpricedModel { .. })
                        || (matches!(
                            pricing.source,
                            crate::PricingSource::PublishedPartial { .. }
                        ) && !pricing
                            .non_token_rates_nano_usd
                            .contains_key(&crate::NonTokenBillableUnit::WebSearchRequest)))
            });
            if response_pricing == CostResponsePricing::Unpriced
                || (quoted_nano_usd.is_none()
                    && (matches!(resolution, PricingResolution::UnpricedModel { .. })
                        || CostCalculator::uses_unknown_rate(&usage, &pricing)))
                || (unknown_hosted_tool
                    && !matches!(response_pricing, CostResponsePricing::CompleteQuote(_)))
            {
                staged.unpriced_models.insert(model_ref.clone());
            }
            if let Some(s) = usage.server_tool_use {
                staged.total_web_search_requests = staged
                    .total_web_search_requests
                    .saturating_add(s.web_search_requests);
            }
            if measurement == CostResponseMeasurement::Observed {
                staged.last_usage = Some(usage);
                staged.last_usage_revision = Some(next_revision);
                staged.last_cache_read_input_tokens = cache_read_input_tokens;
                staged.last_cache_creation_input_tokens = cache_creation_input_tokens;
            }
            match preflight_permit {
                Some(permit) => match self.enqueue_snapshot_locked_with_id(
                    &staged,
                    permit,
                    CostMutationSource::ModelResponse,
                    mutation_id,
                ) {
                    Ok((mutation_id, revision, ack_rx)) => {
                        *state = staged;
                        Transfer::Durable {
                            mutation_id,
                            revision,
                            ack_rx,
                        }
                    }
                    Err(error) => Transfer::Rejected(error),
                },
                None => {
                    *state = staged.clone();
                    Transfer::Ephemeral(staged)
                }
            }
        };
        let persistence = match transfer {
            Transfer::Durable {
                mutation_id,
                revision,
                ack_rx,
            } => self
                .await_persistence_ack(session_id, mutation_id, revision, ack_rx)
                .await
                .map(Some),
            Transfer::Ephemeral(snapshot) => {
                let _ = self.persist_tx.send(snapshot).await;
                Ok(None)
            }
            Transfer::Rejected(error) => {
                authority.durability_gate.freeze(error.to_string());
                Err(error)
            }
        };

        // `tengu_cost_recorded` was a PORT-ONLY event (0 hits in claude-code
        // 2.1.195) — dropped under strict parity. The per-request success
        // telemetry is now `tengu_api_success`, fired from the orchestrator
        // success path (where request id / stop reason / provider live), not
        // from here. Cost ACCOUNTING above is untouched.
        let _ = (&bus, is_batch_request, &session_id);

        if let Some(turn) = durability_turn {
            turn.finish();
        }

        CostResponseSettlement {
            observed_nano_usd: cost,
            persistence,
        }
    }

    /// Cumulative cost across all models in nano-USD.
    pub async fn total_nano_usd(&self) -> u64 {
        self.selected_state().await.read().await.total_nano_usd
    }

    /// Update the in-memory state with externally-priced spend and return the
    /// snapshot that should be persisted.
    ///
    /// This is the lock-only half of [`Self::record_external_cost`]. Budget
    /// settlement uses it while holding its reservation-book lock so a
    /// concurrent reserve observes either the pre-settlement hold or the
    /// post-settlement realized spend, never both. The persistence send must
    /// happen after that lock is released; a bounded persistence channel must
    /// not hold budget capacity hostage.
    pub(crate) async fn record_external_cost_snapshot(
        &self,
        nano_usd: u64,
    ) -> Result<Option<CostState>, CostPersistError> {
        if nano_usd == 0 {
            return Ok(None);
        }
        let state_cell = self.selected_state().await;
        let mut state = state_cell.write().await;
        let staged = Self::record_external_cost_in_state(&state, nano_usd)?;
        if let Some(staged) = &staged {
            *state = staged.clone();
        }
        Ok(staged)
    }

    /// Stage one external charge from an already-held state guard. The caller
    /// publishes the returned clone only after synchronous queue acceptance.
    /// No await is performed, so budget settlement can preserve its atomic
    /// state/reservation transition.
    pub(crate) fn record_external_cost_in_state(
        state: &CostState,
        nano_usd: u64,
    ) -> Result<Option<CostState>, CostPersistError> {
        if nano_usd == 0 {
            return Ok(None);
        }
        let next_revision = state
            .cost_revision
            .checked_add(1)
            .ok_or_else(|| CostPersistError::Storage("cost revision overflow".into()))?;
        let mut staged = state.clone();
        staged.cost_revision = next_revision;
        staged.total_nano_usd = staged.total_nano_usd.saturating_add(nano_usd);
        // Track this addition separately from `per_model_usage` (which
        // this call never touches — there is no single `ModelRef` for a
        // Fusion run's several priced components) so a summary can tell
        // "unattributed but accounted-for" apart from a `by_model`
        // breakdown that has silently fallen behind the total.
        staged.external_nano_usd = staged.external_nano_usd.saturating_add(nano_usd);
        Ok(Some(staged))
    }

    /// Persist a snapshot produced by one of the lock-only accounting
    /// operations. The sender is intentionally awaited outside any budget
    /// reservation lock.
    pub(crate) async fn acquire_persist_permit(
        &self,
        session_id: SessionId,
    ) -> Result<Option<crate::persistence::CostPersistPermit>, crate::persistence::CostPersistError>
    {
        let authority = self.selected_entry_for(session_id)?;
        if authority.missing_durable_authority {
            return Err(crate::persistence::CostPersistError::Rejected(
                "durable session scope has not been hydrated".into(),
            ));
        }
        let Some(persistence) = authority.persistence.clone() else {
            if self
                .ledger
                .requires_durable
                .load(std::sync::atomic::Ordering::Acquire)
            {
                return Err(crate::persistence::CostPersistError::Rejected(
                    "durable session scope has no persistence authority".into(),
                ));
            }
            return Ok(None);
        };
        if let Some(reason) = authority.durability_gate.frozen_reason() {
            return Err(crate::persistence::CostPersistError::Frozen(reason));
        }
        let permit = persistence.acquire_permit(session_id).await?;
        if let Some(reason) = authority.durability_gate.frozen_reason() {
            return Err(crate::persistence::CostPersistError::Frozen(reason));
        }
        Ok(Some(permit))
    }

    /// Enqueue an already-mutated snapshot through a permit acquired before
    /// the caller's state/reservation locks. The acknowledgement is awaited
    /// before returning, while no state lock is held.
    pub(crate) fn enqueue_snapshot_locked(
        &self,
        snapshot: &CostState,
        permit: crate::persistence::CostPersistPermit,
        source: CostMutationSource,
    ) -> Result<
        (
            CostMutationId,
            u64,
            tokio::sync::oneshot::Receiver<crate::persistence::CostPersistResult>,
        ),
        crate::persistence::CostPersistError,
    > {
        let session_id = snapshot.session_id;
        let mutation_id =
            CostMutationId::new(format!("cost:v1:{}:{}", session_id, snapshot.cost_revision));
        self.enqueue_snapshot_locked_with_id(snapshot, permit, source, mutation_id)
    }

    pub(crate) fn enqueue_snapshot_locked_with_id(
        &self,
        snapshot: &CostState,
        permit: crate::persistence::CostPersistPermit,
        source: CostMutationSource,
        mutation_id: CostMutationId,
    ) -> Result<
        (
            CostMutationId,
            u64,
            tokio::sync::oneshot::Receiver<crate::persistence::CostPersistResult>,
        ),
        crate::persistence::CostPersistError,
    > {
        let session_id = snapshot.session_id;
        let expected_revision = snapshot.cost_revision;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        permit.enqueue(CostPersistRequest {
            session_id,
            cost_revision: snapshot.cost_revision,
            mutation_id: mutation_id.clone(),
            state: crate::persistence::CostStateVector::from(snapshot),
            source,
            ack: ack_tx,
        })?;
        Ok((mutation_id, expected_revision, ack_rx))
    }

    pub(crate) async fn await_persistence_ack(
        &self,
        session_id: SessionId,
        expected_mutation_id: CostMutationId,
        expected_cost_revision: u64,
        ack_rx: tokio::sync::oneshot::Receiver<crate::persistence::CostPersistResult>,
    ) -> Result<crate::persistence::CostPersistAck, crate::persistence::CostPersistError> {
        let gate = self.selected_entry_for(session_id)?.durability_gate.clone();
        let result = match ack_rx.await {
            Ok(Ok(ack))
                if ack.mutation_id == expected_mutation_id
                    && ack.cost_revision == expected_cost_revision
                    && ack.journal_revision > 0 =>
            {
                Ok(ack)
            }
            Ok(Ok(_)) => Err(crate::persistence::CostPersistError::Storage(
                "durable cost acknowledgment identity/revision mismatch".into(),
            )),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(crate::persistence::CostPersistError::Storage(
                "durable cost acknowledgment dropped".into(),
            )),
        };
        if let Err(error) = &result {
            gate.freeze(error.to_string());
        }
        result
    }

    pub(crate) async fn persist_snapshot_with_permit(
        &self,
        snapshot: CostState,
        permit: crate::persistence::CostPersistPermit,
    ) {
        let session_id = snapshot.session_id;
        match self.enqueue_snapshot_locked(&snapshot, permit, CostMutationSource::ModelResponse) {
            Ok((mutation_id, revision, ack_rx)) => {
                let _ = self
                    .await_persistence_ack(session_id, mutation_id, revision, ack_rx)
                    .await;
            }
            Err(error) => {
                self.selected_entry_for(session_id)
                    .expect("ack session authority remains immutable")
                    .durability_gate
                    .freeze(error.to_string());
            }
        }
    }

    /// Compatibility path for callers that already hold no budget lock. New
    /// production accounting acquires the permit before its mutation lock;
    /// this fallback keeps test/mobile constructors and older callers working.
    pub(crate) async fn persist_snapshot(&self, snapshot: CostState) {
        let durability_turn = match self.enter_durable_mutation().await {
            Ok(turn) => turn,
            Err(error) => {
                self.selected_entry_for(snapshot.session_id)
                    .expect("snapshot session authority remains immutable")
                    .durability_gate
                    .freeze(error.to_string());
                return;
            }
        };
        match self.acquire_persist_permit(snapshot.session_id).await {
            Ok(Some(permit)) => self.persist_snapshot_with_permit(snapshot, permit).await,
            Ok(None) => {
                let _ = self.persist_tx.send(snapshot).await;
            }
            Err(error) => {
                self.selected_entry_for(snapshot.session_id)
                    .expect("snapshot session authority remains immutable")
                    .durability_gate
                    .freeze(error.to_string());
            }
        }
        if let Some(turn) = durability_turn {
            turn.finish();
        }
    }

    /// Add externally-priced spend directly onto the cumulative total.
    ///
    /// Used by [`crate::budget::BudgetEnforcer::commit_reservation`] for
    /// Fusion: panel/analyst/synth spend is priced by the fusion crate's own
    /// `FusionPriceBook` from usage the provider adapters never funnel through
    /// [`Self::record_api_response_v2`] (there is no single per-call
    /// `ModelRef`/`Usage` at that seam — a Fusion run prices several models'
    /// worth of usage into one already-computed `actual_nano_usd`). The
    /// addition happens under the SAME write lock `record_api_response_v2`
    /// uses so two concurrent commits cannot lose an update the way a
    /// read-then-[`Self::restore_total_nano_usd`] pair could.
    ///
    /// Emits on the persist channel like a real charge (unlike
    /// `restore_total_nano_usd`, which is a resume hydrate). No-op for `0`.
    ///
    /// KNOWN LIMITATION (tracked, not silently swept): this makes
    /// [`CostState::external_nano_usd`] and [`crate::summary::CostTracker::summary`]
    /// self-reconciling, but does NOT reach the production `/usage`/`/cost`
    /// rendering path — `ConversationModel::snapshot_cost_real`
    /// (`orchestrator/src/conversation/model.rs`) builds `lingxi_core::host::CostSnapshot`
    /// from `state.total_nano_usd` and `state.per_model_usage` directly and does
    /// not read this field, so a Fusion-only session still renders a nonzero
    /// total above a `by_model`/token breakdown of zero. Closing that requires
    /// either threading `external_nano_usd` through `CostSnapshot` and
    /// `cost::render::cost_summary_from_snapshot` (touches ~12 `CostSnapshot { .. }`
    /// literal sites plus the byte-pinned render template), or recording Fusion
    /// usage per-model at the spawner/side-query layer instead — a larger,
    /// separate task.
    pub async fn record_external_cost(&self, nano_usd: u64) {
        if nano_usd == 0 {
            return;
        }
        let authority = self.selected_entry();
        let session_id = authority.session_id;
        let state_cell = authority.state.clone();
        let durability_turn = match self.enter_durable_mutation().await {
            Ok(turn) => turn,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let permit = match self.acquire_persist_permit(session_id).await {
            Ok(permit) => permit,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let (snapshot, ack, rejected) = {
            let mut state = state_cell.write().await;
            let staged = match Self::record_external_cost_in_state(&state, nano_usd) {
                Ok(staged) => staged,
                Err(error) => {
                    authority.durability_gate.freeze(error.to_string());
                    return;
                }
            };
            let Some(staged) = staged else {
                return;
            };
            match permit {
                Some(permit) => match self.enqueue_snapshot_locked(
                    &staged,
                    permit,
                    CostMutationSource::FusionAggregate,
                ) {
                    Ok(ack) => {
                        *state = staged;
                        (None, Some(ack), None)
                    }
                    Err(error) => (None, None, Some(error)),
                },
                None => {
                    *state = staged.clone();
                    (Some(staged), None, None)
                }
            }
        };
        if let Some(error) = rejected {
            authority.durability_gate.freeze(error.to_string());
        } else if let Some((mutation_id, revision, ack_rx)) = ack {
            let _ = self
                .await_persistence_ack(session_id, mutation_id, revision, ack_rx)
                .await;
        } else if let Some(snapshot) = snapshot {
            let _ = self.persist_tx.send(snapshot).await;
        }
        if let Some(turn) = durability_turn {
            turn.finish();
        }
    }

    /// Reset every cumulative counter to zero — the parity twin of claude-code
    /// `resetCostState` (`yJe`), which `clearConversation` invokes so `/clear`
    /// starts a fresh session with a zeroed cost footer/status line instead of
    /// carrying the prior conversation's accumulated total (2.1.211 fix
    /// "Fixed /clear not resetting session cost counter").
    ///
    /// Zeroes the money total, per-model usage breakdown, API/tool durations,
    /// unpriced-model flags, web-search count, and code-change line counters —
    /// the full set `yJe` resets (`totalCostUSD`, `totalAPIDuration`,
    /// `totalAPIDurationWithoutRetries`, `totalToolDuration`, `modelUsage`,
    /// `hasUnknownModelCost`, `totalLinesAdded`, `totalLinesRemoved`).
    ///
    /// The `session_id` is PRESERVED: claude-code regenerates the id in a
    /// separate step (`regenerateSessionId`), and lingxi's orchestrator mints
    /// the fresh `SessionId` on `SessionState` in `clear_session`; the tracker's
    /// copy is informational (snapshots key the id off the live session state).
    ///
    /// In-memory only: like [`Self::restore_total_nano_usd`], this does NOT emit
    /// on the persist channel — a reset is not a new charge, and it mirrors
    /// `yJe`, which mutates the in-process cost singleton only (the prior
    /// session's totals are saved separately, before the reset).
    pub async fn reset(&self) {
        let authority = self.selected_entry();
        let session_id = authority.session_id;
        let state_cell = authority.state.clone();
        let durability_turn = match self.enter_durable_mutation().await {
            Ok(turn) => turn,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let permit = match self.acquire_persist_permit(session_id).await {
            Ok(permit) => permit,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let (ack, rejected) = {
            let mut state = state_cell.write().await;
            let Some(next_revision) = state.cost_revision.checked_add(1) else {
                authority.durability_gate.freeze("cost revision overflow");
                return;
            };
            let snapshot = CostState {
                session_id,
                cost_revision: next_revision,
                ..Default::default()
            };
            match permit {
                Some(permit) => match self.enqueue_snapshot_locked(
                    &snapshot,
                    permit,
                    CostMutationSource::Administrative,
                ) {
                    Ok(ack) => {
                        *state = snapshot;
                        (Some(ack), None)
                    }
                    Err(error) => (None, Some(error)),
                },
                None => {
                    *state = snapshot;
                    (None, None)
                }
            }
        };
        if let Some(error) = rejected {
            authority.durability_gate.freeze(error.to_string());
            return;
        }
        self.ledger
            .hydrated_sessions
            .lock()
            .await
            .remove(&session_id);
        if let Some((mutation_id, revision, ack_rx)) = ack {
            let _ = self
                .await_persistence_ack(session_id, mutation_id, revision, ack_rx)
                .await;
        }
        if let Some(turn) = durability_turn {
            turn.finish();
        }
    }

    /// Accumulate one edit's line changes (claude-code `Bhn(added, removed)`:
    /// `Pt.totalLinesAdded += added; Pt.totalLinesRemoved += removed`).
    pub async fn record_code_change(&self, added: u64, removed: u64) {
        let authority = self.selected_entry();
        let turn = match self.register_durable_mutation_for(&authority) {
            Ok(turn) => turn,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let Some(turn) = turn else {
            self.record_code_change_inner(added, removed, None).await;
            return;
        };
        let tracker = self.scoped(authority.session_id);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            authority
                .durability_gate
                .freeze("code-change persistence ownership requires an async runtime");
            return;
        };
        // Streaming tools are borrowed futures, not independently scheduled
        // tasks. Their driver may stop polling them while awaiting the next
        // model settlement, which is queued behind this mutation. The owned
        // writer waiter must release our FIFO turn after the exact ack even
        // when that tool future is parked or dropped.
        let worker = runtime.spawn(async move {
            tracker
                .record_code_change_inner(added, removed, Some(turn))
                .await;
        });
        if let Err(error) = worker.await {
            authority
                .durability_gate
                .freeze(format!("code-change persistence worker failed: {error}"));
        }
    }

    async fn record_code_change_inner(
        &self,
        added: u64,
        removed: u64,
        mut durability_turn: Option<CostDurabilityTurn>,
    ) {
        let authority = self.selected_entry();
        let session_id = authority.session_id;
        let state_cell = authority.state.clone();
        if let Some(turn) = durability_turn.as_mut() {
            turn.wait().await;
        }
        let permit = match self.acquire_persist_permit(session_id).await {
            Ok(permit) => permit,
            Err(error) => {
                authority.durability_gate.freeze(error.to_string());
                return;
            }
        };
        let (snapshot, ack, rejected) = {
            let mut state = state_cell.write().await;
            let Some(next_revision) = state.cost_revision.checked_add(1) else {
                authority.durability_gate.freeze("cost revision overflow");
                return;
            };
            let mut staged = state.clone();
            staged.cost_revision = next_revision;
            staged.total_lines_added = staged.total_lines_added.saturating_add(added);
            staged.total_lines_removed = staged.total_lines_removed.saturating_add(removed);
            match permit {
                Some(permit) => match self.enqueue_snapshot_locked(
                    &staged,
                    permit,
                    CostMutationSource::Administrative,
                ) {
                    Ok(ack) => {
                        *state = staged;
                        (None, Some(ack), None)
                    }
                    Err(error) => (None, None, Some(error)),
                },
                None => {
                    *state = staged.clone();
                    (Some(staged), None, None)
                }
            }
        };
        if let Some(error) = rejected {
            authority.durability_gate.freeze(error.to_string());
        } else if let Some((mutation_id, revision, ack_rx)) = ack {
            let _ = self
                .await_persistence_ack(session_id, mutation_id, revision, ack_rx)
                .await;
        } else if let Some(snapshot) = snapshot {
            let _ = self.persist_tx.send(snapshot).await;
        }
        if let Some(turn) = durability_turn {
            turn.finish();
        }
    }

    /// Snapshot the current state. Cloned, safe to inspect off-thread.
    pub async fn snapshot(&self) -> CostState {
        self.selected_state().await.read().await.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::{CostHydration, CostPersistAck, CostPersistError, CostPersistPermit};
    use crate::pricing::ProviderId;
    use crate::usage::TokenUsage;
    use async_trait::async_trait;
    use lingxi_core::host::live_sessions::{SessionWriterLease, SharedSessionWriterLease};

    #[tokio::test]
    async fn frozen_client_quote_overrides_conditional_catalog_fallback() {
        use crate::pricing::{MoneyPerToken, PricingSource, TokenClass};
        let model_ref = ModelRef {
            provider: ProviderId::OpenAICompatible {
                name: "deepseek".into(),
            },
            model: "deepseek-flash".into(),
        };
        let pricing = crate::ModelPricing {
            model_ref: model_ref.clone(),
            token_rates: HashMap::from([
                (
                    TokenClass::Input,
                    MoneyPerToken {
                        nano_usd_per_token: 300,
                    },
                ),
                (
                    TokenClass::Output,
                    MoneyPerToken {
                        nano_usd_per_token: 1_200,
                    },
                ),
            ]),
            non_token_rates_nano_usd: HashMap::new(),
            effective_from: None,
            source: PricingSource::PublishedPartial {
                provider: model_ref.provider.clone(),
                conditional: true,
            },
        };
        let (persist_tx, _legacy_rx) = mpsc::channel(8);
        let session = SessionId::new();
        let tracker = Arc::new(CostTracker::new(
            session,
            Arc::new(PricingCatalog::empty().with_entry(pricing)),
            persist_tx,
        ));
        let response = || CostModelResponse {
            model_ref: model_ref.clone(),
            usage: Usage {
                tokens: TokenUsage {
                    input: 1_000,
                    output: 1_000,
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
        };
        let quoted = tracker
            .session_scope(session)
            .submit_model_response_with_pricing(
                response(),
                CostResponsePricing::TokenQuote(750_000),
                CostResponseMeasurement::Observed,
            )
            .settle()
            .await;
        assert_eq!(quoted.observed_nano_usd(), 750_000);
        let snapshot = tracker.snapshot().await;
        assert_eq!(snapshot.total_nano_usd, 750_000);
        assert!(!snapshot.unpriced_models.contains(&model_ref));

        let unknown = tracker
            .session_scope(session)
            .submit_model_response(response())
            .settle()
            .await;
        assert_eq!(unknown.observed_nano_usd(), 30_000_000);
        assert!(tracker
            .snapshot()
            .await
            .unpriced_models
            .contains(&model_ref));
    }

    #[tokio::test]
    async fn complete_quotes_include_tool_fees_and_incomplete_quotes_stay_unpriced() {
        use crate::TokenClass;
        let model_ref = ModelRef {
            provider: ProviderId::Anthropic,
            model: "quoted-model".into(),
        };
        for (pricing, expected_cost, unpriced, catalog_available) in [
            (CostResponsePricing::Catalog, 21_000_000, false, true),
            (
                CostResponsePricing::TokenQuote(1_000),
                20_001_000,
                false,
                true,
            ),
            (
                CostResponsePricing::CompleteQuote(1_000),
                1_000,
                false,
                true,
            ),
            (
                CostResponsePricing::CompleteQuote(1_000),
                1_000,
                false,
                false,
            ),
            (CostResponsePricing::CompleteQuote(0), 0, false, true),
            (CostResponsePricing::Unpriced, 0, true, true),
            (CostResponsePricing::Unpriced, 0, true, false),
        ] {
            let catalog_entry = crate::ModelPricing {
                model_ref: model_ref.clone(),
                token_rates: HashMap::from([
                    (
                        TokenClass::Input,
                        crate::MoneyPerToken {
                            nano_usd_per_token: 5_000,
                        },
                    ),
                    (
                        TokenClass::Output,
                        crate::MoneyPerToken {
                            nano_usd_per_token: 25_000,
                        },
                    ),
                ]),
                non_token_rates_nano_usd: HashMap::from([(
                    crate::NonTokenBillableUnit::WebSearchRequest,
                    10_000_000,
                )]),
                effective_from: None,
                source: crate::PricingSource::BuiltInReference {
                    provider: ProviderId::Anthropic,
                },
            };
            let (persist_tx, _legacy_rx) = mpsc::channel(8);
            let session_id = SessionId::new();
            let catalog = if catalog_available {
                PricingCatalog::empty().with_entry(catalog_entry)
            } else {
                PricingCatalog::empty()
            };
            let tracker = Arc::new(CostTracker::new(session_id, Arc::new(catalog), persist_tx));
            let usage = Usage {
                tokens: TokenUsage {
                    input: 100,
                    output: 20,
                    ..Default::default()
                },
                server_tool_use: Some(crate::ServerToolUsage {
                    web_search_requests: 2,
                }),
                ..Default::default()
            };
            let receipt = tracker
                .session_scope(session_id)
                .submit_model_response_with_pricing(
                    CostModelResponse {
                        model_ref: model_ref.clone(),
                        usage,
                        duration: Duration::ZERO,
                        retries: 0,
                        cache_read_input_tokens: 0,
                        cache_creation_input_tokens: 0,
                        is_batch_request: false,
                        bus: None,
                    },
                    pricing,
                    CostResponseMeasurement::Observed,
                );
            let settled = receipt.settle().await;
            assert_eq!(settled.observed_nano_usd(), expected_cost, "{pricing:?}");
            let state = tracker.snapshot().await;
            assert_eq!(state.total_nano_usd, expected_cost);
            assert_eq!(state.unpriced_models.contains(&model_ref), unpriced);
            assert_eq!(state.per_model_usage[&model_ref].usage, usage);
        }
    }

    #[tokio::test]
    async fn price_without_aggregate_usage_keeps_the_measurement_missing() {
        let session_id = SessionId::new();
        let (persist_tx, _legacy_rx) = mpsc::channel(8);
        let tracker = Arc::new(CostTracker::new(
            session_id,
            Arc::new(PricingCatalog::empty()),
            persist_tx,
        ));
        let scope = tracker.session_scope(session_id);
        let receipt = scope.submit_model_response_with_pricing(
            CostModelResponse {
                model_ref: ModelRef {
                    provider: ProviderId::Anthropic,
                    model: "served".into(),
                },
                usage: Usage::default(),
                duration: Duration::ZERO,
                retries: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                is_batch_request: false,
                bus: None,
            },
            CostResponsePricing::CompleteQuote(12_000_000),
            CostResponseMeasurement::Missing,
        );
        let mutation_id = receipt.mutation_id().clone();
        assert_eq!(receipt.settle().await.observed_nano_usd(), 12_000_000);
        let state = tracker.snapshot().await;
        assert_eq!(state.total_nano_usd, 12_000_000);
        assert!(state.last_usage.is_none());
        assert!(state.last_usage_revision.is_none());
        assert!(state.unpriced_models.is_empty());
        assert_eq!(
            scope
                .retained_response(&mutation_id)
                .unwrap()
                .observation
                .measurement,
            CostResponseMeasurement::Missing
        );
    }

    struct TestLease(String);

    impl SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    struct TestPersistence {
        requests: tokio::sync::mpsc::UnboundedSender<CostPersistRequest>,
    }

    struct RejectingPersistence;

    struct StaticHydrator(CostHydration);

    struct PanicHydrator;

    struct BlockingHydrator {
        hydration: CostHydration,
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: tokio::sync::Notify,
    }

    impl BlockingHydrator {
        fn new(hydration: CostHydration) -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            (
                Arc::new(Self {
                    hydration,
                    entered: std::sync::Mutex::new(Some(entered_tx)),
                    release: tokio::sync::Notify::new(),
                }),
                entered_rx,
            )
        }
    }

    #[async_trait]
    impl CostHydrator for StaticHydrator {
        async fn hydrate(&self, session_id: SessionId) -> Result<CostHydration, CostPersistError> {
            if self.0.state.session_id != session_id {
                return Err(CostPersistError::Rejected(
                    "test hydration session mismatch".into(),
                ));
            }
            Ok(self.0.clone())
        }
    }

    #[async_trait]
    impl CostHydrator for PanicHydrator {
        async fn hydrate(&self, _session_id: SessionId) -> Result<CostHydration, CostPersistError> {
            panic!("an exact prepared durable entry must not be hydrated again")
        }
    }

    #[async_trait]
    impl CostHydrator for BlockingHydrator {
        async fn hydrate(&self, session_id: SessionId) -> Result<CostHydration, CostPersistError> {
            if self.hydration.state.session_id != session_id {
                return Err(CostPersistError::Rejected(
                    "test hydration session mismatch".into(),
                ));
            }
            if let Some(entered) = self
                .entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = entered.send(());
            }
            self.release.notified().await;
            Ok(self.hydration.clone())
        }
    }

    #[async_trait]
    impl CostPersistence for RejectingPersistence {
        async fn acquire_permit(
            &self,
            _session_id: SessionId,
        ) -> Result<CostPersistPermit, CostPersistError> {
            Ok(CostPersistPermit::new(|_request| {
                Err(CostPersistError::Rejected(
                    "synthetic enqueue rejection".into(),
                ))
            }))
        }
    }

    fn hydration(session_id: SessionId) -> CostHydration {
        CostHydration {
            state: CostState {
                session_id,
                ..Default::default()
            },
            journal_revision: 0,
            attempt_outputs: Vec::new(),
        }
    }

    #[async_trait]
    impl CostPersistence for TestPersistence {
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

    #[tokio::test]
    async fn record_accumulates_cost() {
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        tracker
            .record_api_response(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 1000,
                        output: 500,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(200),
                0,
            )
            .await;
        let snap = rx.recv().await.unwrap();
        // 1000 * 5000 + 500 * 25000 = 5_000_000 + 12_500_000 = 17_500_000 nano-USD = $0.0175
        assert_eq!(snap.total_nano_usd, 17_500_000);
    }

    #[test]
    fn attempt_host_binding_requires_fixed_durable_unfrozen_origin() {
        let (persist_tx, _) = mpsc::channel(8);
        let session = SessionId::new();
        let ephemeral = CostTracker::new(
            session,
            Arc::new(PricingCatalog::empty()),
            persist_tx.clone(),
        );
        assert!(ephemeral
            .scoped(session)
            .validate_attempt_host_binding(session)
            .is_err());
        let (requests, _) = tokio::sync::mpsc::unbounded_channel();
        let tracker = CostTracker::new(session, Arc::new(PricingCatalog::empty()), persist_tx)
            .try_with_durable_persistence(
                hydration(session),
                Arc::new(TestPersistence { requests }),
                Arc::new(TestLease(session.to_string())),
                CostDurabilityGate::default(),
            )
            .unwrap();
        assert!(tracker.validate_attempt_host_binding(session).is_err());
        let captured = tracker.scoped(session);
        assert!(captured.validate_attempt_host_binding(session).is_ok());
        assert!(captured
            .validate_attempt_host_binding(SessionId::new())
            .is_err());
        assert!(tracker
            .scoped(SessionId::new())
            .validate_attempt_host_binding(session)
            .is_err());
        captured.durability_gate().freeze("host unavailable");
        assert!(matches!(
            captured.validate_attempt_host_binding(session),
            Err(CostPersistError::Frozen(_))
        ));
    }

    #[tokio::test]
    async fn parked_code_change_caller_does_not_block_durable_preflight() {
        let (persist_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracker = CostTracker::new(
            session_id,
            Arc::new(PricingCatalog::builtin_reference()),
            persist_tx,
        )
        .try_with_durable_persistence(
            hydration(session_id),
            Arc::new(TestPersistence {
                requests: requests_tx,
            }),
            Arc::new(TestLease(session_id.to_string())),
            CostDurabilityGate::default(),
        )
        .unwrap();
        let mut edit = Box::pin(tracker.record_code_change(1, 1));
        let request = tokio::select! {
            result = &mut edit => panic!("edit completed before durable acknowledgment: {result:?}"),
            request = requests_rx.recv() => request.unwrap(),
        };
        request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: request.mutation_id,
                journal_revision: 257,
                cost_revision: request.cost_revision,
            }))
            .unwrap();
        // The streaming driver can park this borrowed tool future while it
        // waits for another settlement. The durable owner must still release
        // the FIFO position without another poll of `edit`.
        let turn =
            tokio::time::timeout(Duration::from_secs(1), tracker.acquire_durable_preflight())
                .await
                .expect("parked tool future retained the acknowledged cost turn")
                .unwrap();
        drop(turn);
        edit.await;
        assert_eq!(tracker.snapshot().await.total_lines_added, 1);
        assert!(tracker.durability_gate().frozen_reason().is_none());

        // Cancellation after the edit was accepted also leaves its durable
        // owner alive; dropping the UI/tool waiter must not freeze the ledger.
        let mut cancelled_edit = Box::pin(tracker.record_code_change(2, 0));
        let request = tokio::select! {
            result = &mut cancelled_edit => panic!("edit completed before acknowledgment: {result:?}"),
            request = requests_rx.recv() => request.unwrap(),
        };
        drop(cancelled_edit);
        request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: request.mutation_id,
                journal_revision: 258,
                cost_revision: request.cost_revision,
            }))
            .unwrap();
        let turn =
            tokio::time::timeout(Duration::from_secs(1), tracker.acquire_durable_preflight())
                .await
                .expect("cancelled tool waiter prevented durable completion")
                .unwrap();
        drop(turn);
        assert_eq!(tracker.snapshot().await.total_lines_added, 3);
        assert!(tracker.durability_gate().frozen_reason().is_none());
    }

    #[tokio::test]
    async fn durable_record_waits_for_ack_and_freezes_on_append_failure() {
        let (persist_tx, _legacy_rx) = mpsc::channel(8);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = CostTracker::new(
            session_id,
            Arc::new(PricingCatalog::builtin_reference()),
            persist_tx,
        )
        .try_with_durable_persistence(
            hydration(session_id),
            Arc::new(TestPersistence {
                requests: requests_tx,
            }),
            lease,
            CostDurabilityGate::default(),
        )
        .expect("matching session lease");
        let task = tokio::spawn(async move {
            tracker
                .record_api_response(
                    ModelRef {
                        provider: ProviderId::Anthropic,
                        model: "claude-opus-4-6".into(),
                    },
                    Usage {
                        tokens: TokenUsage {
                            input: 1,
                            output: 1,
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                    Duration::from_millis(1),
                    0,
                )
                .await;
            tracker
                .durability_gate()
                .frozen_reason()
                .expect("append failure freezes the session");
        });
        let request = requests_rx.recv().await.expect("durable request");
        request
            .ack
            .send(Err(CostPersistError::Storage("append failed".into())))
            .expect("caller is still awaiting the ack");
        task.await.expect("record task");
    }

    #[tokio::test]
    async fn enqueue_rejection_retains_observed_bill_without_publishing_state() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(RejectingPersistence),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let receipt = tracker
            .session_scope(session_id)
            .submit_model_response(CostModelResponse {
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

        let outcome = receipt.settle().await;

        assert!(outcome.observed_nano_usd() > 0);
        assert!(matches!(
            outcome.persistence_result(),
            Err(CostPersistError::Rejected(message)) if message.contains("synthetic")
        ));
        assert_eq!(tracker.snapshot().await.total_nano_usd, 0);
        assert!(tracker.durability_gate().frozen_reason().is_some());
    }

    #[tokio::test]
    async fn dropped_waiter_keeps_exact_failed_response_in_the_session_owner() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(RejectingPersistence),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let scope = tracker.session_scope(session_id);
        let model_ref = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let usage = Usage {
            tokens: TokenUsage {
                input: 7,
                output: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        let receipt = scope.submit_model_response(CostModelResponse {
            model_ref: model_ref.clone(),
            usage,
            duration: Duration::from_millis(3),
            retries: 1,
            cache_read_input_tokens: 2,
            cache_creation_input_tokens: 1,
            is_batch_request: false,
            bus: None,
        });
        let mutation_id = receipt.mutation_id().clone();
        drop(receipt);

        let retained = loop {
            let retained = scope
                .retained_response(&mutation_id)
                .expect("observation is installed synchronously");
            if retained.settlement.is_some() {
                break retained;
            }
            tokio::task::yield_now().await;
        };

        assert_eq!(retained.mutation_id, mutation_id);
        assert_eq!(retained.observation.model_ref, model_ref);
        assert_eq!(retained.observation.usage, usage);
        assert!(retained.observation.observed_nano_usd > 0);
        assert!(matches!(
            retained.settlement,
            Some(Err(CostPersistError::Rejected(message))) if message.contains("synthetic")
        ));
        assert!(tracker.durability_gate().frozen_reason().is_some());
    }

    #[tokio::test]
    async fn shutdown_drain_waits_for_a_dropped_response_receipt_and_exact_ack() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(TestPersistence {
                    requests: requests_tx,
                }),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let receipt = tracker
            .session_scope(session_id)
            .submit_model_response(CostModelResponse {
                model_ref: ModelRef {
                    provider: ProviderId::Anthropic,
                    model: "claude-opus-4-6".into(),
                },
                usage: Usage {
                    tokens: TokenUsage {
                        input: 7,
                        output: 3,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                duration: Duration::from_millis(3),
                retries: 0,
                cache_read_input_tokens: 0,
                cache_creation_input_tokens: 0,
                is_batch_request: false,
                bus: None,
            });
        drop(receipt);
        let request = requests_rx.recv().await.expect("durable response request");

        let mut drain = Box::pin(tracker.drain_owned_settlements());
        tokio::select! {
            biased;
            result = &mut drain => panic!("shutdown drain returned before response ack: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: request.mutation_id,
                journal_revision: 1,
                cost_revision: request.cost_revision,
            }))
            .unwrap();

        drain.await.unwrap();
        let snapshot = tracker.snapshot().await;
        assert!(snapshot.total_nano_usd > 0);
        let usage = snapshot.last_usage.expect("response usage was retained");
        assert_eq!(usage.tokens.input, 7);
        assert_eq!(usage.tokens.output, 3);
    }

    #[tokio::test]
    async fn each_response_receipt_keeps_its_own_serialized_durable_result() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(TestPersistence {
                    requests: requests_tx,
                }),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let scope = tracker.session_scope(session_id);
        let response = || CostModelResponse {
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
        };
        let first = scope.submit_model_response(response());
        let first_request = requests_rx.recv().await.unwrap();
        let second = scope.submit_model_response(response());
        first_request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: first_request.mutation_id.clone(),
                journal_revision: 1,
                cost_revision: first_request.cost_revision,
            }))
            .unwrap();
        let second_request = requests_rx.recv().await.unwrap();
        assert_eq!(second_request.cost_revision, 2);
        second_request
            .ack
            .send(Err(CostPersistError::Storage("second failed".into())))
            .unwrap();

        let second_outcome = second.settle().await;
        let first_outcome = first.settle().await;

        assert!(matches!(
            second_outcome.persistence_result(),
            Err(CostPersistError::Storage(message)) if message == "second failed"
        ));
        assert!(
            matches!(first_outcome.persistence_result(), Ok(Some(ack)) if ack.journal_revision == 1)
        );
    }

    #[tokio::test]
    async fn an_already_in_flight_second_response_retains_exact_facts_after_first_failure_freezes()
    {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(TestPersistence {
                    requests: requests_tx,
                }),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let scope = tracker.session_scope(session_id);
        let response = |input| CostModelResponse {
            model_ref: ModelRef {
                provider: ProviderId::Anthropic,
                model: "claude-opus-4-6".into(),
            },
            usage: Usage {
                tokens: TokenUsage {
                    input,
                    ..Default::default()
                },
                ..Default::default()
            },
            duration: Duration::from_millis(input),
            retries: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            is_batch_request: false,
            bus: None,
        };

        let first = scope.submit_model_response(response(3));
        let first_request = requests_rx.recv().await.unwrap();
        let second = scope.submit_model_response(response(11));
        let second_mutation_id = second.mutation_id().clone();
        drop(second);
        let preflight = scope.preflight();
        tokio::pin!(preflight);
        tokio::select! {
            biased;
            result = &mut preflight => panic!("preflight bypassed the first unacked mutation: {result:?}"),
            () = tokio::task::yield_now() => {}
        }

        first_request
            .ack
            .send(Err(CostPersistError::Storage("first append failed".into())))
            .unwrap();
        assert!(matches!(
            first.settle().await.persistence_result(),
            Err(CostPersistError::Storage(message)) if message == "first append failed"
        ));

        let retained = loop {
            let retained = scope
                .retained_response(&second_mutation_id)
                .expect("second observation was synchronously retained");
            if retained.settlement.is_some() {
                break retained;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(retained.observation.usage.tokens.input, 11);
        assert!(retained.observation.observed_nano_usd > 0);
        let frozen_reason = tracker
            .durability_gate()
            .frozen_reason()
            .expect("the first durable failure freezes the shared authority");
        assert!(frozen_reason.contains("first append failed"));
        assert!(matches!(
            retained.settlement,
            Some(Err(CostPersistError::Frozen(ref message))) if message == &frozen_reason
        ));
        assert!(matches!(
            preflight.await,
            Err(CostPersistError::Frozen(message)) if message == frozen_reason
        ));
        assert!(
            requests_rx.try_recv().is_err(),
            "a known response queued behind a failed revision must not enqueue a dependent revision"
        );
    }

    #[tokio::test]
    async fn provisional_response_blocks_paid_preflight_until_durable_ack() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_id = SessionId::new();
        let (requests_tx, mut requests_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease: SharedSessionWriterLease = Arc::new(TestLease(session_id.to_string()));
        let tracker = Arc::new(
            CostTracker::new(
                session_id,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_id),
                Arc::new(TestPersistence {
                    requests: requests_tx,
                }),
                lease,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let scope = tracker.session_scope(session_id);
        let receipt = scope.submit_model_response(CostModelResponse {
            model_ref: ModelRef {
                provider: ProviderId::Anthropic,
                model: "claude-opus-4-6".into(),
            },
            usage: Usage {
                tokens: TokenUsage {
                    input: 3,
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
        let preflight = scope.preflight();
        tokio::pin!(preflight);
        tokio::select! {
            biased;
            result = &mut preflight => panic!("preflight bypassed an unacked mutation: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        let request = requests_rx.recv().await.expect("first durable request");

        request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: request.mutation_id.clone(),
                journal_revision: 1,
                cost_revision: request.cost_revision,
            }))
            .unwrap();
        preflight.await.expect("ack releases paid preflight");
        assert!(receipt.settle().await.persistence_result().is_ok());
    }

    #[tokio::test]
    async fn prepared_durable_session_is_inert_until_synchronous_activation() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, _a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let lease_a: SharedSessionWriterLease = Arc::new(TestLease(session_a.to_string()));
        let lease_b: SharedSessionWriterLease = Arc::new(TestLease(session_b.to_string()));
        let gate_b = CostDurabilityGate::default();
        let tracker = Arc::new(
            CostTracker::new(
                session_a,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_a),
                Arc::new(TestPersistence { requests: a_tx }),
                lease_a,
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let mut hydrated_b = hydration(session_b);
        hydrated_b.state.total_nano_usd = 41;

        let prepared = tracker
            .prepare_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydrated_b),
                Arc::new(TestPersistence { requests: b_tx }),
                lease_b.clone(),
                gate_b.clone(),
            )
            .await
            .unwrap();

        assert_eq!(prepared.session_id(), session_b);
        assert_eq!(tracker.session_id().await, session_a);
        assert_eq!(tracker.snapshot().await.session_id, session_a);

        let scope = prepared.activate();
        assert_eq!(tracker.session_id().await, session_b);
        assert_eq!(scope.tracker.snapshot().await.total_nano_usd, 41);
        assert!(
            lingxi_core::host::live_sessions::same_writer_lease_authority(
                &scope.tracker.writer_lease().unwrap(),
                &lease_b
            )
        );
        assert!(scope.tracker.durability_gate().shares_authority(&gate_b));
    }

    #[tokio::test]
    async fn dropped_prepared_session_keeps_a_active_and_reuses_b_without_rereading() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, _a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let persistence_b: Arc<dyn CostPersistence> = Arc::new(TestPersistence { requests: b_tx });
        let lease_b: SharedSessionWriterLease = Arc::new(TestLease(session_b.to_string()));
        let gate_b = CostDurabilityGate::default();
        let tracker = Arc::new(
            CostTracker::new(
                session_a,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_a),
                Arc::new(TestPersistence { requests: a_tx }),
                Arc::new(TestLease(session_a.to_string())),
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let mut hydrated_b = hydration(session_b);
        hydrated_b.state.total_nano_usd = 73;

        let prepared = tracker
            .prepare_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydrated_b),
                persistence_b.clone(),
                lease_b.clone(),
                gate_b.clone(),
            )
            .await
            .unwrap();
        drop(prepared);
        assert_eq!(tracker.session_id().await, session_a);

        let reused = tracker
            .prepare_session_hydrated_with_durable(
                session_b,
                &PanicHydrator,
                persistence_b,
                lease_b,
                gate_b,
            )
            .await
            .unwrap();
        assert_eq!(tracker.session_id().await, session_a);
        let scope = reused.activate();
        assert_eq!(scope.tracker.snapshot().await.total_nano_usd, 73);
    }

    #[tokio::test]
    async fn cancelled_or_failed_durable_prepare_never_moves_active_a() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, _a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let persistence_b: Arc<dyn CostPersistence> = Arc::new(TestPersistence { requests: b_tx });
        let lease_b: SharedSessionWriterLease = Arc::new(TestLease(session_b.to_string()));
        let gate_b = CostDurabilityGate::default();
        let tracker = Arc::new(
            CostTracker::new(
                session_a,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_a),
                Arc::new(TestPersistence { requests: a_tx }),
                Arc::new(TestLease(session_a.to_string())),
                CostDurabilityGate::default(),
            )
            .unwrap(),
        );
        let (blocked, entered) = BlockingHydrator::new(hydration(session_b));
        let preparing_tracker = tracker.clone();
        let preparing_persistence = persistence_b.clone();
        let preparing_lease = lease_b.clone();
        let preparing_gate = gate_b.clone();
        let prepare = tokio::spawn(async move {
            preparing_tracker
                .prepare_session_hydrated_with_durable(
                    session_b,
                    blocked.as_ref(),
                    preparing_persistence,
                    preparing_lease,
                    preparing_gate,
                )
                .await
        });
        entered.await.unwrap();
        prepare.abort();
        assert!(prepare.await.is_err_and(|error| error.is_cancelled()));
        assert_eq!(tracker.session_id().await, session_a);

        let wrong_session = SessionId::new();
        let error = tracker
            .prepare_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydration(wrong_session)),
                persistence_b,
                lease_b,
                gate_b,
            )
            .await
            .expect_err("failed B hydration must not publish an active switch");
        assert!(matches!(error, CostPersistError::Rejected(_)));
        assert_eq!(tracker.session_id().await, session_a);
    }

    #[tokio::test]
    async fn a_to_b_to_a_reuses_the_existing_live_durable_authority() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, mut a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let persistence_a: Arc<dyn CostPersistence> = Arc::new(TestPersistence { requests: a_tx });
        let persistence_b: Arc<dyn CostPersistence> = Arc::new(TestPersistence { requests: b_tx });
        let lease_a: SharedSessionWriterLease = Arc::new(TestLease(session_a.to_string()));
        let lease_b: SharedSessionWriterLease = Arc::new(TestLease(session_b.to_string()));
        let gate_a = CostDurabilityGate::default();
        let gate_b = CostDurabilityGate::default();
        let tracker = Arc::new(
            CostTracker::new(
                session_a,
                Arc::new(PricingCatalog::builtin_reference()),
                legacy_tx,
            )
            .try_with_durable_persistence(
                hydration(session_a),
                persistence_a.clone(),
                lease_a.clone(),
                gate_a.clone(),
            )
            .unwrap(),
        );
        let a1_tracker = tracker.scoped(session_a);
        let a1_scope = CostSessionScope::new(a1_tracker.clone());

        tracker
            .switch_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydration(session_b)),
                persistence_b,
                lease_b,
                gate_b,
            )
            .await
            .unwrap();
        let mut a2_hydration = hydration(session_a);
        a2_hydration.state.total_nano_usd = 100;
        tracker
            .switch_session_hydrated_with_durable(
                session_a,
                &StaticHydrator(a2_hydration),
                persistence_a,
                lease_a.clone(),
                gate_a.clone(),
            )
            .await
            .unwrap();

        let response = || CostModelResponse {
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
        };
        let late_a1 = a1_scope.submit_model_response(response());
        let first_request = a_rx.recv().await.unwrap();
        assert_eq!(first_request.cost_revision, 1);
        let first_total = first_request.state.total_nano_usd;
        first_request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: first_request.mutation_id.clone(),
                journal_revision: 1,
                cost_revision: first_request.cost_revision,
            }))
            .unwrap();
        assert!(late_a1.settle().await.persistence_result().is_ok());

        let active_a2 = tracker.session_scope(session_a);
        active_a2.preflight().await.unwrap();
        let current = active_a2.submit_model_response(response());
        let second_request = a_rx.recv().await.unwrap();
        assert_eq!(second_request.cost_revision, 2);
        assert_eq!(second_request.state.total_nano_usd, first_total * 2);
        second_request
            .ack
            .send(Ok(CostPersistAck {
                mutation_id: second_request.mutation_id.clone(),
                journal_revision: 2,
                cost_revision: second_request.cost_revision,
            }))
            .unwrap();
        assert!(current.settle().await.persistence_result().is_ok());
        assert!(
            lingxi_core::host::live_sessions::same_writer_lease_authority(
                &a1_tracker.writer_lease().unwrap(),
                &lease_a
            )
        );
        assert!(
            lingxi_core::host::live_sessions::same_writer_lease_authority(
                &tracker.writer_lease().unwrap(),
                &lease_a
            )
        );
        assert!(tracker.durability_gate().shares_authority(&gate_a));
    }

    #[tokio::test]
    async fn a_to_b_to_a_rejects_a_second_authority_without_switching_active_session() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, _a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let (other_tx, _other_rx) = tokio::sync::mpsc::unbounded_channel();
        let persistence_a: Arc<dyn CostPersistence> = Arc::new(TestPersistence { requests: a_tx });
        let lease_a: SharedSessionWriterLease = Arc::new(TestLease(session_a.to_string()));
        let gate_a = CostDurabilityGate::default();
        let tracker = CostTracker::new(
            session_a,
            Arc::new(PricingCatalog::builtin_reference()),
            legacy_tx,
        )
        .try_with_durable_persistence(
            hydration(session_a),
            persistence_a.clone(),
            lease_a.clone(),
            gate_a.clone(),
        )
        .unwrap();
        tracker
            .switch_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydration(session_b)),
                Arc::new(TestPersistence { requests: b_tx }),
                Arc::new(TestLease(session_b.to_string())),
                CostDurabilityGate::default(),
            )
            .await
            .unwrap();

        let error = tracker
            .switch_session_hydrated_with_durable(
                session_a,
                &StaticHydrator(hydration(session_a)),
                Arc::new(TestPersistence { requests: other_tx }),
                lease_a.clone(),
                gate_a.clone(),
            )
            .await
            .expect_err("a live session cannot be rebound to a second coordinator");

        assert!(
            matches!(error, CostPersistError::Rejected(message) if message.contains("authority"))
        );
        assert_eq!(tracker.session_id().await, session_b);

        let error = tracker
            .switch_session_hydrated_with_durable(
                session_a,
                &StaticHydrator(hydration(session_a)),
                persistence_a.clone(),
                Arc::new(TestLease(session_a.to_string())),
                gate_a.clone(),
            )
            .await
            .expect_err("a matching session id is not the original writer lease");
        assert!(
            matches!(error, CostPersistError::Rejected(message) if message.contains("authority"))
        );
        assert_eq!(tracker.session_id().await, session_b);

        let error = tracker
            .switch_session_hydrated_with_durable(
                session_a,
                &StaticHydrator(hydration(session_a)),
                persistence_a,
                lease_a,
                CostDurabilityGate::default(),
            )
            .await
            .expect_err("a fresh gate is not the live session freeze authority");
        assert!(
            matches!(error, CostPersistError::Rejected(message) if message.contains("authority"))
        );
        assert_eq!(tracker.session_id().await, session_b);
    }

    #[test]
    fn external_cost_revision_overflow_is_a_byte_for_byte_noop() {
        let mut state = CostState {
            session_id: SessionId::new(),
            cost_revision: u64::MAX,
            total_nano_usd: 17,
            external_nano_usd: 9,
            ..Default::default()
        };
        let original = state.clone();

        assert!(matches!(
            CostTracker::record_external_cost_in_state(&mut state, 5),
            Err(CostPersistError::Storage(message)) if message.contains("overflow")
        ));
        assert_eq!(state, original);
    }

    #[tokio::test]
    async fn record_external_cost_adds_onto_existing_total_and_persists() {
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.record_external_cost(1_000).await;
        let _ = rx.recv().await.unwrap();
        tracker.record_external_cost(2_500).await;
        assert_eq!(
            tracker.total_nano_usd().await,
            3_500,
            "external cost adds onto the existing total, never replaces it"
        );
        let snap = rx.recv().await.unwrap();
        assert_eq!(snap.total_nano_usd, 3_500, "the addition is persisted");
    }

    /// The [`crate::summary::CostTracker::summary`] projection (NOT the
    /// production `/cost`/`/usage` path — see the `record_external_cost` doc
    /// comment) must be able to tell "this total includes externally-priced
    /// Fusion spend with no per-model row" apart from "the per-model
    /// breakdown has silently fallen behind the total" — before this fix a
    /// session with ONLY a Fusion run showed a nonzero `total_nano_usd` and
    /// an empty `by_model` map with nothing distinguishing that from a bug.
    #[tokio::test]
    async fn summary_reconciles_external_cost_against_the_empty_by_model_breakdown() {
        let (tx, _rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.record_external_cost(1_800_000_000).await;
        let summary = tracker.summary().await;
        assert!(
            summary.by_model.is_empty(),
            "no per-model call was recorded"
        );
        assert_eq!(
            summary.session.total_nano_usd, 1_800_000_000,
            "the Fusion run's money reached the session total"
        );
        let by_model_total: u64 = summary.by_model.values().map(|m| m.total_nano_usd).sum();
        assert_eq!(
            by_model_total + summary.session.external_nano_usd,
            summary.session.total_nano_usd,
            "by_model's total plus external_nano_usd must reconcile to the \
session total — before this fix external_nano_usd did not exist and this \
gap had no explanation at all"
        );
        assert_eq!(summary.session.external_nano_usd, 1_800_000_000);
    }

    #[tokio::test]
    async fn record_external_cost_zero_is_a_true_noop() {
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.record_external_cost(0).await;
        assert_eq!(tracker.total_nano_usd().await, 0);
        assert!(
            rx.try_recv().is_err(),
            "a zero-cost commit must not emit a persist snapshot"
        );
    }

    #[tokio::test]
    async fn record_v2_tracks_cache_tokens() {
        // Strict-parity note: the tracker no longer emits a telemetry event —
        // `tengu_cost_recorded` was a port-only event (0 hits in claude-code
        // 2.1.195) and was dropped; the per-request success telemetry is now
        // `tengu_api_success`, fired from the orchestrator. This test asserts
        // the cost ACCOUNTING (return value + per-model cache counters).
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let bus = Arc::new(telemetry::AnalyticsBus::new());

        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let cost = tracker
            .record_api_response_v2(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 1_000,
                        output: 500,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(200),
                0,     // retries
                128,   // cache_read_input_tokens
                64,    // cache_creation_input_tokens
                false, // is_batch_request — ALWAYS false in M3
                Some(&bus),
            )
            .await;

        // 1000 * 5000 + 500 * 25000 = 17_500_000 nano-USD = $0.0175
        assert_eq!(cost, 17_500_000, "returned cost in nano-USD");

        // Snapshot reflects the new cache counters on the per-model entry.
        let snap = rx.recv().await.unwrap();
        assert_eq!(snap.total_nano_usd, 17_500_000);
        let entry = snap.per_model_usage.get(&mr).expect("model entry present");
        assert_eq!(entry.cache_read_input_tokens, 128);
        assert_eq!(entry.cache_creation_input_tokens, 64);
    }

    #[tokio::test]
    async fn record_v2_without_bus_still_updates_state() {
        // No bus → state still updates (the tracker emits no telemetry).
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let cost = tracker
            .record_api_response_v2(
                mr,
                Usage {
                    tokens: TokenUsage {
                        input: 100,
                        output: 50,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
                0,
                0,
                false,
                None, // no bus
            )
            .await;
        assert!(cost > 0);
        let snap = rx.recv().await.unwrap();
        assert!(snap.total_nano_usd > 0);
    }

    #[tokio::test]
    async fn record_v2_unknown_model_bills_default_tier_and_flags() {
        // COST.1 parity: an unknown model is NOT billed at zero. Mirroring
        // claude-code's getModelCosts (`utils/modelCost.ts:155-163`), tokens are
        // billed at the DEFAULT_UNKNOWN_MODEL_COST tier ($5/$25) and the model
        // is flagged in `unpriced_models` so the host can surface the inaccuracy
        // warning. The cost event still fires so dashboards see the call.
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-nonexistent-model".into(),
        };
        let cost = tracker
            .record_api_response_v2(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 100,
                        output: 50,
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
        // 100 * 5_000 + 50 * 25_000 = 500_000 + 1_250_000 = 1_750_000 nano-USD.
        assert_eq!(
            cost, 1_750_000,
            "unknown model bills at the $5/$25 default tier, not zero"
        );
        let snap = rx.recv().await.unwrap();
        assert!(
            snap.unpriced_models.contains(&mr),
            "unknown model is flagged so the inaccuracy warning can be surfaced"
        );
        assert_eq!(snap.total_nano_usd, 1_750_000);
    }

    #[tokio::test]
    async fn legacy_record_api_response_still_works() {
        // M1/M2 callers (e.g. M1 plan 02 cost-tracking code) still compile
        // and behave identically.
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let mr = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        tracker
            .record_api_response(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 1_000,
                        output: 500,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(200),
                0,
            )
            .await;
        let snap = rx.recv().await.unwrap();
        // 17_500_000 nano-USD identical to the existing M1 test.
        assert_eq!(snap.total_nano_usd, 17_500_000);
        // Cache counters default to 0 in the legacy path.
        let entry = snap.per_model_usage.get(&mr).unwrap();
        assert_eq!(entry.cache_read_input_tokens, 0);
        assert_eq!(entry.cache_creation_input_tokens, 0);
    }

    #[tokio::test]
    async fn per_model_usage_preserves_insertion_order() {
        // Byte-parity: claude-code's `cbg` renders "Usage by model:" rows in
        // JS-object insertion (first-seen) order. Our per_model_usage must
        // match — a HashMap would iterate in a per-process-randomized order.
        // Record "zzz-first" before "aaa-second" so neither alphabetical sort
        // nor hash order could coincide with insertion order by accident.
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let first = ModelRef {
            provider: ProviderId::Anthropic,
            model: "zzz-first".into(),
        };
        let second = ModelRef {
            provider: ProviderId::Anthropic,
            model: "aaa-second".into(),
        };
        tracker
            .record_api_response(
                first,
                Usage {
                    tokens: TokenUsage {
                        input: 100,
                        output: 50,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
            )
            .await;
        let _ = rx.recv().await.unwrap();
        tracker
            .record_api_response(
                second,
                Usage {
                    tokens: TokenUsage {
                        input: 200,
                        output: 75,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
            )
            .await;
        let _ = rx.recv().await.unwrap();

        let snap = tracker.snapshot().await;
        let order: Vec<String> = snap
            .per_model_usage
            .keys()
            .map(|m| m.model.clone())
            .collect();
        assert_eq!(
            order,
            vec!["zzz-first".to_string(), "aaa-second".to_string()],
            "per_model_usage must iterate in insertion order, not hash order"
        );
    }

    #[tokio::test]
    async fn reset_zeros_every_cumulative_counter_and_keeps_session_id() {
        // parity 2.1.212: `/clear` → resetCostState (yJe) zeroes the whole cost
        // state so a freshly-cleared session no longer shows the prior total.
        let (tx, mut rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        // Accrue a known model, an unknown model (flags unpriced_models), a web
        // search, and code-change lines so every reset target is non-zero.
        let priced = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-opus-4-6".into(),
        };
        let unknown = ModelRef {
            provider: ProviderId::Anthropic,
            model: "claude-nonexistent-model".into(),
        };
        tracker
            .record_api_response(
                priced,
                Usage {
                    tokens: TokenUsage {
                        input: 1_000,
                        output: 500,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(200),
                0,
            )
            .await;
        let _ = rx.recv().await.unwrap();
        tracker
            .record_api_response(
                unknown.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 10,
                        output: 5,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                1, // a retry — folds into total_api_duration_ms only
            )
            .await;
        let _ = rx.recv().await.unwrap();
        tracker.record_code_change(7, 3).await;

        let before = tracker.snapshot().await;
        assert!(before.total_nano_usd > 0);
        assert!(!before.per_model_usage.is_empty());
        assert!(before.unpriced_models.contains(&unknown));
        assert!(before.total_api_duration_ms > 0);

        tracker.reset().await;

        let snap = tracker.snapshot().await;
        assert_eq!(snap.total_nano_usd, 0);
        assert!(snap.per_model_usage.is_empty());
        assert_eq!(snap.total_api_duration_ms, 0);
        assert_eq!(snap.total_api_duration_without_retries_ms, 0);
        assert_eq!(snap.total_tool_duration_ms, 0);
        assert_eq!(snap.total_lines_added, 0);
        assert_eq!(snap.total_lines_removed, 0);
        assert_eq!(snap.total_web_search_requests, 0);
        assert!(snap.unpriced_models.is_empty());
        // session_id survives the reset (claude-code regenerates it separately).
        assert_eq!(snap.session_id, SessionId::nil());
        assert_eq!(tracker.total_nano_usd().await, 0);
    }

    #[tokio::test]
    async fn record_code_change_accumulates() {
        let (tx, _rx) = mpsc::channel(8);
        let tracker = CostTracker::new(
            SessionId::nil(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.record_code_change(3, 1).await;
        tracker.record_code_change(0, 2).await;
        let snap = tracker.snapshot().await;
        assert_eq!(snap.total_lines_added, 3);
        assert_eq!(snap.total_lines_removed, 3);
    }

    #[tokio::test]
    async fn scoped_session_view_survives_active_switch_and_late_settlement() {
        let (tx, _rx) = mpsc::channel(16);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let tracker = Arc::new(CostTracker::new(
            session_a,
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        ));
        let origin = tracker.scoped(session_a);
        origin.record_external_cost(125).await;

        tracker.switch_session(session_b).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 0);
        origin.record_external_cost(75).await;
        assert_eq!(tracker.total_nano_usd().await, 0);

        tracker.switch_session(session_a).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 200);
        assert_eq!(origin.snapshot().await.session_id, session_a);
    }

    #[tokio::test]
    async fn restore_hydrates_pristine_session_once_without_overwriting_live_spend() {
        let (tx, _rx) = mpsc::channel(16);
        let session = SessionId::new();
        let tracker = CostTracker::new(session, Arc::new(PricingCatalog::builtin_reference()), tx);
        tracker.restore_total_for_session(session, 500).await;
        tracker.restore_total_for_session(session, 900).await;
        assert_eq!(tracker.total_nano_usd().await, 500);

        tracker.record_external_cost(25).await;
        tracker.restore_total_for_session(session, 1_000).await;
        assert_eq!(tracker.total_nano_usd().await, 525);
    }

    #[tokio::test]
    async fn legacy_restore_for_b_never_freezes_active_durable_session_a() {
        let (legacy_tx, _legacy_rx) = mpsc::channel(1);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let (a_tx, _a_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, _b_rx) = tokio::sync::mpsc::unbounded_channel();
        let gate_a = CostDurabilityGate::default();
        let gate_b = CostDurabilityGate::default();
        let tracker = CostTracker::new(
            session_a,
            Arc::new(PricingCatalog::builtin_reference()),
            legacy_tx,
        )
        .try_with_durable_persistence(
            hydration(session_a),
            Arc::new(TestPersistence { requests: a_tx }),
            Arc::new(TestLease(session_a.to_string())),
            gate_a.clone(),
        )
        .unwrap();
        tracker
            .switch_session_hydrated_with_durable(
                session_b,
                &StaticHydrator(hydration(session_b)),
                Arc::new(TestPersistence { requests: b_tx }),
                Arc::new(TestLease(session_b.to_string())),
                gate_b.clone(),
            )
            .await
            .unwrap();
        tracker.switch_session(session_a).await.unwrap();

        tracker.restore_total_for_session(session_b, 500).await;

        tracker.preflight_durable().unwrap();
        assert!(gate_a.frozen_reason().is_none());
        assert!(matches!(
            tracker.scoped(session_b).preflight_durable(),
            Err(CostPersistError::Frozen(message))
                if message.contains("legacy total restore")
        ));
        assert!(gate_b.frozen_reason().is_some());
    }

    #[tokio::test]
    async fn leaving_a_live_session_marks_its_zero_baseline_before_revisit() {
        let (tx, _rx) = mpsc::channel(16);
        let session_a = SessionId::new();
        let session_b = SessionId::new();
        let tracker =
            CostTracker::new(session_a, Arc::new(PricingCatalog::builtin_reference()), tx);
        tracker.record_external_cost(100).await;
        tracker.switch_session(session_b).await.unwrap();
        tracker.restore_total_for_session(session_a, 100).await;
        tracker.switch_session(session_a).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 100);
    }

    #[tokio::test]
    async fn builder_adoption_moves_state_and_its_hydration_marker_once() {
        let (tx, _rx) = mpsc::channel(16);
        let placeholder = SessionId::new();
        let mounted = SessionId::new();
        let tracker = CostTracker::new(
            placeholder,
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.restore_total_for_session(placeholder, 500).await;
        tracker.record_external_cost(25).await;

        tracker.adopt_active_session_for_builder(mounted);
        let adopted = tracker.snapshot().await;
        assert_eq!(adopted.session_id, mounted);
        assert_eq!(adopted.total_nano_usd, 525);

        // The persisted baseline moved with the state. Hydrating the mounted
        // id again must not add the same 500 nano-USD a second time.
        tracker.restore_total_for_session(mounted, 500).await;
        assert_eq!(tracker.total_nano_usd().await, 525);

        // Adoption moves rather than clones: the placeholder no longer owns a
        // duplicate of the mounted session's spend.
        assert_eq!(tracker.scoped(placeholder).total_nano_usd().await, 0);
    }

    #[test]
    #[should_panic(expected = "cannot remap published scoped views")]
    fn builder_adoption_rejects_a_tracker_with_a_published_origin_view() {
        let (tx, _rx) = mpsc::channel(1);
        let placeholder = SessionId::new();
        let tracker = CostTracker::new(
            placeholder,
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        let _published_origin = tracker.scoped(placeholder);

        tracker.adopt_active_session_for_builder(SessionId::new());
    }

    #[tokio::test]
    #[should_panic(expected = "requires one construction-time session")]
    async fn builder_adoption_rejects_a_multi_session_ledger() {
        let (tx, _rx) = mpsc::channel(1);
        let tracker = CostTracker::new(
            SessionId::new(),
            Arc::new(PricingCatalog::builtin_reference()),
            tx,
        );
        tracker.switch_session(SessionId::new()).await.unwrap();

        tracker.adopt_active_session_for_builder(SessionId::new());
    }
}
