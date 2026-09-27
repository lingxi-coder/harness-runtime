//! Combined money/output transitions in the existing session book. These
//! operations perform no I/O and grant no wire authority on their own. The
//! host must enqueue the intent and publish acknowledged receipts in the
//! same owned durability turn as these transitions.
use super::*;
use crate::{AttemptDisposition, AttemptReceipt};
use platform_api::BudgetError;
use protocol::MessageId;

/// Temporary publication owner only. Retained mutation slots keep its opaque
/// key, never this account/tracker owner (which would form a reference cycle).
pub(crate) struct BoundAttemptBudget {
    account: Arc<super::output::BudgetOutputAccount>,
    _retention_pin: platform_api::SessionRetentionPin,
}

impl BoundAttemptBudget {
    pub(super) fn new(
        account: Arc<super::output::BudgetOutputAccount>,
    ) -> Result<Self, BudgetError> {
        let pin = account
            .tracker
            .durability_gate()
            .retention_gate()
            .try_pin()
            .map_err(|error| BudgetError::Internal(error.to_string()))?;
        Ok(Self {
            account,
            _retention_pin: pin,
        })
    }

    pub(crate) fn key(&self) -> MessageId {
        self.account.binding_id
    }

    pub(crate) fn tracker(&self) -> Arc<CostTracker> {
        self.account.tracker.clone()
    }

    pub(crate) fn capture_output_scope(
        &self,
        intent: &mut crate::AttemptIntent,
    ) -> Result<(), crate::CostPersistError> {
        let book = self
            .account
            .session
            .lock_reservations(&self.account.tracker.durability_gate())
            .map_err(|error| crate::CostPersistError::Rejected(error.to_string()))?;
        let state = book
            .output_scopes
            .get(&self.account.generation_id)
            .ok_or_else(|| {
                crate::CostPersistError::Rejected("attempt output scope disappeared".into())
            })?;
        let captured = crate::attempt::AttemptOutputScope {
            generation_id: self.account.generation_id,
            max_output_tokens: state.max_output_tokens,
        };
        if intent
            .output_scope
            .as_ref()
            .is_some_and(|supplied| supplied != &captured)
        {
            return Err(crate::CostPersistError::Rejected(
                "attempt output generation mismatch".into(),
            ));
        }
        intent.output_scope = Some(captured);
        Ok(())
    }

    pub(crate) fn authorize_and_enqueue(
        &self,
        intent: &crate::AttemptIntent,
        max_reserved_nano_usd: u64,
        session_limit: Option<u64>,
        state: &crate::CostState,
        permit: crate::AttemptPersistPermit,
        ack: tokio::sync::oneshot::Sender<
            Result<crate::AttemptPersistAck, crate::CostPersistError>,
        >,
    ) -> Result<(), AttemptAdmissionError> {
        if intent.session_id != self.account.session_id
            || state.session_id != self.account.session_id
        {
            return Err(invalid("attempt admission session mismatch").into());
        }
        let gate = self.account.tracker.durability_gate();
        let mut book = self.account.session.lock_reservations(&gate)?;
        self.account
            .tracker
            .preflight_durable()
            .map_err(|error| invalid_owned(error.to_string()))?;
        let inserted = book.reserve_attempt(
            &intent.attempt_id,
            AttemptHold {
                run_id: intent.run_id.clone(),
                generation: self.account.generation_id,
                nano_usd: intent.authorized_nano_usd,
                output_tokens: intent.authorized_output_tokens,
            },
            max_reserved_nano_usd,
            state
                .total_nano_usd
                .saturating_add(state.unverified_nano_usd),
            session_limit,
        )?;
        if !inserted {
            return Err(invalid("attempt already owns an admission").into());
        }
        permit
            .enqueue(crate::AttemptPersistRequest {
                session_id: intent.session_id,
                mutation: crate::AttemptPersistMutation::Intent(intent.clone()),
                ack,
            })
            .map_err(|error| invalid_owned(error.to_string()))?;
        Ok(())
    }

