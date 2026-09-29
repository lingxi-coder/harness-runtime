//! Cost half of coordinated Desktop retirement. This does not choose LRU
//! victims, close coordinators, retire recorders, or delete any durable files.
use super::*;

/// Cost/output-cache retirement reservation for one inactive session epoch.
/// It retains bookkeeping exclusivity while the Desktop owner checks its other
/// caches and drains the coordinator. No token operation deletes durable history.
pub struct CostSessionRetirement {
    ledger: Arc<SessionLedger>,
    entry: Arc<SessionEntry>,
    bookkeeping: OwnedMutexGuard<HashMap<SessionId, u64>>,
    budget: Option<crate::budget::BudgetCacheRetirement>,
    retirement: Option<lingxi_core::host::SessionRetirement>,
    closing: bool,
}

impl CostTracker {
    /// The Desktop owner must serialize this operation with coordinator
    /// creation/lookup, then check recorder/outbox/worker eligibility before
    /// calling begin_close. A dropped unstarted token rolls back to Live.
    pub async fn prepare_session_retirement(
        self: &Arc<Self>,
        session_id: SessionId,
        authority: &CostDurabilityGate,
        budget: Arc<crate::BudgetEnforcer>,
    ) -> Result<Option<CostSessionRetirement>, CostPersistError> {
        if self.scope.is_some() || !budget.shares_tracker(self) {
            return Err(CostPersistError::Rejected(
                "retirement requires the original shared cost ledger".into(),
            ));
        }
        let bookkeeping = self.ledger.hydrated_sessions.clone().lock_owned().await;
        if self.scope_or_active() == session_id {
            return Ok(None);
        }
        let Some(entry) = self.ledger.existing_entry(session_id) else {
            return Ok(None);
        };
        if !entry.durability_gate.shares_authority(authority) {
            return Err(CostPersistError::Rejected(
                "retirement session authority mismatch".into(),
            ));
        }
        if entry.persistence.is_none() || entry.missing_durable_authority || !authority.is_idle() {
            return Ok(None);
        }
        let retirement = match authority.retention_gate().try_begin_retirement() {
            Ok(Some(token)) => token,
            Ok(None) | Err(lingxi_core::host::SessionRetentionError::Unavailable) => {
                return Ok(None)
            }
            Err(error) => {
                authority.freeze(error.to_string());
                return Err(CostPersistError::Storage(error.to_string()));
            }
        };
        // Registering a mutation takes a pin before its durability ticket.
        // Recheck after blocking new pins, with no synchronous guard at await.
        if !authority.is_idle()
            || !entry
                .response_settlements
                .lock()
                .map_err(|_| CostPersistError::Storage("response registry poisoned".into()))?
                .values()
                .all(|slot| slot.result.lock().is_ok_and(|result| result.is_some()))
            || !entry
                .attempt_settlements
                .lock()
                .map_err(|_| CostPersistError::Storage("attempt registry poisoned".into()))?
                .is_idle()
        {
            return Ok(None);
        }
        let Some(budget) = budget.prepare_cache_retirement(session_id).await? else {
            return Ok(None);
        };
        Ok(Some(CostSessionRetirement {
            ledger: self.ledger.clone(),
            entry,
            bookkeeping,
            budget: Some(budget),
            retirement: Some(retirement),
            closing: false,
        }))
    }
}

impl CostSessionRetirement {
    /// Canonical session whose in-memory cost/output entries are reserved for retirement.
    pub fn session_id(&self) -> SessionId {
        self.entry.session_id
    }
    /// Monotonic epoch of the corresponding session-authority retirement gate.
    pub fn epoch(&self) -> u64 {
        self.retirement
            .as_ref()
            .expect("live retirement token")
            .epoch()
    }

    /// Desktop must first verify no live owner, pending ACK, outbox or frozen
    /// authority in its own caches. From here cancellation cannot reopen it.
    pub fn begin_close(&mut self) -> Result<(), CostPersistError> {
        self.retirement
            .as_mut()
            .expect("live retirement token")
            .begin_close()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        self.closing = true;
        Ok(())
    }

