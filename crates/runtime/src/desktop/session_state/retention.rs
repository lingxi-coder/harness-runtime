//! Coordinated, memory-only retirement. The creation gate serializes mount,
//! hydration and retirement; retention pins serialize external use with close.
use super::*;
use crate::desktop::fusion_recorder::DesktopFusionRecorderFactory;
use std::sync::Weak;

const ELIGIBLE_INACTIVE_LIMIT: usize = 64;

pub(super) struct Maintenance {
    tracker: Weak<CostTracker>,
    budget: Weak<cost::BudgetEnforcer>,
    recorders: Weak<DesktopFusionRecorderFactory>,
    runtime: tokio::runtime::Handle,
    pub(super) wake: Arc<dyn Fn() + Send + Sync>,
}

impl SessionStateManager {
    /// Wire after the shared cost/output books exist. Weak edges avoid a
    /// manager -> recorder factory -> manager or cost persistence cycle.
    pub(crate) fn configure_retention(
        self: &Arc<Self>,
        tracker: &Arc<CostTracker>,
        budget: &Arc<cost::BudgetEnforcer>,
        recorders: &Arc<DesktopFusionRecorderFactory>,
    ) {
        let owner = Arc::downgrade(self);
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(owner) = owner.upgrade() {
                owner.request_maintenance();
            }
        });
        let configured = self.maintenance.set(Maintenance {
            tracker: Arc::downgrade(tracker),
            budget: Arc::downgrade(budget),
            recorders: Arc::downgrade(recorders),
            runtime: tokio::runtime::Handle::current(),
            wake,
        });
        assert!(configured.is_ok(), "retention composition installed once");
        for coordinator in self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            self.attach_retention_callback(coordinator);
        }
        self.request_maintenance();
    }

    pub(super) fn attach_retention_callback(&self, coordinator: &SessionStateCoordinator) {
        if let Some(maintenance) = self.maintenance.get() {
            coordinator
                .durability_gate()
                .retention_gate()
                .set_idle_callback(Arc::downgrade(&maintenance.wake));
        }
    }

    pub(super) fn touch(&self, session_id: SessionId) {
        self.last_used
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(session_id, std::time::Instant::now());
    }

    fn request_maintenance(self: &Arc<Self>) {
        let Some(maintenance) = self.maintenance.get() else {
            return;
        };
        if self.closing.load(AtomicOrdering::Acquire) {
            return;
        }
        self.maintenance_requested
            .store(true, AtomicOrdering::Release);
        if self
            .maintenance_running
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
        {
            return;
        }
        let owner = self.clone();
        // This task owns the entire close/finalize sequence. Callers and idle
        // notification waiters do not own a cancellable retirement future.
        maintenance.runtime.spawn(async move {
            loop {
                owner.maintenance_requested.store(false, AtomicOrdering::Release);
                if let Err(error) = owner.retire_excess().await {
                    tracing::error!(%error, "session cache retirement failed; authority retained closed");
                }
                if owner.maintenance_requested.swap(false, AtomicOrdering::AcqRel) {
                    continue;
                }
                owner.maintenance_running.store(false, AtomicOrdering::Release);
                // Close the idle-notification race without spawning a task
                // per Drop. At most one worker owns the running bit.
                if owner.maintenance_requested.load(AtomicOrdering::Acquire)
                    && owner.maintenance_running.compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire).is_ok() {
                    continue;
                }
                break;
            }
        });
    }

    async fn retire_excess(&self) -> Result<(), CostPersistError> {
        let Some(maintenance) = self.maintenance.get() else {
            return Ok(());
        };
        // Queued maintenance must not retain the cost ledger or writer claims
        // behind shutdown. Only upgrade authorities once we own the mount
        // gate and know shutdown has not started.
        let _creation = self.creation_gate.clone().lock_owned().await;
        if self.closing.load(AtomicOrdering::Acquire) {
            return Ok(());
        }
        let (Some(tracker), Some(budget), Some(recorders)) = (
            maintenance.tracker.upgrade(),
            maintenance.budget.upgrade(),
            maintenance.recorders.upgrade(),
        ) else {
            return Ok(());
        };
        let mut candidates = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, core)| (*id, core.clone()))
            .collect::<Vec<_>>();
        {
            let last_used = self
                .last_used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            candidates.sort_by_key(|(id, _)| (last_used.get(id).copied(), id.to_string()));
        }
        let mut eligible = Vec::new();
        for (id, core) in candidates {
            if !core.retirement_projection_idle() {
                continue;
            }
            if let Some(token) = tracker
                .prepare_session_retirement(id, &core.durability_gate(), budget.clone())
                .await?
            {
                // Never retain multiple cost bookkeeping tokens: each holds
                // the same global prepare/activation gate. Counting creates
                // no external pins and rollback does not signal idle.
                drop(token);
                eligible.push((id, core));
            }
        }
        let excess = eligible.len().saturating_sub(ELIGIBLE_INACTIVE_LIMIT);
        for (id, core) in eligible.into_iter().take(excess) {
            let Some(mut token) = tracker
                .prepare_session_retirement(id, &core.durability_gate(), budget.clone())
                .await?
            else {
                continue;
            };
            if !core.retirement_projection_idle() {
                continue;
            }
            token.begin_close()?;
            // From here every error leaves the epoch unavailable. Keep the
            // original manager core and claim as quarantine; never remount it.
            core.close_and_drain().await?;
            token.finalize().await?;
            recorders.retire_cache(id, &core).await?;
            let mut entries = self
                .entries
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !entries
                .get(&id)
                .is_some_and(|entry| entry.shares_authority(&core))
            {
                return Err(CostPersistError::Rejected(
                    "retirement manager authority changed".into(),
                ));
            }
            entries.remove(&id);
            self.last_used
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
            // Drop every core clone before releasing creation_gate. This
            // releases the OS claim before a subsequent A -> B -> A mount.
        }
        Ok(())
    }
}

