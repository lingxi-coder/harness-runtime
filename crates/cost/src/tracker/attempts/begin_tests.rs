use super::*;
use crate::budget::{BudgetConfig, BudgetEnforcer, BudgetExceedPolicy};
use crate::{AttemptDisposition, AttemptIntent, AttemptReceipt};
use platform_api::WorkflowOutputScope;
use protocol::MessageId;
use tokio::sync::Semaphore;

async fn setup(
    output_limit: u64,
) -> (
    Arc<CostTracker>,
    BudgetEnforcer,
    WorkflowOutputScope,
    mpsc::Receiver<AttemptPersistRequest>,
    Arc<Semaphore>,
) {
    let (tracker, requests) = super::tests::tracker();
    let tracker = Arc::new(tracker);
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
        .begin_turn(
            tracker.session_id().await,
            MessageId::new(),
            Some(output_limit),
        )
        .await
        .unwrap();
    (
        tracker,
        enforcer,
        scope,
        requests,
        Arc::new(Semaphore::new(1)),
    )
}

fn intent(session: SessionId, id: &str) -> AttemptIntent {
    let model = crate::ModelRef {
        provider: crate::ProviderId::Anthropic,
        model: "mock".into(),
    };
    AttemptIntent {
        schema_version: 1,
        session_id: session,
        output_scope: None,
        attempt_id: id.into(),
        run_id: "run".into(),
        logical_call_id: id.into(),
        wire_ordinal: 1,
        stage: crate::AttemptStage::Panel,
        panel_slot: Some(0),
        profile_id: "profile".into(),
        model: model.clone(),
        route_revision: 1,
        pricing: crate::ModelPricing {
            model_ref: model,
            token_rates: [
                crate::TokenClass::Input,
                crate::TokenClass::Output,
                crate::TokenClass::CacheRead,
                crate::TokenClass::CacheWrite,
                crate::TokenClass::CacheWrite1h,
                crate::TokenClass::ReasoningOutput,
            ]
            .into_iter()
            .map(|class| {
                (
                    class,
                    crate::MoneyPerToken {
                        nano_usd_per_token: 1,
                    },
                )
            })
            .collect(),
            non_token_rates_nano_usd: Default::default(),
            effective_from: None,
            source: crate::PricingSource::BuiltInReference {
                provider: crate::ProviderId::Anthropic,
            },
        },
        token_pricing_requires_quote: false,
        authorized_nano_usd: 10,
        authorized_input_tokens: 10,
        authorized_output_tokens: 20,
        billing_mode: crate::AttemptBillingMode::MeteredAttempts,
        usage_contract: crate::AttemptUsageContract::AnthropicCacheTtlV1,
    }
}

fn acknowledge(request: AttemptPersistRequest, cost: u64, output: u64) {
    let is_receipt = matches!(&request.mutation, AttemptPersistMutation::Receipt(_));
    let revision = u64::from(is_receipt);
    acknowledge_at_revision(request, cost, output, revision, 1 + revision);
}

fn acknowledge_at_revision(
    request: AttemptPersistRequest,
    cost: u64,
    output: u64,
    revision: u64,
    journal_revision: u64,
) {
    let (_, id) = identity(&request.mutation);
    let is_receipt = matches!(&request.mutation, AttemptPersistMutation::Receipt(_));
    let ack = AttemptPersistAck {
        persistence: CostPersistAck {
            mutation_id: CostMutationId::new(id),
            journal_revision,
            cost_revision: revision,
        },
        state: CostStateVector::from(&CostState {
            session_id: request.session_id,
            cost_revision: revision,
            total_nano_usd: cost,
            external_nano_usd: cost,
            ..Default::default()
        }),
        receipt: is_receipt.then(|| AttemptFoldAck {
            cost_revision: revision,
            last_usage_revision: None,
            contribution: crate::AttemptContribution {
                nano_usd: cost,
                output_occupancy: output,
                ..Default::default()
            },
        }),
        applied: true,
    };
    request.ack.send(Ok(ack)).unwrap();
}

async fn start(
    tracker: &CostTracker,
    enforcer: &BudgetEnforcer,
    scope: &WorkflowOutputScope,
    permits: &Arc<Semaphore>,
    id: &str,
    run_limit: u64,
) -> tokio::sync::oneshot::Receiver<
    Result<crate::budget::CostBudgetAttempt, platform_api::BudgetError>,
> {
    tracker
        .begin_budgeted_attempt(
            intent(scope.session_id(), id),
            enforcer.bind_attempt_budget(scope).await.unwrap(),
            run_limit,
            Some(100),
            permits.clone().acquire_owned().await.unwrap(),
        )
        .unwrap()
}

#[tokio::test]
async fn owned_begin_rejects_a_supplied_output_generation_mismatch() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let mut attempted = intent(scope.session_id(), "mismatch");
    attempted.output_scope = Some(crate::AttemptOutputScope {
        generation_id: protocol::MessageId::new(),
        max_output_tokens: Some(100),
    });
    let result = tracker.begin_budgeted_attempt(
        attempted,
        enforcer.bind_attempt_budget(&scope).await.unwrap(),
        100,
        Some(100),
        permits.clone().acquire_owned().await.unwrap(),
    );
    assert!(result.is_err());
    assert!(requests.try_recv().is_err());
    assert_eq!(permits.available_permits(), 1);
    assert_eq!(scope.spent(), 0);
}