    /// Call only after the matching Desktop coordinator has successfully
    /// closed/drained. Removes only cost-side in-memory caches; no disk I/O.
    pub async fn finalize(mut self) -> Result<(), CostPersistError> {
        if !self.closing {
            return Err(CostPersistError::Rejected(
                "retirement close has not started".into(),
            ));
        }
        if !self.entry.durability_gate.is_idle() {
            return Err(CostPersistError::Rejected(
                "retirement authority is not drained".into(),
            ));
        }
        self.budget
            .take()
            .expect("retirement budget token")
            .finalize()
            .await?;
        let removed = {
            let mut entries = self.ledger.entries.lock().map_err(|_| {
                CostPersistError::Storage("retirement tracker registry poisoned".into())
            })?;
            if !entries
                .get(&self.entry.session_id)
                .is_some_and(|entry| Arc::ptr_eq(entry, &self.entry))
            {
                return Err(CostPersistError::Rejected(
                    "retirement tracker authority changed".into(),
                ));
            }
            entries.remove(&self.entry.session_id)
        };
        self.bookkeeping.remove(&self.entry.session_id);
        self.retirement
            .take()
            .expect("retirement epoch token")
            .finish()
            .map_err(|error| CostPersistError::Rejected(error.to_string()))?;
        drop(removed);
        Ok(())
    }
}

