use super::*;

impl CoordinatorState {
    pub(super) fn recover_disk_full(
        &self,
        expected: CostStateVector,
    ) -> Result<Vec<CostPersistAck>, CostPersistError> {
        let _serial = self
            .mutation_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(reason) = self.durability_gate.frozen_reason() else {
            return Ok(Vec::new());
        };
        if !cost::persistence::is_disk_full(&reason) {
            return Err(CostPersistError::Frozen(reason));
        }
        let mut failure = None;
        let recovered =
            self.durability_gate
                .thaw_if_intact(|| match self.retry_storage_writes(&expected) {
                    Ok(()) => true,
                    Err(error) => {
                        failure = Some(error);
                        false
                    }
                });
        if recovered {
            Ok(self
                .results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .filter_map(|cached| cached.result.as_ref().ok().cloned())
                .collect())
        } else {
            Err(failure.unwrap_or_else(|| {
                CostPersistError::Frozen(self.durability_gate.frozen_reason().unwrap_or(reason))
            }))
        }
    }

    fn retry_storage_writes(&self, expected: &CostStateVector) -> Result<(), CostPersistError> {
        let mut pending = self
            .results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter_map(|cached| cached.record.clone())
            .collect::<Vec<_>>();
        pending.sort_by_key(|record| record.cost_revision);

        // Replay validates the existing prefix and repairs only an incomplete
        // final WAL line. Never discard a ledger or truncate committed history.
        let replay = self.journal.replay().map_err(map_journal_error)?;
        let known = self.last_journal_revision.load(AtomicOrdering::Acquire);
        if replay
            .entries
            .last()
            .map_or(0, |entry| entry.journal_revision)
            < known
        {
            return Err(CostPersistError::Storage(
                "storage recovery found missing committed journal history".into(),
            ));
        }
        for entry in replay
            .entries
            .iter()
            .filter(|entry| entry.journal_revision > known)
        {
            let SessionEvent::Cost(record) = decode_session_event(entry.event.clone())? else {
                return Err(CostPersistError::Storage(
                    "storage recovery found an unaccounted journal event".into(),
                ));
            };
            if entry.event_id != record.mutation_id.as_str() || !pending.contains(&record) {
                return Err(CostPersistError::Storage(
                    "storage recovery found an unknown cost mutation".into(),
                ));
            }
        }

        for record in pending {
            let (ack, _) = tokio::sync::oneshot::channel();
            let request = CostPersistRequest {
                session_id: self.session_id,
                cost_revision: record.cost_revision,
                mutation_id: record.mutation_id.clone(),
                state: record.state.clone(),
                source: record.source.clone(),
                ack,
            };
            // append_once reuses the original mutation identity, including when
            // bytes landed but the original fsync/ack failed.
            let result = self.persist_request_inner(&request, &record, true)?;
            self.results
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(
                    record.mutation_id.clone(),
                    CachedCostResult {
                        record: None,
                        result: Ok(result),
                    },
                );
        }
        let snapshot = self.snapshot_projection();
        if &snapshot.cost != expected {
            return Err(CostPersistError::Storage(
                "storage recovery cost projection does not match retained usage".into(),
            ));
        }
        let revision = self.last_journal_revision.load(AtomicOrdering::Acquire);
        self.journal
            .write_snapshot(revision, &encode_projection_snapshot(&snapshot))
            .map_err(map_journal_error)?;
        self.snapshot_present.store(true, AtomicOrdering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::live_sessions::SessionWriterLease;

    struct Lease(String);
    impl SessionWriterLease for Lease {
        fn session_id(&self) -> &str {
            &self.0
        }
    }

    struct FailOnce {
        coordinator: Arc<SessionStateCoordinator>,
        fail: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl CostPersistence for FailOnce {
        async fn acquire_permit(
            &self,
            id: SessionId,
        ) -> Result<cost::CostPersistPermit, CostPersistError> {
            let permit = self.coordinator.acquire_permit(id).await?;
            if !self.fail.swap(false, AtomicOrdering::SeqCst) {
                return Ok(permit);
            }
            let coordinator = self.coordinator.clone();
            Ok(cost::CostPersistPermit::new(move |request| {
                drop(permit);
                let error =
                    CostPersistError::Storage("No space left on device (os error 28)".into());
                let record = CostMutationRecord {
                    mutation_id: request.mutation_id.clone(),
                    cost_revision: request.cost_revision,
                    state: request.state,
                    source: request.source,
                };
                coordinator.state.results.lock().unwrap().insert(
                    record.mutation_id.clone(),
                    CachedCostResult {
                        record: Some(record),
                        result: Err(error.clone()),
                    },
                );
                coordinator.durability_gate().freeze(error.to_string());
                let _ = request.ack.send(Err(error));
                Ok(())
            }))
        }

        async fn recover_disk_full(
            &self,
            id: SessionId,
            expected: CostStateVector,
        ) -> Result<Vec<CostPersistAck>, CostPersistError> {
            self.coordinator.recover_disk_full(id, expected).await
        }
    }

    #[tokio::test]
    async fn disk_full_recovery_restores_paid_dispatch_and_the_original_response_receipt() {
        let home = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let manager = SessionStateManager::new(home.path());
        let coordinator = manager.ensure_coordinator(id).await.unwrap();
        let hydration = coordinator.hydrate(id).await.unwrap();
        let (legacy, _receiver) = tokio::sync::mpsc::channel(1);
        let tracker = Arc::new(
            CostTracker::new(
                id,
                Arc::new(cost::PricingCatalog::builtin_reference()),
                legacy,
            )
            .try_with_durable_persistence(
                hydration,
                Arc::new(FailOnce {
                    coordinator: coordinator.clone(),
                    fail: std::sync::atomic::AtomicBool::new(true),
                }),
                coordinator.writer_lease(),
                coordinator.durability_gate(),
            )
            .unwrap(),
        );
        let scope = cost::CostSessionScope::new(tracker.clone());
        let response = || cost::CostModelResponse {
            model_ref: cost::ModelRef {
                provider: cost::ProviderId::Anthropic,
                model: "claude-opus-4-8".into(),
            },
            usage: cost::Usage::default(),
            duration: std::time::Duration::ZERO,
            retries: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            is_batch_request: false,
            bus: None,
        };
        let receipt = scope.submit_model_response_with_pricing(
            response(),
            cost::CostResponsePricing::CompleteQuote(17),
            cost::CostResponseMeasurement::Observed,
        );
        let mutation = receipt.mutation_id().clone();
        assert!(receipt.settle().await.persistence_result().is_err());
        assert!(scope.preflight().await.is_err());
        scope.recover_disk_full().await.unwrap();
        scope.preflight().await.unwrap();
        assert!(matches!(
            scope.retained_response(&mutation).unwrap().settlement,
            Some(Ok(Some(_)))
        ));
        assert_eq!(tracker.snapshot().await.total_nano_usd, 17);
        tracker.drain_owned_settlements().await.unwrap();
        let next = scope.submit_model_response_with_pricing(
            response(),
            cost::CostResponsePricing::CompleteQuote(17),
            cost::CostResponseMeasurement::Observed,
        );
        assert!(next.settle().await.persistence_result().is_ok());
        assert_eq!(tracker.snapshot().await.total_nano_usd, 34);
        let model_responses = coordinator.journal().replay().unwrap().entries.into_iter().filter(|entry| {
            matches!(decode_session_event(entry.event.clone()), Ok(SessionEvent::Cost(record)) if record.source == CostMutationSource::ModelResponse)
        }).count();
        assert_eq!(model_responses, 2);
        manager.close_and_drain().await.unwrap();
    }

    fn interrupted() -> (
        tempfile::TempDir,
        Arc<SessionStateCoordinator>,
        CostMutationRecord,
    ) {
        let home = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let coordinator =
            SessionStateCoordinator::open(home.path(), id, Arc::new(Lease(id.to_string())))
                .unwrap();
        let record = CostMutationRecord {
            mutation_id: CostMutationId::new("model-response:disk-full:test"),
            cost_revision: 1,
            source: CostMutationSource::ModelResponse,
            state: CostStateVector::from(&CostState {
                session_id: id,
                cost_revision: 1,
                total_nano_usd: 17,
                ..Default::default()
            }),
        };
        let error = CostPersistError::Storage("No space left on device (os error 28)".into());
        coordinator.state.results.lock().unwrap().insert(
            record.mutation_id.clone(),
            CachedCostResult {
                record: Some(record.clone()),
                result: Err(error.clone()),
            },
        );
        coordinator.durability_gate().freeze(error.to_string());
        (home, coordinator, record)
    }

    #[test]
    fn disk_full_recovery_retries_original_cost_mutation_exactly_once() {
        let (_home, coordinator, record) = interrupted();
        for _ in 0..2 {
            coordinator
                .state
                .recover_disk_full(record.state.clone())
                .unwrap();
        }
        let entries = coordinator.journal().replay().unwrap().entries;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_id, record.mutation_id.as_str());
        assert_eq!(coordinator.state.snapshot_projection().cost, record.state);
        assert!(coordinator.durability_gate().frozen_reason().is_none());
        assert!(
            coordinator.state.results.lock().unwrap()[&record.mutation_id]
                .result
                .is_ok()
        );
    }

    #[test]
    fn disk_full_recovery_handles_a_write_that_landed_before_its_ack_failed() {
        let (_home, coordinator, record) = interrupted();
        coordinator
            .journal()
            .append_once(
                record.mutation_id.as_str(),
                &encode_session_event(&SessionEvent::Cost(record.clone())).unwrap(),
            )
            .unwrap();
        coordinator
            .state
            .recover_disk_full(record.state.clone())
            .unwrap();
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 1);
        assert_eq!(coordinator.state.snapshot_projection().cost, record.state);
    }

    #[test]
    fn disk_full_recovery_keeps_pause_when_storage_is_still_unwritable() {
        let (_home, coordinator, record) = interrupted();
        let snapshot = coordinator
            .journal()
            .root()
            .join(session::jsonl::SNAPSHOT_FILE_NAME);
        std::fs::create_dir(&snapshot).unwrap();
        assert!(coordinator
            .state
            .recover_disk_full(record.state.clone())
            .is_err());
        assert!(coordinator.durability_gate().frozen_reason().is_some());
        std::fs::remove_dir(snapshot).unwrap();
        coordinator
            .state
            .recover_disk_full(record.state.clone())
            .unwrap();
        assert_eq!(coordinator.journal().replay().unwrap().entries.len(), 1);
        assert!(coordinator.durability_gate().frozen_reason().is_none());
    }

    #[test]
    fn disk_full_recovery_preserves_corrupt_history_and_refuses_to_thaw() {
        let (_home, coordinator, record) = interrupted();
        let wal = coordinator.journal().root().join("ledger.v1.jsonl");
        let corrupt = b"{\"schema_version\":1,\"broken\":true}\n";
        std::fs::write(&wal, corrupt).unwrap();
        assert!(coordinator.state.recover_disk_full(record.state).is_err());
        assert!(coordinator.durability_gate().frozen_reason().is_some());
        assert_eq!(std::fs::read(wal).unwrap(), corrupt);
        assert!(!coordinator.journal().root().join("quarantine").exists());
    }

    #[test]
    fn disk_full_recovery_never_ignores_missing_committed_records() {
        let (_home, coordinator, record) = interrupted();
        coordinator
            .state
            .recover_disk_full(record.state.clone())
            .unwrap();
        coordinator.durability_gate().freeze("ENOSPC");
        let wal = coordinator.journal().root().join("ledger.v1.jsonl");
        std::fs::write(&wal, b"").unwrap();
        assert!(coordinator.state.recover_disk_full(record.state).is_err());
        assert!(coordinator.durability_gate().frozen_reason().is_some());
        assert!(std::fs::read(wal).unwrap().is_empty());
    }

    #[test]
    fn disk_full_recovery_refuses_unrelated_freezes_and_usage_mismatches() {
        let (_home, coordinator, record) = interrupted();
        let mut mismatched = record.state.clone();
        mismatched.total_nano_usd += 1;
        assert!(coordinator.state.recover_disk_full(mismatched).is_err());
        assert!(coordinator.durability_gate().frozen_reason().is_some());
        let gate = coordinator.durability_gate();
        assert!(gate.thaw_if_intact(|| true));
        gate.freeze("journal ownership changed");
        assert!(coordinator.state.recover_disk_full(record.state).is_err());
        assert!(gate.frozen_reason().is_some());
    }
}
