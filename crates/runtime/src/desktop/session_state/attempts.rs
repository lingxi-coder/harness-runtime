//! Attempt events share the session WAL and single mutation order. Only the
//! bounded current model vector is staged; attempt history is never cloned.
use super::*;
use std::collections::HashMap;

pub(super) struct AttemptProjection {
    ledger: AttemptLedger,
    acks: HashMap<String, CostPersistAck>,
    failures: HashMap<String, (SessionId, AttemptPersistMutation, CostPersistError)>,
    #[cfg(test)]
    fail_next_append_ack: bool,
}

fn intent_id(intent: &AttemptIntent) -> String {
    format!("attempt-intent:{}", intent.attempt_id)
}

fn receipt_id(receipt: &AttemptReceipt) -> String {
    format!(
        "attempt-receipt:{}:{}",
        receipt.attempt_id, receipt.revision
    )
}

fn fold_error(error: cost::AttemptFoldError) -> CostPersistError {
    CostPersistError::Storage(error.to_string())
}

impl AttemptProjection {
    pub(super) fn output_recovery(&self) -> Vec<cost::AttemptOutputRecovery> {
        self.ledger.output_recovery()
    }

    pub(super) fn new(session_id: SessionId) -> Self {
        Self {
            ledger: AttemptLedger::new(session_id),
            acks: HashMap::new(),
            failures: HashMap::new(),
            #[cfg(test)]
            fail_next_append_ack: false,
        }
    }

    pub(super) fn replay(
        &mut self,
        event: SessionEvent,
        event_id: &str,
        journal_revision: u64,
        latest: &mut CostState,
    ) -> Result<(), CostPersistError> {
        if self.acks.contains_key(event_id) {
            return Err(CostPersistError::Storage(
                "duplicate physical attempt event".into(),
            ));
        }
        match event {
            SessionEvent::AttemptIntent(intent) => {
                if intent_id(&intent) != event_id {
                    return Err(CostPersistError::Storage(
                        "attempt intent event id mismatch".into(),
                    ));
                }
                self.ledger.record_intent(intent).map_err(fold_error)?;
            }
            SessionEvent::AttemptReceipt(receipt, persisted) => {
                if receipt_id(&receipt) != event_id {
                    return Err(CostPersistError::Storage(
                        "attempt receipt event id mismatch".into(),
                    ));
                }
                let mut current = CostStateVector::from(&*latest);
                let prepared = self
                    .ledger
                    .prepare_receipt(&current, receipt)
                    .map_err(fold_error)?;
                if prepared.is_duplicate() || prepared.state() != &persisted {
                    return Err(CostPersistError::Storage(
                        "attempt receipt projection disagrees with validated fold".into(),
                    ));
                }
                self.ledger
                    .commit_receipt(&mut current, prepared)
                    .map_err(fold_error)?;
                *latest = current.try_into_state()?;
            }
            _ => {
                return Err(CostPersistError::Storage(
                    "non-attempt event in attempt fold".into(),
                ));
            }
        }
        self.acks.insert(
            event_id.into(),
            CostPersistAck {
                mutation_id: CostMutationId::new(event_id),
                journal_revision,
                cost_revision: latest.cost_revision,
            },
        );
        Ok(())
    }
}

impl CoordinatorState {
    pub(super) fn persist_attempt(
        &self,
        session_id: SessionId,
        mutation: AttemptPersistMutation,
    ) -> Result<AttemptPersistAck, CostPersistError> {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let id = match &mutation {
            AttemptPersistMutation::Intent(intent) => intent_id(intent),
            AttemptPersistMutation::Receipt(receipt) => receipt_id(receipt),
        };
        if let Some((previous_session, previous, error)) = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failures
            .get(&id)
        {
            return Err(
                if previous_session == &session_id && previous == &mutation {
                    error.clone()
                } else {
                    CostPersistError::Storage("failed attempt identity conflict".into())
                },
            );
        }
        let result = self.persist_attempt_locked(session_id, mutation.clone());
        if let Err(error) = &result {
            self.durability_gate.freeze(error.to_string());
            let mut attempts = self
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !attempts.acks.contains_key(&id) {
                attempts
                    .failures
                    .insert(id, (session_id, mutation, error.clone()));
            }
        }
        result
    }

