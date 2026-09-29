//! Owned admission consumes one existing durability turn. No second ticket is
//! acquired while it is active, and that turn never spans provider transport.
use super::*;
use crate::budget::{AttemptAdmissionError, BoundAttemptBudget, CostBudgetAttempt};
use crate::{AttemptBillingMode, AttemptIntent, AttemptLedger};
use lingxi_core::host::BudgetError;
use tokio::sync::{oneshot, OwnedSemaphorePermit};

enum BeginFailure {
    Denied(BudgetError),
    Fault(CostPersistError),
}

impl From<CostPersistError> for BeginFailure {
    fn from(error: CostPersistError) -> Self {
        Self::Fault(error)
    }
}

impl From<AttemptAdmissionError> for BeginFailure {
    fn from(error: AttemptAdmissionError) -> Self {
        match error {
            AttemptAdmissionError::Denied(error) => Self::Denied(error),
            AttemptAdmissionError::Fault(error) => {
                Self::Fault(CostPersistError::Storage(error.to_string()))
            }
        }
    }
}

impl CostTracker {
    pub(crate) fn begin_budgeted_attempt(
        &self,
        mut intent: AttemptIntent,
        publication: BoundAttemptBudget,
        max_reserved_nano_usd: u64,
        session_limit: Option<u64>,
        profile_permit: OwnedSemaphorePermit,
    ) -> Result<oneshot::Receiver<Result<CostBudgetAttempt, BudgetError>>, CostPersistError> {
        let authority = self.selected_entry();
        self.preflight_authority(&authority)?;
        publication.validate_tracker(self)?;
        publication.capture_output_scope(&mut intent)?;
        if intent.session_id != authority.session_id
            || authority.persistence.is_none()
            || intent.billing_mode != AttemptBillingMode::MeteredAttempts
        {
            return Err(CostPersistError::Rejected(
                "invalid durable attempt authority or billing mode".into(),
            ));
        }
        AttemptLedger::new(authority.session_id)
            .check_intent(&intent)
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            CostPersistError::Rejected("attempt admission requires an async runtime".into())
        })?;
        let mutation = AttemptPersistMutation::Intent(intent.clone());
        let (_, id) = identity(&mutation);
        let lifecycle = Arc::new(AttemptLifecycle::new());
        let (slot, mut turn) = {
            let mut slots = authority
                .attempt_settlements
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if slots.slots.contains_key(&id) {
                return Err(CostPersistError::Rejected(
                    "attempt admission identity already consumed".into(),
                ));
            }
            let generation = slots.next_generation(&authority.durability_gate)?;
            let turn = self
                .register_durable_mutation_for(&authority)?
                .expect("durable authority validated above");
            let slot = Arc::new(AttemptSlot {
                mutation,
                publication_key: Some(publication.key()),
                lifecycle: Some(lifecycle.clone()),
                result: std::sync::Mutex::new(None),
                notify: tokio::sync::Notify::new(),
            });
            slots.slots.insert(id.clone(), slot.clone());
            slots.registration_generation = generation;
            (slot, turn)
        };
        let tracker = Self {
            scope: Some(authority.clone()),
            ..self.clone()
        };
        let gate = authority.durability_gate.clone();
        let (sender, receiver) = oneshot::channel();
        runtime.spawn(async move {
            turn.wait().await;
            let dispatch_intent = intent.clone();
            let owned = tokio::spawn(async move {
                let result = tracker
                    .authorize_attempt_owned(
                        &id,
                        &intent,
                        &publication,
                        max_reserved_nano_usd,
                        session_limit,
                    )
                    .await;
                (result, publication, profile_permit)
            })
            .await;
            match owned {
                Ok((Ok(settled), publication, permit)) => {
                    // End the intent turn BEFORE lease Drop can register the
                    // receipt turn. Both successful and failed delivery keep
                    // lifecycle pending until that synchronous registration.
                    turn.finish();
                    let lease =
                        CostBudgetAttempt::new(&dispatch_intent, publication, permit, lifecycle);
                    drop(sender.send(Ok(lease)));
                    slot.complete(Ok(settled));
                }
                Ok((Err(BeginFailure::Denied(error)), publication, permit)) => {
                    drop(permit);
                    drop(publication);
                    lifecycle.close();
                    *slot
                        .result
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(AttemptSlotOutcome::NotAdmitted(error.clone()));
                    slot.notify.notify_waiters();
                    turn.finish();
                    let _ = sender.send(Err(error));
                }
                failure => {
                    let error = match failure {
                        Ok((Err(BeginFailure::Fault(error)), publication, permit)) => {
                            gate.freeze(error.to_string());
                            drop(permit);
                            drop(publication);
                            error
                        }
                        Err(error) => CostPersistError::Storage(format!(
                            "attempt admission owner failed: {error}"
                        )),
                        _ => unreachable!("successful and denied admissions handled above"),
                    };
                    gate.freeze(error.to_string());
                    lifecycle.close();
                    slot.complete(Err(error.clone()));
                    turn.finish();
                    let _ = sender.send(Err(BudgetError::Internal(error.to_string())));
                }
            }
        });
        Ok(receiver)
    }

    async fn authorize_attempt_owned(
        &self,
        id: &str,
        intent: &AttemptIntent,
        publication: &BoundAttemptBudget,
        max_reserved_nano_usd: u64,
        session_limit: Option<u64>,
    ) -> Result<CostAttemptSettlement, BeginFailure> {
        let authority = self.selected_entry();
        self.preflight_authority(&authority)?;
        let permit = authority
            .persistence
            .as_ref()
            .expect("durable authority retained")
            .acquire_attempt_permit(authority.session_id)
            .await?;
        let (ack, receiver) = oneshot::channel();
        let before = {
            let state = authority.state.read().await;
            publication.authorize_and_enqueue(
                intent,
                max_reserved_nano_usd,
                session_limit,
                &state,
                permit,
                ack,
            )?;
            CostStateVector::from(&*state)
        };
        let acknowledged = receiver.await.map_err(|_| {
            CostPersistError::Storage("attempt intent acknowledgment dropped".into())
        })??;
        validate_ack(id, false, &before, &acknowledged)?;
        if CostStateVector::from(&*authority.state.read().await) != before {
            return Err(CostPersistError::Storage(
                "cost changed outside intent durability turn".into(),
            )
            .into());
        }
        Ok(CostAttemptSettlement {
            persistence: acknowledged.persistence,
            receipt: None,
            applied: acknowledged.applied,
        })
    }
}