#[tokio::test]
async fn owned_begin_cancel_before_intent_ack_settles_not_sent_once() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let receiver = start(&tracker, &enforcer, &scope, &permits, "cancel", 100).await;
    let request = requests.recv().await.unwrap();
    let AttemptPersistMutation::Intent(captured) = &request.mutation else {
        panic!("intent first")
    };
    assert_eq!(
        captured.output_scope.as_ref().unwrap().generation_id,
        scope.generation_id()
    );
    assert!(matches!(
        &request.mutation,
        AttemptPersistMutation::Intent(_)
    ));
    drop(receiver);
    assert_eq!(permits.available_permits(), 0);
    acknowledge(request, 0, 0);
    let receipt = requests.recv().await.unwrap();
    assert!(
        matches!(&receipt.mutation, AttemptPersistMutation::Receipt(observed) if observed.disposition == AttemptDisposition::ProvenNotSent && observed.usage == Usage::default())
    );
    assert_eq!(permits.available_permits(), 0);
    acknowledge(receipt, 0, 0);
    tracker.drain_owned_settlements().await.unwrap();
    assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    assert_eq!(scope.spent(), 0);
    assert_eq!(permits.available_permits(), 1);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn owned_begin_buffered_success_keeps_drain_pending_until_receiver_drop_and_receipt_ack() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let receiver = start(&tracker, &enforcer, &scope, &permits, "buffered", 100).await;
    acknowledge(requests.recv().await.unwrap(), 0, 0);
    let slot = tracker
        .selected_entry()
        .attempt_settlements
        .lock()
        .unwrap()
        .slots["attempt-intent:buffered"]
        .clone();
    slot.wait().await.unwrap(); // begin has sent the lease, but receiver was never polled
    let drain = tracker.drain_owned_settlements();
    tokio::pin!(drain);
    tokio::select! { biased; _ = &mut drain => panic!("drain ignored buffered lease"), () = tokio::task::yield_now() => {} }
    drop(receiver);
    let receipt = requests.recv().await.unwrap();
    tokio::select! { biased; _ = &mut drain => panic!("drain ignored pending receipt ack"), () = tokio::task::yield_now() => {} }
    acknowledge(receipt, 0, 0);
    drain.await.unwrap();
    assert_eq!(permits.available_permits(), 1);
    assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
}

#[tokio::test]
async fn owned_begin_drain_detects_registration_after_same_length_retirement() {
    use std::future::Future;
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    permits.add_permits(1);
    let first = start(&tracker, &enforcer, &scope, &permits, "first", 100).await;
    acknowledge_at_revision(requests.recv().await.unwrap(), 0, 0, 0, 1);
    let authority = tracker.selected_entry();
    let first_slot =
        authority.attempt_settlements.lock().unwrap().slots["attempt-intent:first"].clone();
    first_slot.wait().await.unwrap();
    let drain = tracker.drain_owned_settlements();
    tokio::pin!(drain);
    let waiting =
        std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx).is_pending()))
            .await;
    assert!(waiting, "drain must capture the first live lease");
    let second = start(&tracker, &enforcer, &scope, &permits, "second", 100).await;
    acknowledge_at_revision(requests.recv().await.unwrap(), 0, 0, 0, 2);
    let second_slot =
        authority.attempt_settlements.lock().unwrap().slots["attempt-intent:second"].clone();
    second_slot.wait().await.unwrap();
    drop(first);
    let receipt = requests.recv().await.unwrap();
    let (_, receipt_id) = identity(&receipt.mutation);
    acknowledge_at_revision(receipt, 0, 0, 1, 3);
    let receipt_slot = authority.attempt_settlements.lock().unwrap().slots[&receipt_id].clone();
    receipt_slot.wait().await.unwrap();
    first_slot.drain().await.unwrap();
    // Retire only completely settled old slots. The second intent has a live
    // buffered lease, so it remains and restores the original map length.
    {
        let mut slots = authority.attempt_settlements.lock().unwrap();
        let generation = slots.generation();
        slots.slots.remove("attempt-intent:first").unwrap();
        slots.slots.remove(&receipt_id).unwrap();
        assert_eq!(slots.slots.len(), 1);
        assert_eq!(
            slots.generation(),
            generation,
            "retirement cannot rewind generation"
        );
    }
    let completed_early =
        std::future::poll_fn(|cx| std::task::Poll::Ready(drain.as_mut().poll(cx).is_ready())).await;
    // Always release/ack the new lease before asserting, including on RED.
    drop(second);
    let receipt = requests.recv().await.unwrap();
    // Both receipts belong to one session projection. Retiring registry slots
    // does not reset its cost revision; the second applied receipt advances 1
    // to 2 even though both ProvenNotSent contributions cost zero.
    acknowledge_at_revision(receipt, 0, 0, 2, 4);
    if !completed_early {
        drain.await.unwrap();
    }
    tracker.drain_owned_settlements().await.unwrap();
    assert!(
        !completed_early,
        "same map length cannot hide a new registered lease from drain"
    );
    assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    assert_eq!(permits.available_permits(), 2);
}