    fn persist_attempt_locked(
        &self,
        session_id: SessionId,
        mutation: AttemptPersistMutation,
    ) -> Result<AttemptPersistAck, CostPersistError> {
        if session_id != self.session_id {
            return Err(CostPersistError::Rejected(
                "attempt belongs to a different session".into(),
            ));
        }
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut current = {
            let projection = self
                .projection
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (state, _) = projection.latest.as_ref().ok_or_else(|| {
                CostPersistError::Rejected("attempt requires hydrated session".into())
            })?;
            CostStateVector::from(state)
        };
        let (id, event, prepared, applied) = match mutation {
            AttemptPersistMutation::Intent(intent) => {
                let applied = attempts.ledger.check_intent(&intent).map_err(fold_error)?;
                (
                    intent_id(&intent),
                    SessionEvent::AttemptIntent(intent),
                    None,
                    applied,
                )
            }
            AttemptPersistMutation::Receipt(receipt) => {
                let prepared = attempts
                    .ledger
                    .prepare_receipt(&current, receipt.clone())
                    .map_err(fold_error)?;
                let applied = !prepared.is_duplicate();
                (
                    receipt_id(&receipt),
                    SessionEvent::AttemptReceipt(receipt, prepared.state().clone()),
                    Some(prepared),
                    applied,
                )
            }
        };
        let receipt = prepared.as_ref().map(|prepared| prepared.ack().clone());
        if !applied {
            let persistence = attempts.acks.get(&id).cloned().ok_or_else(|| {
                CostPersistError::Storage("attempt duplicate has no durable acknowledgment".into())
            })?;
            return Ok(AttemptPersistAck {
                persistence,
                state: current,
                receipt,
                applied: false,
            });
        }
        if let Some(reason) = self.durability_gate.frozen_reason() {
            return Err(CostPersistError::Frozen(reason));
        }
        let encoded = encode_session_event(&event)?;
        let append = self
            .journal
            .append_once(&id, &encoded)
            .map_err(map_journal_error)?;
        if append.duplicate {
            return Err(CostPersistError::Storage(
                "durable attempt event is missing from the hydrated projection".into(),
            ));
        }
        // Model an acknowledged-write failure after the complete line exists,
        // the same ambiguous state as a failed append fsync. Never publish it.
        #[cfg(test)]
        if std::mem::take(&mut attempts.fail_next_append_ack) {
            return Err(CostPersistError::Storage(
                "injected attempt acknowledgment failure".into(),
            ));
        }
        match event {
            SessionEvent::AttemptIntent(intent) => {
                attempts.ledger.record_intent(intent).map_err(fold_error)?;
            }
            SessionEvent::AttemptReceipt(_, _) => {
                attempts
                    .ledger
                    .commit_receipt(&mut current, prepared.expect("receipt was prepared"))
                    .map_err(fold_error)?;
            }
            _ => unreachable!("only attempt events constructed"),
        }
        let persistence = CostPersistAck {
            mutation_id: CostMutationId::new(&id),
            journal_revision: append.journal_revision,
            cost_revision: current.cost_revision,
        };
        attempts.acks.insert(id, persistence.clone());
        self.projection
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .latest = Some((current.clone().try_into_state()?, append.journal_revision));
        drop(attempts);
        self.note_durable_append(append.journal_revision);
        Ok(AttemptPersistAck {
            persistence,
            state: current,
            receipt,
            applied: true,
        })
    }

