//! Owned attempt handoff. The coordinator is the sole contribution fold;
//! this layer only serializes and publishes its acknowledged current vector.
use super::*;
use crate::{
    AttemptFoldAck, AttemptPersistAck, AttemptPersistMutation, AttemptPersistRequest,
    CostStateVector,
};
mod begin;
#[cfg(test)]
mod begin_tests;

/// Compact stable result; no historical cumulative vector is retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostAttemptSettlement {
    /// Original mutation acknowledgment.
    pub persistence: CostPersistAck,
    /// Checked receipt contribution, absent for a dispatch intent.
    pub receipt: Option<AttemptFoldAck>,
    /// Whether the coordinator applied this mutation on its first submission.
    pub applied: bool,
}

pub(super) struct AttemptSlot {
    mutation: AttemptPersistMutation,
    publication_key: Option<lingxi_core::types::MessageId>,
    result: std::sync::Mutex<Option<AttemptSlotOutcome>>,
    lifecycle: Option<Arc<AttemptLifecycle>>,
    notify: tokio::sync::Notify,
}

/// Slot membership and registration history share one lock. Retiring settled
/// slots may reduce membership, but must never rewind registration history.
#[derive(Default)]
pub(super) struct AttemptRegistry {
    slots: HashMap<String, Arc<AttemptSlot>>,
    registration_generation: u64,
}

impl AttemptRegistry {
    fn next_generation(&self, gate: &CostDurabilityGate) -> Result<u64, CostPersistError> {
        self.registration_generation.checked_add(1).ok_or_else(|| {
            let error =
                CostPersistError::Storage("attempt registration generation exhausted".into());
            gate.freeze(error.to_string());
            error
        })
    }

    pub(super) fn snapshot(&self) -> (u64, Vec<Arc<AttemptSlot>>) {
        (
            self.registration_generation,
            self.slots.values().cloned().collect(),
        )
    }

    pub(super) fn generation(&self) -> u64 {
        self.registration_generation
    }

    pub(super) fn is_idle(&self) -> bool {
        self.slots.values().all(|slot| {
            slot.result.lock().is_ok_and(|result| result.is_some())
                && slot.lifecycle.as_ref().is_none_or(|lifecycle| {
                    lifecycle.closed.load(std::sync::atomic::Ordering::Acquire)
                })
        })
    }
}

#[derive(Clone)]
enum AttemptSlotOutcome {
    Persisted(Result<CostAttemptSettlement, CostPersistError>),
    NotAdmitted(lingxi_core::host::BudgetError),
}

/// No account/lease backedge. Intent acknowledgement ends its durability
/// turn, while shutdown waits until the unique lease registers its receipt.
pub(crate) struct AttemptLifecycle {
    closed: std::sync::atomic::AtomicBool,
    changed: tokio::sync::Notify,
}

impl AttemptLifecycle {
    pub(crate) fn new() -> Self {
        Self {
            closed: std::sync::atomic::AtomicBool::new(false),
            changed: tokio::sync::Notify::new(),
        }
    }