#[tokio::test]
async fn owned_begin_capacity_denial_leaves_session_healthy_and_next_attempt_usable() {
    for (output_limit, run_limit, ordinary) in [(100, 100, 95), (10, 100, 0), (100, 5, 0)] {
        let (tracker, enforcer, scope, mut requests, permits) = setup(output_limit).await;
        let hold = enforcer.reserve_nano_usd(ordinary).await.unwrap();
        let denied = start(&tracker, &enforcer, &scope, &permits, "denied", run_limit).await;
        assert!(denied.await.unwrap().is_err());
        tracker.drain_owned_settlements().await.unwrap();
        assert!(tracker.preflight_durable().is_ok());
        assert!(requests.try_recv().is_err());
        assert_eq!(enforcer.active_reservation_nano_usd().await, ordinary);
        enforcer.release_reservation(hold).await;
        let mut next = intent(scope.session_id(), "next");
        next.authorized_output_tokens = 5;
        let receiver = tracker
            .begin_budgeted_attempt(
                next,
                enforcer.bind_attempt_budget(&scope).await.unwrap(),
                100,
                Some(100),
                permits.clone().acquire_owned().await.unwrap(),
            )
            .unwrap();
        acknowledge(requests.recv().await.unwrap(), 0, 0);
        drop(receiver.await.unwrap().unwrap());
        acknowledge(requests.recv().await.unwrap(), 0, 0);
        tracker.drain_owned_settlements().await.unwrap();
        assert_eq!(permits.available_permits(), 1);
        assert!(tracker.preflight_durable().is_ok());
    }
}

#[tokio::test]
async fn owned_begin_generation_overflow_releases_permit_without_hold_or_intent() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let authority = tracker.selected_entry();
    authority
        .attempt_settlements
        .lock()
        .unwrap()
        .registration_generation = u64::MAX;
    let result = tracker.begin_budgeted_attempt(
        intent(scope.session_id(), "overflow-begin"),
        enforcer.bind_attempt_budget(&scope).await.unwrap(),
        100,
        Some(100),
        permits.clone().acquire_owned().await.unwrap(),
    );
    let error = result.err().expect("new begin generation must fail closed");
    assert!(error
        .to_string()
        .contains("registration generation exhausted"));
    assert!(authority.durability_gate.frozen_reason().is_some());
    assert_eq!(permits.available_permits(), 1);
    assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    assert!(requests.try_recv().is_err());
    let slots = authority.attempt_settlements.lock().unwrap();
    assert_eq!(slots.generation(), u64::MAX);
    assert!(slots.slots.is_empty());
}

#[tokio::test]
async fn owned_begin_ack_failure_preserves_hold_and_prevents_dispatch() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let receiver = start(&tracker, &enforcer, &scope, &permits, "failure", 100).await;
    requests
        .recv()
        .await
        .unwrap()
        .ack
        .send(Err(CostPersistError::Storage(
            "injected intent failure".into(),
        )))
        .unwrap();
    assert!(receiver.await.unwrap().is_err());
    assert!(tracker.drain_owned_settlements().await.is_err());
    assert_eq!(enforcer.active_reservation_nano_usd().await, 10);
    assert_eq!(scope.spent(), 0);
    assert_eq!(permits.available_permits(), 1);
    assert!(requests.try_recv().is_err());
}

#[tokio::test]
async fn owned_begin_dispatched_drop_retains_partial_usage_and_profile_until_ack() {
    let (tracker, enforcer, scope, mut requests, permits) = setup(100).await;
    let receiver = start(&tracker, &enforcer, &scope, &permits, "partial", 100).await;
    acknowledge(requests.recv().await.unwrap(), 0, 0);
    let mut lease = receiver.await.unwrap().unwrap();
    lease.mark_dispatched().unwrap();
    let observed = AttemptReceipt {
        session_id: scope.session_id(),
        attempt_id: "partial".into(),
        revision: 1,
        replaces_revision: None,
        disposition: AttemptDisposition::Unknown,
        token_quote_nano_usd: None,
        usage: Usage {
            tokens: crate::TokenUsage {
                input: 7,
                output: 3,
                ..Default::default()
            },
            ..Default::default()
        },
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
        api_duration_ms: 9,
        api_duration_without_retries_ms: 9,
    };
    lease.observe(observed.clone()).unwrap();
    drop(lease);
    let receipt = requests.recv().await.unwrap();
    assert_eq!(receipt.mutation, AttemptPersistMutation::Receipt(observed));
    assert_eq!(permits.available_permits(), 0);
    acknowledge(receipt, 10, 20);
    tracker.drain_owned_settlements().await.unwrap();
    assert_eq!(enforcer.active_reservation_nano_usd().await, 0);
    assert_eq!(scope.spent(), 20);
    assert_eq!(tracker.total_nano_usd().await, 10);
    assert_eq!(permits.available_permits(), 1);
}