    /// Called only by startup hydration while holding the session mutation gate.
    pub(super) fn recover_attempts(&self) -> Result<(), CostPersistError> {
        let receipts = {
            let attempts = self
                .attempts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            attempts
                .ledger
                .pending_intent_ids()
                .into_iter()
                .map(|id| attempts.ledger.recovery_receipt(id).map_err(fold_error))
                .collect::<Result<Vec<_>, _>>()?
        };
        for receipt in receipts.into_iter().flatten() {
            self.persist_attempt_locked(self.session_id, AttemptPersistMutation::Receipt(receipt))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cost::{
        AttemptBillingMode, AttemptDisposition, AttemptStage, ModelPricing, ModelRef,
        MoneyPerToken, PricingSource, ProviderId, TokenClass,
    };

    struct TestLease(String);
    impl lingxi_core::host::live_sessions::SessionWriterLease for TestLease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    fn open(root: &Path, session: SessionId) -> Arc<SessionStateCoordinator> {
        SessionStateCoordinator::open(root, session, Arc::new(TestLease(session.to_string())))
            .unwrap()
    }

    fn intent(session_id: SessionId, id: &str) -> AttemptIntent {
        let model = ModelRef {
            provider: ProviderId::Anthropic,
            model: "test-model".into(),
        };
        AttemptIntent {
            token_pricing_requires_quote: false,
            schema_version: 1,
            session_id,
            output_scope: None,
            attempt_id: id.into(),
            run_id: "test-run".into(),
            logical_call_id: id.into(),
            wire_ordinal: 1,
            stage: AttemptStage::Panel,
            panel_slot: Some(0),
            profile_id: "test-profile".into(),
            model: model.clone(),
            route_revision: 1,
            pricing: ModelPricing {
                model_ref: model,
                token_rates: [
                    TokenClass::Input,
                    TokenClass::Output,
                    TokenClass::CacheRead,
                    TokenClass::CacheWrite,
                    TokenClass::CacheWrite1h,
                    TokenClass::ReasoningOutput,
                ]
                .into_iter()
                .map(|class| {
                    (
                        class,
                        MoneyPerToken {
                            nano_usd_per_token: 2,
                        },
                    )
                })
                .collect(),
                non_token_rates_nano_usd: Default::default(),
                effective_from: None,
                source: PricingSource::BuiltInReference {
                    provider: ProviderId::Anthropic,
                },
            },
            authorized_nano_usd: 1000,
            authorized_input_tokens: 100,
            authorized_output_tokens: 200,
            billing_mode: AttemptBillingMode::MeteredAttempts,
            usage_contract: cost::AttemptUsageContract::AnthropicCacheTtlV1,
        }
    }

    fn receipt(intent: &AttemptIntent) -> AttemptReceipt {
        AttemptReceipt {
            token_quote_nano_usd: None,
            session_id: intent.session_id,
            attempt_id: intent.attempt_id.clone(),
            revision: 1,
            replaces_revision: None,
            disposition: AttemptDisposition::Unknown,
            usage: Default::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            api_duration_ms: 0,
            api_duration_without_retries_ms: 0,
        }
    }

    #[test]
    fn attempt_wal_duplicate_returns_original_ack_and_current_projection() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let coordinator = open(root.path(), session);
        coordinator.hydrate_blocking().unwrap();
        let first = intent(session, "first");
        let intent_ack = coordinator
            .state
            .persist_attempt(session, AttemptPersistMutation::Intent(first.clone()))
            .unwrap();
        assert_eq!(intent_ack.persistence.cost_revision, 0);
        let first_receipt = receipt(&first);
        let first_ack = coordinator
            .state
            .persist_attempt(
                session,
                AttemptPersistMutation::Receipt(first_receipt.clone()),
            )
            .unwrap();
        let second = intent(session, "second");
        coordinator
            .state
            .persist_attempt(session, AttemptPersistMutation::Intent(second.clone()))
            .unwrap();
        let second_ack = coordinator
            .state
            .persist_attempt(session, AttemptPersistMutation::Receipt(receipt(&second)))
            .unwrap();
        let duplicate = coordinator
            .state
            .persist_attempt(
                session,
                AttemptPersistMutation::Receipt(first_receipt.clone()),
            )
            .unwrap();
        assert!(!duplicate.applied);
        assert_eq!(duplicate.persistence, first_ack.persistence);
        assert_eq!(duplicate.state, second_ack.state);
        // Two dispatched attempts, neither with a usage report: nothing
        // realized, both authorizations disclosed as unverified.
        assert_eq!(duplicate.state.total_nano_usd, 0);
        assert_eq!(duplicate.state.unverified_nano_usd, 2000);
        drop(coordinator);
        let reopened = open(root.path(), session);
        assert_eq!(
            reopened
                .hydrate_blocking()
                .unwrap()
                .state
                .unverified_nano_usd,
            2000
        );
        let duplicate = reopened
            .state
            .persist_attempt(session, AttemptPersistMutation::Receipt(first_receipt))
            .unwrap();
        assert_eq!(duplicate.persistence, first_ack.persistence);
        assert_eq!(duplicate.state, second_ack.state);
        assert!(!duplicate.applied);
    }

    #[tokio::test]
    async fn attempt_startup_recovers_intent_once_without_replaying_provider() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let coordinator = open(root.path(), session);
        coordinator.hydrate_blocking().unwrap();
        let mut authorization = intent(session, "interrupted");
        let generation = lingxi_core::types::MessageId::new();
        authorization.output_scope = Some(cost::AttemptOutputScope {
            generation_id: generation,
            max_output_tokens: Some(100),
        });
        coordinator
            .state
            .persist_attempt(
                session,
                AttemptPersistMutation::Intent(authorization.clone()),
            )
            .unwrap();
        // A live hydration must not manufacture a receipt for in-flight work.
        assert_eq!(
            coordinator
                .hydrate_blocking()
                .unwrap()
                .state
                .unverified_nano_usd,
            0
        );
        drop(coordinator);
        let reopened = open(root.path(), session);
        let worker = reopened.start().await.unwrap();
        let recovered = reopened.hydrate_blocking().unwrap();
        assert_eq!(recovered.state.total_nano_usd, 0);
        assert_eq!(recovered.state.unverified_nano_usd, 1000);
        assert_eq!(recovered.state.cost_revision, 1);
        assert_eq!(recovered.attempt_outputs.len(), 1);
        assert_eq!(recovered.attempt_outputs[0].scope.generation_id, generation);
        assert_eq!(
            recovered.attempt_outputs[0].current.disposition,
            AttemptDisposition::Unknown
        );
        assert_eq!(
            recovered.attempt_outputs[0].current.output,
            authorization.authorized_output_tokens
        );
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(reopened);
        let reopened = open(root.path(), session);
        let worker = reopened.start().await.unwrap();
        let again = reopened.hydrate_blocking().unwrap();
        assert_eq!(again.state.cost_revision, 1);
        assert_eq!(again.attempt_outputs, recovered.attempt_outputs);
        reopened.close_and_drain().await.unwrap();
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn attempt_dropped_ack_waiter_still_persists_and_foreign_session_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let coordinator = open(root.path(), session);
        let worker = coordinator.start().await.unwrap();
        assert!(
            coordinator
                .acquire_attempt_permit(SessionId::new())
                .await
                .is_err()
        );
        let authorization = intent(session, "detached");
        let (ack, receiver) = tokio::sync::oneshot::channel();
        coordinator
            .acquire_attempt_permit(session)
            .await
            .unwrap()
            .enqueue(AttemptPersistRequest {
                session_id: session,
                mutation: AttemptPersistMutation::Intent(authorization.clone()),
                ack,
            })
            .unwrap();
        drop(receiver);
        let (ack, receiver) = tokio::sync::oneshot::channel();
        coordinator
            .acquire_attempt_permit(session)
            .await
            .unwrap()
            .enqueue(AttemptPersistRequest {
                session_id: session,
                mutation: AttemptPersistMutation::Receipt(receipt(&authorization)),
                ack,
            })
            .unwrap();
        let acked = receiver.await.unwrap().unwrap();
        assert_eq!(acked.state.total_nano_usd, 0);
        assert_eq!(acked.state.unverified_nano_usd, 1000);
        coordinator.close_and_drain().await.unwrap();
        worker.await.unwrap();
        assert_eq!(
            coordinator.hydrate_blocking().unwrap().state.cost_revision,
            1
        );
    }