    pub(crate) fn close(&self) {
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        self.changed.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.changed.notified();
            if self.closed.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl AttemptSlot {
    pub(super) async fn wait(&self) -> Result<CostAttemptSettlement, CostPersistError> {
        match self.wait_outcome().await {
            AttemptSlotOutcome::Persisted(result) => result,
            AttemptSlotOutcome::NotAdmitted(error) => {
                Err(CostPersistError::Rejected(error.to_string()))
            }
        }
    }

    pub(super) async fn drain(&self) -> Result<(), CostPersistError> {
        let outcome = self.wait_outcome().await;
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.wait().await;
        }
        match outcome {
            AttemptSlotOutcome::Persisted(result) => result.map(|_| ()),
            AttemptSlotOutcome::NotAdmitted(_) => Ok(()),
        }
    }

    async fn wait_outcome(&self) -> AttemptSlotOutcome {
        loop {
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

    fn complete(&self, result: Result<CostAttemptSettlement, CostPersistError>) {
        *self
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            Some(AttemptSlotOutcome::Persisted(result));
        self.notify.notify_waiters();
    }
}

/// Dropping this waiter never cancels the session-owned mutation or observation.
pub struct CostAttemptReceipt {
    slot: Arc<AttemptSlot>,
}

impl CostAttemptReceipt {
    /// Wait for durable persistence and origin projection publication.
    pub async fn settle(self) -> Result<CostAttemptSettlement, CostPersistError> {
        self.slot.wait().await
    }
}

fn identity(mutation: &AttemptPersistMutation) -> (SessionId, String) {
    match mutation {
        AttemptPersistMutation::Intent(intent) => (
            intent.session_id,
            format!("attempt-intent:{}", intent.attempt_id),
        ),
        AttemptPersistMutation::Receipt(receipt) => (
            receipt.session_id,
            format!(
                "attempt-receipt:{}:{}",
                receipt.attempt_id, receipt.revision
            ),
        ),
    }
}

impl CostTracker {
    /// Retain an immutable mutation and register a drain-visible owned task
    /// synchronously. This does not authorize a send or reserve money/output.
    /// Registered accounting requires a durable host; there is no fallback.
    pub fn submit_attempt_mutation(
        &self,
        mutation: AttemptPersistMutation,
    ) -> Result<CostAttemptReceipt, CostPersistError> {
        self.submit_attempt_inner(mutation, None, None)
    }

    pub(crate) fn submit_budgeted_attempt_receipt(
        &self,
        receipt: crate::AttemptReceipt,
        publication: crate::budget::BoundAttemptBudget,
    ) -> Result<CostAttemptReceipt, CostPersistError> {
        self.submit_budgeted_attempt_with_permit(receipt, publication, None)
    }

    pub(crate) fn submit_budgeted_attempt_with_permit(
        &self,
        receipt: crate::AttemptReceipt,
        publication: crate::budget::BoundAttemptBudget,
        profile_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<CostAttemptReceipt, CostPersistError> {
        publication.validate_tracker(self)?;
        self.submit_attempt_inner(
            AttemptPersistMutation::Receipt(receipt),
            Some(publication),
            profile_permit,
        )
    }

    fn submit_attempt_inner(
        &self,
        mutation: AttemptPersistMutation,
        publication: Option<crate::budget::BoundAttemptBudget>,
        profile_permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<CostAttemptReceipt, CostPersistError> {
        let authority = self.selected_entry();
        let publication_key = publication
            .as_ref()
            .map(crate::budget::BoundAttemptBudget::key);
        let _accepting = authority
            .durability_gate
            .retention_gate()
            .try_pin()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        let (session_id, id) = identity(&mutation);
        if session_id != authority.session_id {
            return Err(CostPersistError::Rejected(
                "attempt and captured tracker session differ".into(),
            ));
        }
        self.validate_authority_shape(&authority)?;
        if authority.persistence.is_none() {
            return Err(CostPersistError::Rejected(
                "attempt accounting requires durable persistence".into(),
            ));
        }
        let mut slots = authority
            .attempt_settlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(slot) = slots.slots.get(&id) {
            if slot.mutation != mutation || slot.publication_key != publication_key {
                let error =
                    CostPersistError::Rejected("attempt submission identity conflict".into());
                authority.durability_gate.freeze(error.to_string());
                return Err(error);
            }
            return Ok(CostAttemptReceipt { slot: slot.clone() });
        }
        let generation = slots.next_generation(&authority.durability_gate)?;
        let slot = Arc::new(AttemptSlot {
            mutation,
            publication_key,
            lifecycle: None,
            result: std::sync::Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        });
        slots.slots.insert(id.clone(), slot.clone());
        slots.registration_generation = generation;
        // No await between retaining the observation and registering its turn.
        let turn = self.register_durable_mutation_for(&authority);
        drop(slots);
        let gate = authority.durability_gate.clone();
        let mut turn = match turn {
            Ok(Some(turn)) => turn,
            Ok(None) => unreachable!("durable authority checked above"),
            Err(error) => {
                gate.freeze(error.to_string());
                slot.complete(Err(error));
                return Ok(CostAttemptReceipt { slot });
            }
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            let error =
                CostPersistError::Rejected("attempt handoff requires an async runtime".into());
            gate.freeze(error.to_string());
            slot.complete(Err(error));
            turn.finish();
            return Ok(CostAttemptReceipt { slot });
        };
        let tracker = Self {
            scope: Some(authority),
            ..self.clone()
        };
        let worker_slot = slot.clone();
        runtime.spawn(async move {
            turn.wait().await;
            let mutation = worker_slot.mutation.clone();
            let result = tokio::spawn(async move {
                // Moves the guard into this scope so it drops at the END of it. The lint sees
                // a `_`-binding with no side effect; the side effect is the Drop deadline.
                #[allow(clippy::no_effect_underscore_binding)]
                let _profile_permit = profile_permit;
                tracker
                    .persist_attempt_owned(id, mutation, publication)
                    .await
            })
            .await
            .map_err(|error| CostPersistError::Storage(format!("attempt owner failed: {error}")))
            .and_then(|result| result);
            if let Err(error) = &result {
                gate.freeze(error.to_string());
            }
            worker_slot.complete(result);
            turn.finish();
        });
        Ok(CostAttemptReceipt { slot })
    }

    async fn persist_attempt_owned(
        &self,
        id: String,
        mutation: AttemptPersistMutation,
        publication: Option<crate::budget::BoundAttemptBudget>,
    ) -> Result<CostAttemptSettlement, CostPersistError> {
        let authority = self.selected_entry();
        self.preflight_authority(&authority)?;
        let persistence = authority
            .persistence
            .as_ref()
            .ok_or_else(|| CostPersistError::Rejected("attempt persistence disappeared".into()))?;
        let permit = persistence
            .acquire_attempt_permit(authority.session_id)
            .await?;
        self.preflight_authority(&authority)?;
        let before = CostStateVector::from(&*authority.state.read().await);
        let receipt_expected = matches!(&mutation, AttemptPersistMutation::Receipt(_));
        let output_receipt = match &mutation {
            AttemptPersistMutation::Receipt(receipt) if publication.is_some() => {
                Some(receipt.clone())
            }
            _ => None,
        };
        let (ack, receiver) = tokio::sync::oneshot::channel();
        permit.enqueue(AttemptPersistRequest {
            session_id: authority.session_id,
            mutation,
            ack,
        })?;
        let acknowledged = receiver
            .await
            .map_err(|_| CostPersistError::Storage("attempt acknowledgment dropped".into()))??;
        validate_ack(&id, receipt_expected, &before, &acknowledged)?;
        let projected = acknowledged.state.clone().try_into_state()?;
        let mut state = authority.state.write().await;
        if CostStateVector::from(&*state) != before {
            return Err(CostPersistError::Storage(
                "cost projection changed outside the attempt durability turn".into(),
            ));
        }
        if let Some(publication) = publication {
            let receipt = output_receipt.as_ref().ok_or_else(|| {
                CostPersistError::Storage("output publication requires a receipt".into())
            })?;
            let output = acknowledged
                .receipt
                .as_ref()
                .ok_or_else(|| {
                    CostPersistError::Storage("output publication has no contribution".into())
                })?
                .contribution
                .output_occupancy;
            publication.publish(receipt, output, &mut state, projected)?;
        } else {
            *state = projected;
        }
        Ok(CostAttemptSettlement {
            persistence: acknowledged.persistence,
            receipt: acknowledged.receipt,
            applied: acknowledged.applied,
        })
    }
}

fn validate_ack(
    id: &str,
    receipt_expected: bool,
    before: &CostStateVector,
    ack: &AttemptPersistAck,
) -> Result<(), CostPersistError> {
    let expected_revision = if ack.applied && receipt_expected {
        before.cost_revision.checked_add(1)
    } else {
        Some(before.cost_revision)
    };
    if ack.persistence.mutation_id.as_str() != id
        || ack.persistence.journal_revision == 0
        || ack.state.session_id != before.session_id
        || Some(ack.state.cost_revision) != expected_revision
        || ack.persistence.cost_revision > ack.state.cost_revision
        || ack.receipt.is_some() != receipt_expected
        || ack
            .receipt
            .as_ref()
            .is_some_and(|receipt| receipt.cost_revision != ack.persistence.cost_revision)
        || (ack.applied && ack.persistence.cost_revision != ack.state.cost_revision)
        || ((!ack.applied || !receipt_expected) && ack.state != *before)
    {
        return Err(CostPersistError::Storage(
            "attempt acknowledgment identity/projection mismatch".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestLease(String);
    impl lingxi_core::host::live_sessions::SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    struct TestPersistence(mpsc::Sender<AttemptPersistRequest>);
    struct TestHydrator;
    #[async_trait::async_trait]
    impl CostHydrator for TestHydrator {
        async fn hydrate(&self, session_id: SessionId) -> Result<CostHydration, CostPersistError> {
            Ok(CostHydration {
                state: CostState {
                    session_id,
                    ..Default::default()
                },
                journal_revision: 0,
                attempt_outputs: Vec::new(),
            })
        }
    }
    #[async_trait::async_trait]
    impl CostPersistence for TestPersistence {
        async fn acquire_permit(
            &self,
            _: SessionId,
        ) -> Result<crate::CostPersistPermit, CostPersistError> {
            Err(CostPersistError::Rejected("ordinary path not used".into()))
        }
        async fn acquire_attempt_permit(
            &self,
            _: SessionId,
        ) -> Result<crate::AttemptPersistPermit, CostPersistError> {
            let permit = self.0.clone().reserve_owned().await.unwrap();
            Ok(crate::AttemptPersistPermit::new(move |request| {
                permit.send(request);
                Ok(())
            }))
        }
    }

    pub(super) fn tracker() -> (CostTracker, mpsc::Receiver<AttemptPersistRequest>) {
        let session = SessionId::new();
        let (legacy, _) = mpsc::channel(1);
        let (requests, receiver) = mpsc::channel(4);
        let tracker = CostTracker::new(session, Arc::new(PricingCatalog::empty()), legacy)
            .with_durable_persistence(
                CostHydration {
                    state: CostState {
                        session_id: session,
                        ..Default::default()
                    },
                    journal_revision: 0,
                    attempt_outputs: Vec::new(),
                },
                Arc::new(TestPersistence(requests)),
                Arc::new(TestLease(session.to_string())),
                CostDurabilityGate::default(),
            );
        (tracker, receiver)
    }

    fn observation(session_id: SessionId) -> AttemptPersistMutation {
        AttemptPersistMutation::Receipt(crate::AttemptReceipt {
            session_id,
            attempt_id: "owned-test".into(),
            revision: 1,
            replaces_revision: None,
            disposition: crate::AttemptDisposition::Unknown,
            token_quote_nano_usd: None,
            usage: Usage::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            api_duration_ms: 0,
            api_duration_without_retries_ms: 0,
        })
    }

    fn acknowledgment(request: &AttemptPersistRequest) -> AttemptPersistAck {
        let (_, id) = identity(&request.mutation);
        AttemptPersistAck {
            persistence: CostPersistAck {
                mutation_id: CostMutationId::new(id),
                journal_revision: 2,
                cost_revision: 1,
            },
            state: CostStateVector::from(&CostState {
                session_id: request.session_id,
                cost_revision: 1,
                total_nano_usd: 42,
                external_nano_usd: 42,
                ..Default::default()
            }),
            receipt: Some(AttemptFoldAck {
                cost_revision: 1,
                last_usage_revision: None,
                contribution: crate::AttemptContribution::default(),
            }),
            applied: true,
        }
    }

    #[tokio::test]
    async fn owned_attempt_dropped_waiter_still_projects_and_duplicate_joins_once() {
        let (tracker, mut requests) = tracker();
        let session = tracker.selected_entry().session_id;
        let mutation = observation(session);
        let first = tracker.submit_attempt_mutation(mutation.clone()).unwrap();
        let duplicate = tracker.submit_attempt_mutation(mutation.clone()).unwrap();
        assert_eq!(
            tracker
                .selected_entry()
                .attempt_settlements
                .lock()
                .unwrap()
                .generation(),
            1,
            "duplicate pending submission must not register another generation"
        );
        drop(first);
        let request = requests.recv().await.unwrap();
        let ack = acknowledgment(&request);
        request.ack.send(Ok(ack.clone())).unwrap();
        assert_eq!(
            duplicate.settle().await.unwrap().persistence,
            ack.persistence
        );
        tracker.drain_owned_settlements().await.unwrap();
        assert_eq!(
            tracker.selected_entry().state.read().await.total_nano_usd,
            42
        );
        assert!(requests.try_recv().is_err());
        let repeated = tracker
            .submit_attempt_mutation(mutation)
            .unwrap()
            .settle()
            .await
            .unwrap();
        assert_eq!(repeated.persistence, ack.persistence);
        assert!(requests.try_recv().is_err());
        assert_eq!(
            tracker
                .selected_entry()
                .attempt_settlements
                .lock()
                .unwrap()
                .generation(),
            1,
            "settled replay must not advance registration generation"
        );
    }

    #[tokio::test]
    async fn owned_attempt_generation_overflow_freezes_without_inserting_or_enqueuing() {
        let (tracker, mut requests) = tracker();
        let authority = tracker.selected_entry();
        let mutation = observation(authority.session_id);
        let receipt = tracker.submit_attempt_mutation(mutation.clone()).unwrap();
        let request = requests.recv().await.unwrap();
        let ack = acknowledgment(&request);
        request.ack.send(Ok(ack)).unwrap();
        receipt.settle().await.unwrap();
        authority
            .attempt_settlements
            .lock()
            .unwrap()
            .registration_generation = u64::MAX;
        // Idempotency does not require a new sequence value, even at the cap.
        tracker
            .submit_attempt_mutation(mutation.clone())
            .unwrap()
            .settle()
            .await
            .unwrap();
        assert!(authority.durability_gate.frozen_reason().is_none());
        let mut fresh = mutation;
        if let AttemptPersistMutation::Receipt(receipt) = &mut fresh {
            receipt.attempt_id = "overflow-new-receipt".into();
        }
        let error = tracker
            .submit_attempt_mutation(fresh)
            .err()
            .expect("new generation must fail closed");
        assert!(error
            .to_string()
            .contains("registration generation exhausted"));
        assert!(authority.durability_gate.frozen_reason().is_some());
        let slots = authority.attempt_settlements.lock().unwrap();
        assert_eq!(slots.generation(), u64::MAX);
        assert_eq!(slots.slots.len(), 1);
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn owned_attempt_bad_ack_freezes_without_projection_and_retains_failure() {
        let (tracker, mut requests) = tracker();
        let mutation = observation(tracker.selected_entry().session_id);
        let receipt = tracker.submit_attempt_mutation(mutation.clone()).unwrap();
        let request = requests.recv().await.unwrap();
        let mut ack = acknowledgment(&request);
        ack.state.session_id = SessionId::new();
        request.ack.send(Ok(ack)).unwrap();
        let error = receipt.settle().await.unwrap_err();
        assert!(tracker.durability_gate().frozen_reason().is_some());
        assert_eq!(tracker.selected_entry().state.read().await.cost_revision, 0);
        assert_eq!(
            tracker
                .submit_attempt_mutation(mutation)
                .unwrap()
                .settle()
                .await
                .unwrap_err(),
            error
        );
        assert!(tracker.drain_owned_settlements().await.is_err());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn owned_attempt_conflict_does_not_replace_original_result() {
        let (tracker, mut requests) = tracker();
        let mutation = observation(tracker.selected_entry().session_id);
        let receipt = tracker.submit_attempt_mutation(mutation.clone()).unwrap();
        let request = requests.recv().await.unwrap();
        let ack = acknowledgment(&request);
        request.ack.send(Ok(ack.clone())).unwrap();
        receipt.settle().await.unwrap();
        let mut conflict = mutation.clone();
        if let AttemptPersistMutation::Receipt(receipt) = &mut conflict {
            receipt.api_duration_ms = 1;
        }
        assert!(tracker.submit_attempt_mutation(conflict).is_err());
        assert_eq!(
            tracker
                .submit_attempt_mutation(mutation)
                .unwrap()
                .settle()
                .await
                .unwrap()
                .persistence,
            ack.persistence
        );
    }

    #[tokio::test]
    async fn owned_attempt_late_a_ack_never_moves_b_and_receipt_retains_no_authority_cycle() {
        let (tracker, mut requests) = tracker();
        let session_a = tracker.selected_entry().session_id;
        let weak_a = Arc::downgrade(&tracker.selected_entry());
        let receipt = tracker
            .submit_attempt_mutation(observation(session_a))
            .unwrap();
        let retained_waiter = tracker
            .submit_attempt_mutation(observation(session_a))
            .unwrap();
        let request = requests.recv().await.unwrap();
        let session_b = SessionId::new();
        let (b_tx, mut b_requests) = mpsc::channel(4);
        tracker
            .switch_session_hydrated_with_durable(
                session_b,
                &TestHydrator,
                Arc::new(TestPersistence(b_tx)),
                Arc::new(TestLease(session_b.to_string())),
                CostDurabilityGate::default(),
            )
            .await
            .unwrap();
        let ack = acknowledgment(&request);
        request.ack.send(Ok(ack)).unwrap();
        receipt.settle().await.unwrap();
        tracker.drain_owned_settlements().await.unwrap();
        assert_eq!(tracker.session_id().await, session_b);
        assert_eq!(
            tracker.selected_entry().state.read().await.total_nano_usd,
            0
        );
        assert_eq!(
            tracker
                .scoped(session_a)
                .selected_entry()
                .state
                .read()
                .await
                .total_nano_usd,
            42
        );
        assert!(b_requests.try_recv().is_err());
        drop(tracker);
        tokio::time::timeout(Duration::from_secs(1), async {
            while weak_a.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a retained result must not keep its session authority alive");
        assert_eq!(
            retained_waiter
                .settle()
                .await
                .unwrap()
                .persistence
                .cost_revision,
            1
        );
    }
}
