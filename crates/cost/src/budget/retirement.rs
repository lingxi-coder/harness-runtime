//! Joint retirement preparation for this existing money/output book.
use super::*;

pub(crate) struct BudgetCacheRetirement {
    budget: Arc<BudgetEnforcer>,
    session_id: protocol::SessionId,
    expected: Option<Arc<BudgetSessionState>>,
}

impl BudgetEnforcer {
    pub(crate) fn shares_tracker(&self, tracker: &CostTracker) -> bool {
        self.cost_tracker.shares_ledger(tracker)
    }

    /// Caller owns the originating retention retirement token and tracker
    /// bookkeeping gate. New external views/admissions are already excluded.
    pub(crate) async fn prepare_cache_retirement(
        self: &Arc<Self>,
        session_id: protocol::SessionId,
    ) -> Result<Option<BudgetCacheRetirement>, crate::CostPersistError> {
        if self.session_scope.is_some_and(|id| id != session_id) {
            return Err(crate::CostPersistError::Rejected(
                "retirement budget session mismatch".into(),
            ));
        }
        let _publication = self.sessions.output_publication.lock().await;
        let sessions = self.sessions.sessions.lock().await;
        let expected = sessions.get(&session_id).cloned();
        if self
            .sessions
            .settlements
            .lock()
            .map_err(|_| {
                crate::CostPersistError::Storage("retirement settlement registry poisoned".into())
            })?
            .values()
            .any(|slot| {
                slot.session_id == session_id
                    && !slot.result.lock().is_ok_and(|result| result.is_some())
            })
        {
            return Ok(None);
        }
        if let Some(session) = &expected {
            let book = session.reservations.lock().map_err(|_| {
                crate::CostPersistError::Storage("retirement reservation book poisoned".into())
            })?;
            if !book.active.is_empty() || !book.attempt_holds.is_empty() {
                return Ok(None);
            }
            let owners = self.sessions.owners.lock().map_err(|_| {
                crate::CostPersistError::Storage("retirement reservation owners poisoned".into())
            })?;
            if owners.values().any(|owner| owner.session_id == session_id) {
                return Ok(None);
            }
        }
        Ok(Some(BudgetCacheRetirement {
            budget: self.clone(),
            session_id,
            expected,
        }))
    }
}

impl BudgetCacheRetirement {
    pub(crate) async fn finalize(self) -> Result<(), crate::CostPersistError> {
        let _publication = self.budget.sessions.output_publication.lock().await;
        let mut sessions = self.budget.sessions.sessions.lock().await;
        let matches = match (sessions.get(&self.session_id), self.expected.as_ref()) {
            (Some(actual), Some(expected)) => Arc::ptr_eq(actual, expected),
            (None, None) => true,
            _ => false,
        };
        if !matches {
            return Err(crate::CostPersistError::Rejected(
                "retirement budget authority changed".into(),
            ));
        }
        let mut retired_settlements = Vec::new();
        {
            let mut settlements = self.budget.sessions.settlements.lock().map_err(|_| {
                crate::CostPersistError::Storage("retirement settlement registry poisoned".into())
            })?;
            if settlements.values().any(|slot| {
                slot.session_id == self.session_id
                    && !slot.result.lock().is_ok_and(|result| result.is_some())
            }) {
                return Err(crate::CostPersistError::Rejected(
                    "retirement settlement is still pending".into(),
                ));
            }
            settlements.retain(|_, slot| {
                if slot.session_id == self.session_id {
                    retired_settlements.push(slot.clone());
                    false
                } else {
                    true
                }
            });
        }
        let retired = self
            .budget
            .sessions
            .current_outputs
            .lock()
            .map_err(|_| {
                crate::CostPersistError::Storage("retirement output registry poisoned".into())
            })?
            .remove(&self.session_id);
        let book = sessions.remove(&self.session_id);
        drop(sessions);
        drop(_publication);
        // Cache destructors may release scoped trackers; no accounting locks
        // are held while their internal references are released.
        drop(retired);
        drop(book);
        drop(retired_settlements);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retirement_removes_only_origin_completed_settlements_without_recharging_old_ids() {
        let session_a = protocol::SessionId::new();
        let session_b = protocol::SessionId::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(8);
        let tracker = Arc::new(CostTracker::new(
            session_a,
            Arc::new(crate::PricingCatalog::empty()),
            tx,
        ));
        let budget = Arc::new(BudgetEnforcer::new(
            BudgetConfig {
                max_session_nano_usd: None,
                max_turn_nano_usd: None,
                max_turn_tokens: None,
                warning_thresholds: vec![],
                on_exceed: BudgetExceedPolicy::Halt,
            },
            tracker.clone(),
        ));
        let a = budget.reserve_nano_usd(10).await.unwrap();
        budget.commit_reservation(a, 5).await.unwrap();
        let external_receipt = budget.begin_commit_reservation(a, 5).unwrap().unwrap();
        let other = budget.scoped_for_session(session_b);
        let b = other.reserve_nano_usd(10).await.unwrap();
        other.commit_reservation(b, 7).await.unwrap();
        assert_eq!(budget.sessions.settlements.lock().unwrap().len(), 2);
        assert_eq!(
            budget.sessions.settlements.lock().unwrap()[&a.raw()].session_id,
            session_a
        );
        assert_eq!(
            budget.sessions.settlements.lock().unwrap()[&b.raw()].session_id,
            session_b
        );
        // Simulate a not-yet-completed session-specific slot, independently of
        // the live-hold checks already covered by the budget tests.
        let pending_id = u64::MAX;
        budget
            .sessions
            .settlements
            .lock()
            .unwrap()
            .insert(pending_id, Arc::new(SettlementSlot::new(session_a, 0)));
        assert!(budget
            .prepare_cache_retirement(session_a)
            .await
            .unwrap()
            .is_none());
        budget
            .sessions
            .settlements
            .lock()
            .unwrap()
            .remove(&pending_id);
        budget
            .prepare_cache_retirement(session_a)
            .await
            .unwrap()
            .unwrap()
            .finalize()
            .await
            .unwrap();
        assert_eq!(budget.sessions.settlements.lock().unwrap().len(), 1);
        assert!(budget
            .sessions
            .settlements
            .lock()
            .unwrap()
            .contains_key(&b.raw()));
        external_receipt.finish().await.unwrap();
        budget.commit_reservation(a, 5).await.unwrap();
        assert_eq!(tracker.total_nano_usd().await, 5);
        assert_eq!(tracker.scoped(session_b).total_nano_usd().await, 7);
    }
}