    #[test]
    fn attempt_wal_rejects_forged_projection_and_conflicting_intent() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let coordinator = open(root.path(), session);
        coordinator.hydrate_blocking().unwrap();
        let authorization = intent(session, "invalid");
        let original = coordinator
            .state
            .persist_attempt(
                session,
                AttemptPersistMutation::Intent(authorization.clone()),
            )
            .unwrap();
        let mut conflict = authorization.clone();
        conflict.authorized_nano_usd += 1;
        assert!(
            coordinator
                .state
                .persist_attempt(session, AttemptPersistMutation::Intent(conflict))
                .is_err()
        );
        let duplicate = coordinator
            .state
            .persist_attempt(
                session,
                AttemptPersistMutation::Intent(authorization.clone()),
            )
            .unwrap();
        assert_eq!(duplicate.persistence, original.persistence);
        assert!(!duplicate.applied);
        let observed = receipt(&authorization);
        let fake = CostStateVector::from(&CostState {
            session_id: session,
            cost_revision: 1,
            ..Default::default()
        });
        coordinator
            .state
            .journal
            .append_once(
                receipt_id(&observed),
                &encode_session_event(&SessionEvent::AttemptReceipt(observed, fake)).unwrap(),
            )
            .unwrap();
        assert!(open(root.path(), session).hydrate_blocking().is_err());
    }

    #[tokio::test]
    async fn attempt_failed_ack_freezes_already_queued_receipts_and_keeps_error_stable() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionId::new();
        let coordinator = open(root.path(), session);
        let worker = coordinator.start().await.unwrap();
        let a = intent(session, "a");
        let b = intent(session, "b");
        coordinator
            .state
            .persist_attempt(session, AttemptPersistMutation::Intent(a.clone()))
            .unwrap();
        coordinator
            .state
            .persist_attempt(session, AttemptPersistMutation::Intent(b.clone()))
            .unwrap();
        let first = coordinator.acquire_attempt_permit(session).await.unwrap();
        let second = coordinator.acquire_attempt_permit(session).await.unwrap();
        let ordinary = coordinator.acquire_permit(session).await.unwrap();
        let (first_ack, first_rx) = tokio::sync::oneshot::channel();
        let (second_ack, second_rx) = tokio::sync::oneshot::channel();
        let (ordinary_ack, ordinary_rx) = tokio::sync::oneshot::channel();
        {
            let _gate = coordinator.state.mutation_gate.lock().unwrap();
            coordinator
                .state
                .attempts
                .lock()
                .unwrap()
                .fail_next_append_ack = true;
            first
                .enqueue(AttemptPersistRequest {
                    session_id: session,
                    mutation: AttemptPersistMutation::Receipt(receipt(&a)),
                    ack: first_ack,
                })
                .unwrap();
            second
                .enqueue(AttemptPersistRequest {
                    session_id: session,
                    mutation: AttemptPersistMutation::Receipt(receipt(&b)),
                    ack: second_ack,
                })
                .unwrap();
            ordinary
                .enqueue(CostPersistRequest {
                    session_id: session,
                    cost_revision: 1,
                    mutation_id: CostMutationId::new("ordinary-after-failed-attempt"),
                    state: CostStateVector::from(&CostState {
                        session_id: session,
                        cost_revision: 1,
                        total_nano_usd: 25,
                        ..Default::default()
                    }),
                    source: CostMutationSource::Administrative,
                    ack: ordinary_ack,
                })
                .unwrap();
        }
        let failure = first_rx.await.unwrap().unwrap_err();
        assert!(matches!(
            second_rx.await.unwrap(),
            Err(CostPersistError::Frozen(_))
        ));
        assert!(matches!(
            ordinary_rx.await.unwrap(),
            Err(CostPersistError::Frozen(_))
        ));
        assert!(
            coordinator
                .state
                .journal
                .find_event_durable("ordinary-after-failed-attempt")
                .unwrap()
                .is_none()
        );
        assert!(coordinator.hydrate_blocking().is_err());
        assert_eq!(
            coordinator
                .state
                .persist_attempt(session, AttemptPersistMutation::Receipt(receipt(&a)))
                .unwrap_err(),
            failure
        );
        assert!(
            coordinator
                .state
                .journal
                .find_event_durable(&receipt_id(&receipt(&b)))
                .unwrap()
                .is_none()
        );
        coordinator.close_and_drain().await.unwrap();
        worker.await.unwrap();
        drop(coordinator);
        // A fresh authority may recover the real complete WAL line. It must
        // not encounter B computed against A's stale pre-commit vector.
        let reopened = open(root.path(), session);
        let recovered = reopened.hydrate_blocking().unwrap();
        assert_eq!(recovered.state.cost_revision, 1);
        assert_eq!(recovered.state.total_nano_usd, 0);
        assert_eq!(recovered.state.unverified_nano_usd, 1000);
    }
}