impl SessionStateCoordinator {
    fn retirement_projection_idle(&self) -> bool {
        !self.admission_closed.load(AtomicOrdering::Acquire)
            && self.durability_gate().frozen_reason().is_none()
            && self.fusion_outboxes().iter().all(|outbox| {
                matches!(
                    outbox.receipt.status,
                    FusionPublicationStatus::Published | FusionPublicationStatus::NotRequired
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::fusion_recorder::FusionTranscriptTarget;

    struct Fixture {
        directory: tempfile::TempDir,
        manager: Arc<SessionStateManager>,
        tracker: Arc<CostTracker>,
        budget: Arc<cost::BudgetEnforcer>,
        recorders: Arc<DesktopFusionRecorderFactory>,
        active: SessionId,
    }

    impl Fixture {
        async fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let manager = SessionStateManager::new_with_legacy_shadow(
                directory.path(),
                Some(Arc::new(|_| Some(75))),
            );
            let active = SessionId::new();
            let view = manager.ensure_coordinator(active).await.unwrap();
            let core = manager.coordinator_core(active).unwrap();
            let tracker = Arc::new(
                CostTracker::new(
                    active,
                    Arc::new(cost::PricingCatalog::builtin_reference()),
                    mpsc::channel(1).0,
                )
                .try_with_durable_persistence(
                    view.hydrate(active).await.unwrap(),
                    core.clone(),
                    core.writer_lease_core(),
                    core.durability_gate(),
                )
                .unwrap(),
            );
            let budget = Arc::new(cost::BudgetEnforcer::new(
                cost::BudgetConfig {
                    max_session_nano_usd: None,
                    max_turn_nano_usd: None,
                    max_turn_tokens: None,
                    warning_thresholds: vec![],
                    on_exceed: cost::BudgetExceedPolicy::Halt,
                },
                tracker.clone(),
            ));
            let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
                directory.path().join("transcript.jsonl"),
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    directory.path().to_path_buf(),
                )),
            ));
            let recorders = Arc::new(DesktopFusionRecorderFactory::new(
                manager.clone(),
                FusionTranscriptTarget::new(writer),
            ));
            Self {
                directory,
                manager,
                tracker,
                budget,
                recorders,
                active,
            }
        }

        async fn mount(&self, id: SessionId) {
            let view = self.manager.ensure_coordinator(id).await.unwrap();
            let core = self.manager.coordinator_core(id).unwrap();
            drop(
                self.tracker
                    .prepare_session_hydrated_with_durable(
                        id,
                        view.as_ref(),
                        core.clone(),
                        core.writer_lease_core(),
                        core.durability_gate(),
                    )
                    .await
                    .unwrap(),
            );
            drop(self.recorders.recorder_for_session(id).unwrap());
        }

        fn start_retention(&self) {
            self.manager
                .configure_retention(&self.tracker, &self.budget, &self.recorders);
        }