impl Drop for CostSessionRetirement {
    fn drop(&mut self) {
        // Roll back an unstarted gate before releasing the bookkeeping guard.
        // A queued prepare must never observe a spurious Retiring interval.
        drop(self.retirement.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BudgetConfig, BudgetEnforcer, BudgetExceedPolicy};

    struct Lease(String);
    impl lingxi_core::host::live_sessions::SessionWriterLease for Lease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }
    struct Persistence;
    #[async_trait::async_trait]
    impl crate::CostPersistence for Persistence {
        async fn acquire_permit(
            &self,
            _: SessionId,
        ) -> Result<crate::CostPersistPermit, CostPersistError> {
            Err(CostPersistError::Rejected(
                "no provider work in retirement fixture".into(),
            ))
        }
    }
    #[async_trait::async_trait]
    impl CostHydrator for Persistence {
        async fn hydrate(&self, session: SessionId) -> Result<CostHydration, CostPersistError> {
            Ok(hydration(session))
        }
    }
    fn hydration(session: SessionId) -> CostHydration {
        CostHydration {
            state: CostState {
                session_id: session,
                total_nano_usd: 42,
                ..Default::default()
            },
            journal_revision: 1,
            attempt_outputs: Vec::new(),
        }
    }
    async fn fixture() -> (
        Arc<CostTracker>,
        Arc<BudgetEnforcer>,
        SessionId,
        CostDurabilityGate,
        Arc<Persistence>,
    ) {
        let session = SessionId::new();
        let gate = CostDurabilityGate::default();
        let persistence = Arc::new(Persistence);
        let (tx, _) = mpsc::channel(1);
        let tracker = Arc::new(
            CostTracker::new(session, Arc::new(PricingCatalog::empty()), tx)
                .try_with_durable_persistence(
                    hydration(session),
                    persistence.clone(),
                    Arc::new(Lease(session.to_string())),
                    gate.clone(),
                )
                .unwrap(),
        );
        let budget = Arc::new(BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: Some(100),
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        ));
        drop(
            budget
                .workflow_output_scopes()
                .begin_turn(session, lingxi_core::types::MessageId::new(), Some(100))
                .await
                .unwrap(),
        );
        let next = SessionId::new();
        drop(
            tracker
                .prepare_session_hydrated_with_durable(
                    next,
                    persistence.as_ref(),
                    persistence.clone(),
                    Arc::new(Lease(next.to_string())),
                    CostDurabilityGate::default(),
                )
                .await
                .unwrap()
                .activate(),
        );
        (tracker, budget, session, gate, persistence)
    }

    #[tokio::test]
    async fn retention_cache_core_is_unpinned_but_external_tracker_and_output_views_pin() {
        let (tracker, budget, session, gate, _) = fixture().await;
        let scopes = budget.workflow_output_scopes();
        let scope = Arc::new(scopes.capture(session).unwrap());
        let weak = Arc::downgrade(&scope);
        assert!(tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .is_none());
        drop(scope);
        assert!(weak.upgrade().is_none());
        let external = tracker.scoped(session);
        assert!(tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .is_none());
        drop(external);
        let token = tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(scopes.capture(session).is_err());
        assert!(tracker.scoped(session).preflight_durable().is_err());
        drop(token);
        assert!(scopes.capture(session).is_ok());
        assert!(tracker.scoped(session).preflight_durable().is_ok());
    }

    #[tokio::test]
    async fn retention_prepared_activation_wins_before_retirement_checks_active_session() {
        let (tracker, budget, session, gate, persistence) = fixture().await;
        let lease = tracker
            .ledger
            .existing_entry(session)
            .unwrap()
            .writer_lease
            .clone()
            .unwrap();
        let prepared = tracker
            .prepare_session_hydrated_with_durable(
                session,
                persistence.as_ref(),
                persistence.clone(),
                lease,
                gate.clone(),
            )
            .await
            .unwrap();
        let retirement = tracker.prepare_session_retirement(session, &gate, budget);
        tokio::pin!(retirement);
        tokio::select! { biased;
            _ = &mut retirement => panic!("prepared switch must retain bookkeeping exclusivity"),
            () = tokio::task::yield_now() => {}
        }
        drop(prepared.activate());
        assert!(retirement.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn retention_only_closed_token_removes_memory_and_rehydrate_restores_known_cost() {
        let (tracker, budget, session, gate, persistence) = fixture().await;
        let token = tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .unwrap();
        assert!(token.finalize().await.is_err());
        assert!(gate.retention_gate().is_live());
        let mut token = tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .unwrap();
        token.begin_close().unwrap();
        token.finalize().await.unwrap();
        assert!(budget.workflow_output_scopes().capture(session).is_err());
        assert!(tracker.ledger.existing_entry(session).is_none());
        assert!(!gate.retention_gate().is_live());
        drop(
            tracker
                .prepare_session_hydrated_with_durable(
                    session,
                    persistence.as_ref(),
                    persistence.clone(),
                    Arc::new(Lease(session.to_string())),
                    CostDurabilityGate::default(),
                )
                .await
                .unwrap()
                .activate(),
        );
        assert_eq!(tracker.total_nano_usd().await, 42);
        assert_eq!(
            budget
                .workflow_output_scopes()
                .ensure_current(session, lingxi_core::types::MessageId::new(), Some(100))
                .await
                .unwrap()
                .spent(),
            0
        );
    }

    #[tokio::test]
    async fn retention_frozen_pending_ack_and_wrong_authority_are_not_retired() {
        let (tracker, budget, session, gate, _) = fixture().await;
        assert!(tracker
            .prepare_session_retirement(session, &CostDurabilityGate::default(), budget.clone())
            .await
            .is_err());
        let pending_ack = gate.register_mutation().unwrap();
        assert!(tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .is_none());
        pending_ack.finish();
        let scoped_budget = budget.scoped_for_session(session);
        let hold = scoped_budget.reserve_nano_usd(1).await.unwrap();
        drop(scoped_budget);
        assert!(tracker
            .prepare_session_retirement(session, &gate, budget.clone())
            .await
            .unwrap()
            .is_none());
        budget.release_reservation(hold).await;
        gate.freeze("test durability failure");
        assert!(tracker
            .prepare_session_retirement(session, &gate, budget)
            .await
            .unwrap()
            .is_none());
        assert!(tracker.ledger.existing_entry(session).is_some());
    }
}
