use super::*;
use crate::{
    AttemptContribution, AttemptDisposition, AttemptFoldAck, AttemptPersistAck,
    AttemptPersistMutation, AttemptPersistRequest, AttemptReceipt, CostDurabilityGate,
    CostHydration, CostMutationId, CostPersistAck, CostPersistError, CostPersistence, CostState,
    CostStateVector, PricingCatalog, Usage,
};
use platform_api::WorkflowOutputScope;
use protocol::{MessageId, SessionId};
use tokio::sync::mpsc;

struct Lease(String);
impl platform_api::live_sessions::SessionWriterLease for Lease {
    fn session_id(&self) -> &str {
        &self.0
    }
}
struct Persistence(mpsc::Sender<AttemptPersistRequest>);
#[async_trait::async_trait]
impl CostPersistence for Persistence {
    async fn acquire_permit(
        &self,
        _: SessionId,
    ) -> Result<crate::CostPersistPermit, CostPersistError> {
        Err(CostPersistError::Rejected(
            "unused ordinary persistence".into(),
        ))
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

async fn setup() -> (
    Arc<CostTracker>,
    BudgetEnforcer,
    WorkflowOutputScope,
    mpsc::Receiver<AttemptPersistRequest>,
) {
    let session = SessionId::new();
    let (legacy, _) = mpsc::channel(1);
    let (tx, rx) = mpsc::channel(4);
    let tracker = Arc::new(
        CostTracker::new(session, Arc::new(PricingCatalog::empty()), legacy)
            .with_durable_persistence(
                CostHydration {
                    state: CostState {
                        session_id: session,
                        ..Default::default()
                    },
                    journal_revision: 0,
                    attempt_outputs: Vec::new(),
                },
                Arc::new(Persistence(tx)),
                Arc::new(Lease(session.to_string())),
                CostDurabilityGate::default(),
            ),
    );
    let enforcer = BudgetEnforcer::new(
        BudgetConfig {
            max_session_nano_usd: Some(100),
            max_turn_nano_usd: None,
            max_turn_tokens: None,
            warning_thresholds: vec![],
            on_exceed: BudgetExceedPolicy::Halt,
        },
        tracker.clone(),
    );
    let scope = enforcer
        .workflow_output_scopes()
        .begin_turn(session, MessageId::new(), Some(100))
        .await
        .unwrap();
    let state = enforcer.session_state_for(session).await;
    state
        .reservations
        .lock()
        .unwrap()
        .reserve_attempt(
            "settlement",
            attempts::AttemptHold {
                run_id: "run".into(),
                generation: scope.generation_id(),
                nano_usd: 80,
                output_tokens: 20,
            },
            100,
            0,
            Some(100),
        )
        .unwrap();
    (tracker, enforcer, scope, rx)
}

fn receipt(scope: &WorkflowOutputScope) -> AttemptReceipt {
    AttemptReceipt {
        session_id: scope.session_id(),
        attempt_id: "settlement".into(),
        revision: 1,
        replaces_revision: None,
        disposition: AttemptDisposition::Unknown,
        token_quote_nano_usd: None,
        usage: Usage::default(),
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        api_duration_ms: 0,
        api_duration_without_retries_ms: 0,
    }
}

fn ack(request: &AttemptPersistRequest, output: u64) -> AttemptPersistAck {
    AttemptPersistAck {
        persistence: CostPersistAck {
            mutation_id: CostMutationId::new("attempt-receipt:settlement:1"),
            journal_revision: 1,
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
            contribution: AttemptContribution {
                output_occupancy: output,
                ..Default::default()
            },
        }),
        applied: true,
    }
}

#[tokio::test]
async fn budgeted_settlement_dropped_waiter_publishes_before_ordinary_reserve() {
    let (tracker, enforcer, scope, mut requests) = setup().await;
    let observed = receipt(&scope);
    let first = tracker
        .submit_budgeted_attempt_receipt(
            observed.clone(),
            enforcer.bind_attempt_budget(&scope).await.unwrap(),
        )
        .unwrap();
    let joined = tracker
        .submit_budgeted_attempt_receipt(
            observed,
            enforcer.bind_attempt_budget(&scope).await.unwrap(),
        )
        .unwrap();
    drop(first);
    let request = requests.recv().await.unwrap();
    let reserve = enforcer.reserve_nano_usd(58);
    tokio::pin!(reserve);
    tokio::select! { biased; _ = &mut reserve => panic!("reserve overtook receipt publication"), () = tokio::task::yield_now() => {} }
    let acknowledgment = ack(&request, 20);
    request.ack.send(Ok(acknowledgment)).unwrap();
    joined.settle().await.unwrap();
    assert_eq!(scope.spent(), 20);
    assert_eq!(
        tracker
            .selected_state_cell()
            .await
            .read()
            .await
            .total_nano_usd,
        42
    );
    let id = reserve.await.unwrap();
    assert_eq!(enforcer.active_reservation_nano_usd().await, 58);
    assert!(
        requests.try_recv().is_err(),
        "duplicate must join existing mutation"
    );
    enforcer.release_reservation(id).await;
}

#[tokio::test]
async fn budgeted_settlement_invalid_ack_or_output_preserves_projection_and_hold() {
    for invalid_ack in [false, true] {
        let (tracker, enforcer, scope, mut requests) = setup().await;
        let waiter = tracker
            .submit_budgeted_attempt_receipt(
                receipt(&scope),
                enforcer.bind_attempt_budget(&scope).await.unwrap(),
            )
            .unwrap();
        let request = requests.recv().await.unwrap();
        let mut acknowledgment = ack(&request, if invalid_ack { 20 } else { 19 });
        if invalid_ack {
            acknowledgment.persistence.cost_revision = 2;
        }
        request.ack.send(Ok(acknowledgment)).unwrap();
        assert!(waiter.settle().await.is_err());
        assert!(tracker.preflight_durable().is_err());
        assert_eq!(
            tracker
                .selected_state_cell()
                .await
                .read()
                .await
                .total_nano_usd,
            0
        );
        assert_eq!(scope.spent(), 0);
        let session = enforcer.session_state_for(scope.session_id()).await;
        let book = session.reservations.lock().unwrap();
        assert_eq!(book.attempt_holds["settlement"].nano_usd, 80);
        assert_eq!(book.attempt_holds["settlement"].output_tokens, 20);
        assert!(book.output_scopes[&scope.generation_id()]
            .attempts
            .is_empty());
    }
}

#[tokio::test]
async fn budgeted_settlement_late_old_turn_does_not_update_new_turn() {
    let (tracker, enforcer, old, mut requests) = setup().await;
    let observed = receipt(&old);
    let waiter = tracker
        .submit_budgeted_attempt_receipt(
            observed.clone(),
            enforcer.bind_attempt_budget(&old).await.unwrap(),
        )
        .unwrap();
    let request = requests.recv().await.unwrap();
    let new = enforcer
        .workflow_output_scopes()
        .begin_turn(old.session_id(), MessageId::new(), Some(100))
        .await
        .unwrap();
    let acknowledgment = ack(&request, 20);
    request.ack.send(Ok(acknowledgment)).unwrap();
    waiter.settle().await.unwrap();
    assert_eq!(old.spent(), 20);
    assert_eq!(new.spent(), 0);
    assert!(tracker
        .submit_attempt_mutation(AttemptPersistMutation::Receipt(observed))
        .is_err());
}

#[tokio::test]
async fn budgeted_settlement_completed_slot_does_not_retain_account_cycle() {
    let (tracker, enforcer, scope, mut requests) = setup().await;
    let weak = Arc::downgrade(&tracker);
    let session = enforcer.session_state_for(scope.session_id()).await;
    let weak_session = Arc::downgrade(&session);
    drop(session);
    let waiter = tracker
        .submit_budgeted_attempt_receipt(
            receipt(&scope),
            enforcer.bind_attempt_budget(&scope).await.unwrap(),
        )
        .unwrap();
    let request = requests.recv().await.unwrap();
    let acknowledgment = ack(&request, 20);
    request.ack.send(Ok(acknowledgment)).unwrap();
    waiter.settle().await.unwrap();
    drop(scope);
    drop(enforcer);
    drop(tracker);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while weak.upgrade().is_some() || weak_session.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retained settlement slots must not own the bound output account");
}

#[tokio::test]
async fn budgeted_settlement_old_account_reconstruction_keeps_duplicate_identity() {
    let (tracker, enforcer, scope, mut requests) = setup().await;
    let session_id = scope.session_id();
    let generation = scope.generation_id();
    let observed = receipt(&scope);
    let waiter = tracker
        .submit_budgeted_attempt_receipt(
            observed.clone(),
            enforcer.bind_attempt_budget(&scope).await.unwrap(),
        )
        .unwrap();
    let request = requests.recv().await.unwrap();
    let acknowledgment = ack(&request, 20);
    request.ack.send(Ok(acknowledgment)).unwrap();
    let settled = waiter.settle().await.unwrap();
    tracker.drain_owned_settlements().await.unwrap();
    let session = enforcer.session_state_for(session_id).await;
    let weak = session.reservations.lock().unwrap().output_scopes[&generation]
        .owner
        .clone();
    let factory = enforcer.workflow_output_scopes();
    let next = factory
        .begin_turn(session_id, MessageId::new(), Some(100))
        .await
        .unwrap();
    drop(scope);
    assert!(
        weak.upgrade().is_none(),
        "test must actually retire the old allocation"
    );
    let restored = factory
        .begin_turn(session_id, generation, Some(100))
        .await
        .unwrap();
    let duplicate = tracker
        .submit_budgeted_attempt_receipt(
            observed,
            enforcer.bind_attempt_budget(&restored).await.unwrap(),
        )
        .unwrap();
    assert_eq!(duplicate.settle().await.unwrap(), settled);
    assert_eq!(restored.spent(), 20);
    assert_eq!(next.spent(), 0);
    assert!(factory.capture(session_id).unwrap().shares_account(&next));
    assert!(tracker.preflight_durable().is_ok());
    assert!(requests.try_recv().is_err());
}