        async fn settled(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while self
                    .manager
                    .maintenance_running
                    .load(AtomicOrdering::Acquire)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned retirement finishes without another session switch");
        }
    }

    #[tokio::test]
    async fn retirement_keeps_64_eligible_and_excludes_active_pins_and_pending_outbox() {
        let f = Fixture::new().await;
        let pinned = SessionId::new();
        f.mount(pinned).await;
        let view = f.manager.coordinator(pinned).unwrap();
        let weak = Arc::downgrade(&view);
        let lease = view.writer_lease();
        drop(view);
        assert!(
            weak.upgrade().is_none(),
            "cache core cannot revive an external Weak view"
        );
        let pending = SessionId::new();
        f.mount(pending).await;
        let pending_view = f.manager.coordinator(pending).unwrap();
        let identity = FusionRunIdentity::new(
            platform_api::FusionRunId::generated(),
            Some(pending),
            platform_api::FusionOrigin::Slash,
            None,
        );
        let outbox = DurableFusionOutboxRecord {
            delivery_id: fusion_delivery_id(&identity),
            session_id: pending,
            message_uuid: protocol::MessageId::new().to_string(),
            payload: serde_json::json!({"message": "saved"}),
            attempt: 0,
            retry_cycle_end: 4,
            receipt: FusionPublicationReceipt::queued(),
        };
        pending_view
            .append_fusion_terminal(DurableFusionTerminalRecord {
                event_id: fusion_terminal_event_id(&identity),
                identity,
                result: Err(platform_api::FusionError::Internal),
                facts: platform_api::FusionRunFacts::default(),
                publication: FusionPublicationReceipt::queued(),
                outbox: Some(outbox.clone()),
            })
            .await
            .unwrap();
        assert_eq!(
            pending_view.fusion_outbox(&outbox.delivery_id),
            Some(outbox)
        );
        drop(pending_view);
        for _ in 0..65 {
            f.mount(SessionId::new()).await;
        }
        f.start_retention();
        f.settled().await;
        assert_eq!(f.manager.session_ids().len(), 67);
        assert!(f.manager.session_ids().contains(&f.active));
        assert!(f.manager.session_ids().contains(&pinned));
        assert!(f.manager.session_ids().contains(&pending));
        drop(lease); // no switch follows: last-pin callback must wake the owner
        f.settled().await;
        assert_eq!(f.manager.session_ids().len(), 66);
        assert!(!f.manager.session_ids().contains(&pinned));
        assert!(f.manager.session_ids().contains(&pending));
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_remount_reuses_wal_without_import_or_billing_replay() {
        let f = Fixture::new().await;
        let victim = SessionId::new();
        f.mount(victim).await;
        let old = f.manager.coordinator(victim).unwrap();
        let old_gate = old.durability_gate();
        let old_weak = Arc::downgrade(&old);
        let journal_root = old.journal().root().to_path_buf();
        assert_eq!(old.journal().replay().unwrap().entries.len(), 1);
        drop(old);
        for _ in 0..64 {
            f.mount(SessionId::new()).await;
        }
        f.start_retention();
        f.settled().await;
        assert!(!f.manager.session_ids().contains(&victim));
        assert!(!old_gate.retention_gate().is_live());
        assert!(old_weak.upgrade().is_none());
        assert!(
            journal_root.is_dir(),
            "retirement preserves the actual journal directory"
        );
        let (a, b) = tokio::join!(
            f.manager.ensure_coordinator(victim),
            f.manager.ensure_coordinator(victim)
        );
        let a = a.unwrap();
        let b = b.unwrap();
        assert_eq!(a.journal().root(), journal_root.as_path());
        assert!(
            a.shares_authority(&b),
            "concurrent A -> B -> A remounts acquire one claim"
        );
        assert!(!a
            .durability_gate()
            .retention_gate()
            .shares_authority(&old_gate.retention_gate()));
        assert_eq!(a.projection().unwrap().0.total_nano_usd, 75);
        assert_eq!(
            a.journal().replay().unwrap().entries.len(),
            1,
            "retirement never rewrites disk or replays a paid computation"
        );
        f.mount(victim).await;
        assert_eq!(f.tracker.scoped(victim).snapshot().await.total_nano_usd, 75);
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn retirement_is_owned_when_maintenance_waiter_is_cancelled() {
        let f = Fixture::new().await;
        for _ in 0..65 {
            f.mount(SessionId::new()).await;
        }
        let gate = f.manager.creation_gate.clone().lock_owned().await;
        f.start_retention();
        let owner = f.manager.clone();
        let waiter = tokio::spawn(async move {
            while owner.maintenance_running.load(AtomicOrdering::Acquire) {
                tokio::task::yield_now().await;
            }
        });
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        drop(gate);
        f.settled().await;
        assert_eq!(f.manager.session_ids().len(), 65);
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn issued_mutation_permit_pins_cache_core_until_cancelled_or_acknowledged() {
        let f = Fixture::new().await;
        let id = SessionId::new();
        f.mount(id).await;
        let core = f.manager.coordinator_core(id).unwrap();
        let permit = core.acquire_permit(id).await.unwrap();
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_none());
        drop(permit);
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_some());
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn recorder_and_cost_lease_views_pin_without_cached_weak_resurrection() {
        let f = Fixture::new().await;
        let id = SessionId::new();
        f.mount(id).await;
        let core = f.manager.coordinator_core(id).unwrap();
        let recorder = f.recorders.recorder_for_session(id).unwrap();
        let weak_recorder = Arc::downgrade(&recorder);
        let scoped = f.tracker.scoped(id);
        let lease = scoped.writer_lease().unwrap();
        let weak_cost = Arc::downgrade(&scoped);
        drop(scoped);
        assert!(weak_cost.upgrade().is_none());
        assert!(platform_api::live_sessions::same_writer_lease_authority(
            &lease,
            &core.writer_lease_core()
        ));
        drop(lease);
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_none());
        drop(recorder);
        assert!(weak_recorder.upgrade().is_none());
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_some());
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_ack_waiter_does_not_unpin_an_accepted_blocking_mutation() {
        let f = Fixture::new().await;
        let id = SessionId::new();
        f.mount(id).await;
        let core = f.manager.coordinator_core(id).unwrap();
        let lock_core = core.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        // Block the real mutation worker without retaining a synchronous
        // mutex guard in this async test future. Dropping release_tx on a
        // failed assertion also releases the blocking holder.
        let holder = tokio::task::spawn_blocking(move || {
            let _serial = lock_core.state.mutation_gate.lock().unwrap();
            let _ = ready_tx.send(());
            let _ = release_rx.recv();
        });
        ready_rx.await.unwrap();
        let mut next = core.projection().unwrap().0;
        next.cost_revision += 1;
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        core.acquire_permit(id)
            .await
            .unwrap()
            .enqueue(CostPersistRequest {
                session_id: id,
                cost_revision: next.cost_revision,
                mutation_id: CostMutationId::new("retention-cancelled-ack"),
                state: CostStateVector::from(&next),
                source: CostMutationSource::ModelResponse,
                ack: ack_tx,
            })
            .unwrap();
        drop(ack_rx);
        tokio::task::yield_now().await;
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_none());
        release_tx.send(()).unwrap();
        holder.await.unwrap();
        core.flush().await.unwrap();
        assert!(f
            .tracker
            .prepare_session_retirement(id, &core.durability_gate(), f.budget.clone())
            .await
            .unwrap()
            .is_some());
        f.manager.close_and_drain().await.unwrap();
    }

    #[tokio::test]
    async fn queued_maintenance_cannot_retain_writer_claim_after_shutdown_returns() {
        use std::future::{poll_fn, Future};
        use std::task::Poll;

        let f = Fixture::new().await;
        f.start_retention();
        f.settled().await;
        let manager = f.manager.clone();
        let gate = manager.creation_gate.clone().lock_owned().await;
        let mut shutdown = Box::pin(manager.close_and_drain());
        poll_fn(|cx| {
            assert!(shutdown.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        // Poll exactly once behind shutdown, then deliberately leave this
        // future unpolled even after close returns. It must own only weak
        // authorities while queued, not a tracker retaining every OS claim.
        let mut maintenance = Box::pin(manager.retire_excess());
        poll_fn(|cx| {
            assert!(maintenance.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(gate);
        shutdown.await.unwrap();
        let Fixture {
            directory,
            manager: fixture_manager,
            tracker,
            budget,
            recorders,
            active,
        } = f;
        drop(recorders);
        drop(budget);
        drop(tracker);
        drop(fixture_manager);
        let claim =
            platform_api::live_sessions::LiveSessionDir::at_live(directory.path().join("sessions"))
                .claim_session_id(&active.to_string(), std::process::id());
        assert!(
            claim.is_ok(),
            "unpolled maintenance cannot delay shutdown claim release"
        );
        drop(claim);
        maintenance.await.unwrap();
    }
}