    pub(crate) fn validate_tracker(
        &self,
        tracker: &CostTracker,
    ) -> Result<(), crate::CostPersistError> {
        if !tracker
            .durability_gate()
            .shares_authority(&self.account.tracker.durability_gate())
        {
            return Err(crate::CostPersistError::Rejected(
                "attempt output and tracker authorities differ".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn publish(
        &self,
        receipt: &AttemptReceipt,
        output: u64,
        state: &mut crate::CostState,
        projected: crate::CostState,
    ) -> Result<(), crate::CostPersistError> {
        if receipt.session_id != self.account.session_id
            || state.session_id != self.account.session_id
            || projected.session_id != self.account.session_id
        {
            return Err(crate::CostPersistError::Storage(
                "attempt output session mismatch".into(),
            ));
        }
        let mut book = self
            .account
            .session
            .lock_reservations(&self.account.tracker.durability_gate())
            .map_err(|error| crate::CostPersistError::Storage(error.to_string()))?;
        book.publish_attempt_output(self.account.generation_id, receipt, output)
            .map_err(|error| crate::CostPersistError::Storage(error.to_string()))?;
        *state = projected;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AttemptHold {
    pub(super) run_id: String,
    pub(super) generation: MessageId,
    pub(super) nano_usd: u64,
    pub(super) output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AttemptRunBudget {
    pub(super) generation: MessageId,
    pub(super) max_reserved_nano_usd: u64,
}

/// Capacity denial is an ordinary outcome, not a persistence fault. Keep
/// this classification typed; no caller should inspect diagnostic strings.
#[derive(Debug)]
pub(crate) enum AttemptAdmissionError {
    Denied(BudgetError),
    Fault(BudgetError),
}

impl From<BudgetError> for AttemptAdmissionError {
    fn from(error: BudgetError) -> Self {
        Self::Fault(error)
    }
}

type OutputRevision = crate::AttemptOutputRevision;

pub(super) struct PublishedAttemptOutput {
    pub(super) first: OutputRevision,
    pub(super) current: OutputRevision,
}

fn invalid(reason: &'static str) -> BudgetError {
    BudgetError::Internal(reason.into())
}

fn invalid_owned(reason: String) -> BudgetError {
    BudgetError::Internal(reason)
}

fn sum(mut values: impl Iterator<Item = u64>) -> Result<u64, BudgetError> {
    values.try_fold(0_u64, |total, value| {
        total
            .checked_add(value)
            .ok_or_else(|| invalid("attempt occupancy overflow"))
    })
}

impl ReservationBook {
    /// Called with cost state then book locked, after live authority checks.
    /// A failed check never adds either side of the combined hold.
    pub(super) fn reserve_attempt(
        &mut self,
        attempt_id: &str,
        hold: AttemptHold,
        max_reserved_nano_usd: u64,
        realized_nano_usd: u64,
        session_limit: Option<u64>,
    ) -> Result<bool, AttemptAdmissionError> {
        if attempt_id.is_empty() || hold.run_id.is_empty() {
            return Err(invalid("empty attempt budget identity").into());
        }
        let run = AttemptRunBudget {
            generation: hold.generation,
            max_reserved_nano_usd,
        };
        if self
            .attempt_runs
            .get(&hold.run_id)
            .is_some_and(|previous| previous != &run)
        {
            return Err(
                invalid("attempt run changed its output scope or outstanding limit").into(),
            );
        }
        if let Some(previous) = self.attempt_holds.get(attempt_id) {
            return if previous == &hold {
                Ok(false)
            } else {
                Err(invalid("attempt hold identity conflict").into())
            };
        }
        if self.attempt_origins.contains_key(attempt_id) {
            return Err(invalid("settled attempt cannot acquire another hold").into());
        }
        let scope = self
            .output_scopes
            .get(&hold.generation)
            .ok_or_else(|| invalid("attempt has no captured output scope"))?;
        if scope.attempts.contains_key(attempt_id) {
            return Err(invalid("settled attempt cannot acquire another hold").into());
        }
        // Checked sums reject overflow even when a limit is u64::MAX. A
        // saturating sum would incorrectly authorize at that boundary.
        let outstanding = self
            .checked_held()
            .ok_or_else(|| invalid("money occupancy overflow"))?;
        let current = realized_nano_usd
            .checked_add(outstanding)
            .ok_or_else(|| invalid("money occupancy overflow"))?;
        let next = current
            .checked_add(hold.nano_usd)
            .ok_or_else(|| invalid("money occupancy overflow"))?;
        if session_limit.is_some_and(|limit| next > limit) {
            return Err(AttemptAdmissionError::Denied(BudgetError::Exceeded {
                current_nano_usd: current,
            }));
        }
        let run_held = sum(self
            .attempt_holds
            .values()
            .filter(|entry| entry.run_id == hold.run_id)
            .map(|entry| entry.nano_usd))?;
        if run_held
            .checked_add(hold.nano_usd)
            .is_none_or(|next| next > max_reserved_nano_usd)
        {
            return Err(AttemptAdmissionError::Denied(BudgetError::Exceeded {
                current_nano_usd: run_held,
            }));
        }
        let output_held = sum(self
            .attempt_holds
            .values()
            .filter(|entry| entry.generation == hold.generation)
            .map(|entry| entry.output_tokens))?;
        let output_next = scope
            .spent
            .checked_add(output_held)
            .and_then(|value| value.checked_add(hold.output_tokens))
            .ok_or_else(|| invalid("output occupancy overflow"))?;
        if scope
            .max_output_tokens
            .is_some_and(|limit| output_next > limit)
        {
            return Err(AttemptAdmissionError::Denied(invalid(
                "workflow output budget exceeded",
            )));
        }
        self.attempt_runs.insert(hold.run_id.clone(), run);
        self.attempt_origins
            .insert(attempt_id.into(), hold.generation);
        self.attempt_holds.insert(attempt_id.into(), hold);
        Ok(true)
    }

    /// Called only after a validated durable receipt, while cost state and
    /// book remain locked. All fallible validation precedes hold retirement
    /// and output publication; the caller then installs the acknowledged cost
    /// vector before releasing those locks or finishing its durability turn.
    pub(super) fn publish_attempt_output(
        &mut self,
        generation: MessageId,
        receipt: &AttemptReceipt,
        output: u64,
    ) -> Result<bool, BudgetError> {
        if self.attempt_origins.get(&receipt.attempt_id) != Some(&generation) {
            return Err(invalid("attempt output origin mismatch"));
        }
        let scope = self
            .output_scopes
            .get(&generation)
            .ok_or_else(|| invalid("attempt output scope disappeared"))?;
        let next = OutputRevision {
            revision: receipt.revision,
            disposition: receipt.disposition,
            output,
        };
        let previous = scope.attempts.get(&receipt.attempt_id);
        let old_output = if let Some(previous) = previous {
            if next == previous.first || next == previous.current {
                return Ok(false);
            }
            if previous.current.disposition != AttemptDisposition::Unknown
                || previous.current.revision != 1
                || receipt.revision != 2
                || receipt.replaces_revision != Some(1)
                || receipt.disposition != AttemptDisposition::Exact
            {
                return Err(invalid("invalid output contribution replacement"));
            }
            previous.current.output
        } else {
            let hold = self
                .attempt_holds
                .get(&receipt.attempt_id)
                .ok_or_else(|| invalid("attempt output has no retained hold"))?;
            if hold.generation != generation
                || receipt.revision != 1
                || receipt.replaces_revision.is_some()
                || (receipt.disposition == AttemptDisposition::ProvenNotSent && output != 0)
                || (receipt.disposition == AttemptDisposition::Unknown
                    && output < hold.output_tokens)
            {
                return Err(invalid("attempt output does not match its hold"));
            }
            0
        };
        let spent = scope
            .spent
            .checked_sub(old_output)
            .and_then(|value| value.checked_add(output))
            .ok_or_else(|| invalid("output contribution arithmetic overflow"))?;
        let scope = self
            .output_scopes
            .get_mut(&generation)
            .expect("scope validated under book lock");
        scope
            .attempts
            .entry(receipt.attempt_id.clone())
            .and_modify(|published| published.current = next)
            .or_insert(PublishedAttemptOutput {
                first: next,
                current: next,
            });
        scope.spent = spent;
        scope.snapshot.store(spent, Ordering::Release);
        self.attempt_holds.remove(&receipt.attempt_id);
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::WorkflowOutputScope;
    use protocol::SessionId;

    async fn setup(limit: u64) -> (BudgetEnforcer, Arc<BudgetSessionState>, WorkflowOutputScope) {
        let (tx, _) = tokio::sync::mpsc::channel(1);
        let tracker = Arc::new(CostTracker::new(
            SessionId::new(),
            Arc::new(crate::PricingCatalog::empty()),
            tx,
        ));
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
        let session_id = enforcer.session_id().await;
        let scope = enforcer
            .workflow_output_scopes()
            .begin_turn(session_id, MessageId::new(), Some(limit))
            .await
            .unwrap();
        let session = enforcer.session_state_for(session_id).await;
        (enforcer, session, scope)
    }

    fn hold(scope: &WorkflowOutputScope, run: &str, money: u64, output: u64) -> AttemptHold {
        AttemptHold {
            run_id: run.into(),
            generation: scope.generation_id(),
            nano_usd: money,
            output_tokens: output,
        }
    }

    fn receipt(
        scope: &WorkflowOutputScope,
        id: &str,
        disposition: AttemptDisposition,
    ) -> AttemptReceipt {
        AttemptReceipt {
            session_id: scope.session_id(),
            attempt_id: id.into(),
            revision: 1,
            replaces_revision: None,
            disposition,
            token_quote_nano_usd: None,
            usage: crate::Usage::default(),
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            api_duration_ms: 0,
            api_duration_without_retries_ms: 0,
        }
    }

    #[tokio::test]
    async fn combined_authorization_has_no_partial_hold_and_includes_ordinary_money() {
        let (enforcer, session, scope) = setup(100).await;
        let ordinary = enforcer.reserve_nano_usd(400).await.unwrap();
        {
            let mut book = session.reservations.lock().unwrap();
            assert!(book
                .reserve_attempt(
                    "money-denied",
                    hold(&scope, "r", 601, 1),
                    1_000,
                    0,
                    Some(1_000)
                )
                .is_err());
            assert!(book
                .reserve_attempt(
                    "output-denied",
                    hold(&scope, "r", 1, 101),
                    1_000,
                    0,
                    Some(1_000)
                )
                .is_err());
            assert!(book.attempt_holds.is_empty());
            assert!(book.attempt_runs.is_empty());
            assert_eq!(book.held(), 400);
            assert!(book
                .reserve_attempt(
                    "accepted",
                    hold(&scope, "r", 600, 100),
                    1_000,
                    0,
                    Some(1_000)
                )
                .unwrap());
            assert_eq!(book.held(), 1_000);
        }
        assert!(enforcer.reserve_nano_usd(1).await.is_err());
        assert_eq!(scope.spent(), 0, "holds are occupancy, not observed spend");
        enforcer.release_reservation(ordinary).await;
        assert_eq!(enforcer.active_reservation_nano_usd().await, 600);
    }

    #[tokio::test]
    async fn run_outstanding_is_not_clipped_or_disabled_by_uncapped_session_money() {
        let (_enforcer, session, scope) = setup(100).await;
        let mut book = session.reservations.lock().unwrap();
        assert!(book
            .reserve_attempt("a", hold(&scope, "r", 80, 40), 100, 0, None)
            .unwrap());
        assert!(!book
            .reserve_attempt("a", hold(&scope, "r", 80, 40), 100, 0, None)
            .unwrap());
        assert!(book
            .reserve_attempt("b", hold(&scope, "r", 21, 1), 100, 0, None)
            .is_err());
        assert!(book
            .reserve_attempt("b", hold(&scope, "r", 20, 1), 101, 0, None)
            .is_err());
        assert!(book
            .reserve_attempt("other-run", hold(&scope, "other", 90, 60), 100, 0, None)
            .unwrap());
        assert_eq!(book.held(), 170);
        assert!(book
            .reserve_attempt("output-full", hold(&scope, "third", 1, 1), 100, 0, None)
            .is_err());
        assert_eq!(book.attempt_holds.len(), 2);
    }

    #[tokio::test]
    async fn unknown_exact_replacement_retires_hold_once_and_preserves_legacy_output() {
        let (_enforcer, session, scope) = setup(100).await;
        scope
            .record_legacy(
                platform_api::WorkflowOutputEventId::MainResponse(MessageId::new()),
                10,
            )
            .unwrap();
        let mut book = session.reservations.lock().unwrap();
        book.reserve_attempt("a", hold(&scope, "r", 80, 40), 100, 0, None)
            .unwrap();
        let unknown = receipt(&scope, "a", AttemptDisposition::Unknown);
        assert!(book
            .publish_attempt_output(scope.generation_id(), &unknown, 40)
            .unwrap());
        assert_eq!(book.held(), 0);
        assert_eq!(scope.spent(), 50);
        assert!(!book
            .publish_attempt_output(scope.generation_id(), &unknown, 40)
            .unwrap());
        let exact = AttemptReceipt {
            revision: 2,
            replaces_revision: Some(1),
            disposition: AttemptDisposition::Exact,
            ..unknown.clone()
        };
        assert!(book
            .publish_attempt_output(scope.generation_id(), &exact, 7)
            .unwrap());
        assert_eq!(scope.spent(), 17);
        assert!(!book
            .publish_attempt_output(scope.generation_id(), &unknown, 40)
            .unwrap());
        assert_eq!(scope.spent(), 17);
        assert!(book
            .publish_attempt_output(scope.generation_id(), &exact, 8)
            .is_err());
        assert!(book
            .reserve_attempt("a", hold(&scope, "r", 80, 40), 100, 0, None)
            .is_err());
        // maxReserved caps outstanding requests, not lifetime invoice.
        assert!(book
            .reserve_attempt("b", hold(&scope, "r", 90, 40), 100, 0, None)
            .unwrap());
    }

    #[tokio::test]
    async fn invalid_or_overflowed_publication_retains_hold_and_output() {
        let (_enforcer, session, scope) = setup(u64::MAX).await;
        let mut book = session.reservations.lock().unwrap();
        book.reserve_attempt("a", hold(&scope, "r", 80, 40), 100, 0, None)
            .unwrap();
        let unknown = receipt(&scope, "a", AttemptDisposition::Unknown);
        assert!(book
            .publish_attempt_output(scope.generation_id(), &unknown, 39)
            .is_err());
        assert!(book
            .publish_attempt_output(MessageId::new(), &unknown, 40)
            .is_err());
        let not_sent = receipt(&scope, "a", AttemptDisposition::ProvenNotSent);
        assert!(book
            .publish_attempt_output(scope.generation_id(), &not_sent, 1)
            .is_err());
        assert_eq!(book.held(), 80);
        assert_eq!(scope.spent(), 0);
        drop(book);
        scope
            .record_legacy(
                platform_api::WorkflowOutputEventId::MainResponse(MessageId::new()),
                u64::MAX,
            )
            .unwrap();
        let mut book = session.reservations.lock().unwrap();
        assert!(book
            .publish_attempt_output(scope.generation_id(), &unknown, 40)
            .is_err());
        assert_eq!(book.held(), 80);
        assert_eq!(scope.spent(), u64::MAX);
        assert!(book
            .publish_attempt_output(scope.generation_id(), &not_sent, 0)
            .unwrap());
        assert_eq!(book.held(), 0);
        assert_eq!(scope.spent(), u64::MAX);
    }

    #[tokio::test]
    async fn maximum_integer_is_not_an_overflow_permission() {
        let (_enforcer, session, scope) = setup(u64::MAX).await;
        let mut book = session.reservations.lock().unwrap();
        book.active.insert(99, u64::MAX);
        assert!(book
            .reserve_attempt("a", hold(&scope, "r", 1, 0), u64::MAX, 0, Some(u64::MAX))
            .is_err());
        assert!(book.attempt_holds.is_empty());
        book.active.clear();
        book.reserve_attempt("a", hold(&scope, "r", 0, u64::MAX), u64::MAX, 0, None)
            .unwrap();
        assert!(book
            .reserve_attempt("b", hold(&scope, "r", 0, 1), u64::MAX, 0, None)
            .is_err());
        assert_eq!(book.attempt_holds.len(), 1);
    }

    #[tokio::test]
    async fn ordinary_authorization_cannot_overflow_after_an_attempt_hold() {
        let (mut enforcer, session, scope) = setup(100).await;
        enforcer.config.max_session_nano_usd = Some(u64::MAX);
        session
            .reservations
            .lock()
            .unwrap()
            .reserve_attempt(
                "a",
                hold(&scope, "r", u64::MAX, 0),
                u64::MAX,
                0,
                Some(u64::MAX),
            )
            .unwrap();
        assert!(enforcer.reserve_nano_usd(1).await.is_err());
        let book = session.reservations.lock().unwrap();
        assert!(book.active.is_empty());
        assert_eq!(book.held(), u64::MAX);
    }

    #[tokio::test]
    async fn settled_identity_cannot_move_to_a_new_turn_or_lose_its_hold_to_old_correction() {
        let (enforcer, session, old) = setup(100).await;
        let next = enforcer
            .workflow_output_scopes()
            .begin_turn(old.session_id(), MessageId::new(), Some(100))
            .await
            .unwrap();
        let mut book = session.reservations.lock().unwrap();
        book.reserve_attempt("a", hold(&old, "r1", 10, 20), 100, 0, None)
            .unwrap();
        let unknown = receipt(&old, "a", AttemptDisposition::Unknown);
        book.publish_attempt_output(old.generation_id(), &unknown, 20)
            .unwrap();
        assert!(book
            .reserve_attempt("a", hold(&next, "r2", 30, 40), 100, 0, None)
            .is_err());
        book.reserve_attempt("b", hold(&next, "r2", 30, 40), 100, 0, None)
            .unwrap();
        let exact = AttemptReceipt {
            revision: 2,
            replaces_revision: Some(1),
            disposition: AttemptDisposition::Exact,
            ..unknown
        };
        assert!(book
            .publish_attempt_output(next.generation_id(), &exact, 5)
            .is_err());
        book.publish_attempt_output(old.generation_id(), &exact, 5)
            .unwrap();
        assert_eq!(book.held(), 30);
        assert!(book.attempt_holds.contains_key("b"));
        assert_eq!(old.spent(), 5);
        assert_eq!(next.spent(), 0);
    }
}
