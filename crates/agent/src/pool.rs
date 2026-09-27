//! Host-owned slot table that holds concurrent subagent state machines.
//!
//! Subagents are not nested state machines — each spawn becomes a sibling
//! slot in the [`StateMachinePool`] that the host drives via channels. This
//! avoids stack growth in deep fork chains and gives unified
//! scheduling / cancellation. See spec §10.5.

mod capacity;
use capacity::CapacityCore;
pub(crate) use capacity::TrackedPoolPermit;

use crate::context::SubagentContext;
use crate::runner::SubagentEvent;
use platform_api::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
use protocol::AgentId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
#[cfg(test)]
use tokio::sync::{Notify, Semaphore};

/// One slot in the [`StateMachinePool`].
///
/// The slot stores the [`BackgroundTaskHandle`] driving the subagent runner
/// plus the sender side of the slot's event channel; the receiver is moved
/// into [`crate::runner::run_subagent`] at allocation time.
pub struct StateMachineSlot {
    /// Agent id this slot is keyed by.
    pub agent_id: AgentId,
    /// Handle to the background task running the subagent.
    pub task: BackgroundTaskHandle,
    /// Sender used by the host to deliver `Event`s into the slot.
    pub event_tx: mpsc::Sender<lingxi_core::Event>,
    /// Capacity permit held for the entire lifetime of this slot.
    _capacity_permit: Arc<TrackedPoolPermit>,
    _identity_reservation: Arc<IdentityReservation>,
}

/// Keeps one pool identity exclusive through construction, execution and cleanup.
pub(crate) struct IdentityReservation {
    identities: Arc<std::sync::Mutex<HashSet<AgentId>>>,
    agent_id: AgentId,
    claimed: std::sync::atomic::AtomicBool,
}
impl Drop for IdentityReservation {
    fn drop(&mut self) {
        self.identities.lock().unwrap().remove(&self.agent_id);
    }
}

/// Fixed-capacity table of active subagent slots.
///
/// `max_concurrent` caps how many slots can be alive at once. Allocation
/// hands the caller back an [`mpsc::Receiver`] of [`SubagentEvent`]s the
/// runner emits as it processes the conversation.
pub struct StateMachinePool {
    identities: Arc<std::sync::Mutex<HashSet<AgentId>>>,
    slots: Arc<RwLock<HashMap<AgentId, StateMachineSlot>>>,
    capacity: Arc<CapacityCore>,
    runtime: Arc<dyn RuntimeSpawner>,
    #[cfg(test)]
    post_spawn_wait: Arc<RwLock<Option<Arc<Notify>>>>,
}

/// Cancel-safety guard for [`StateMachinePool::allocate`].
///
/// `allocate` starts the runner before the slot is inserted into `slots`. If the
/// future is dropped in that window, the spawned runner must still be canceled;
/// otherwise a persistent child can outlive its caller with no way to recover
/// its task handle or pool slot.
struct AllocateCancelGuard {
    runtime: Arc<dyn RuntimeSpawner>,
    task: Option<BackgroundTaskHandle>,
}

impl AllocateCancelGuard {
    fn new(runtime: Arc<dyn RuntimeSpawner>, task: BackgroundTaskHandle) -> Self {
        Self {
            runtime,
            task: Some(task),
        }
    }

    fn disarm(&mut self) {
        self.task = None;
    }
}

impl Drop for AllocateCancelGuard {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let runtime = self.runtime.clone();
            handle.spawn(async move {
                let _ = runtime.cancel(&task).await;
            });
        }
    }
}

impl StateMachinePool {
    /// Construct a pool with capacity for `max_concurrent` slots, driven by
    /// the provided [`RuntimeSpawner`].
    #[must_use]
    pub fn new(runtime: Arc<dyn RuntimeSpawner>, max_concurrent: usize) -> Self {
        Self {
            identities: Arc::new(std::sync::Mutex::new(HashSet::new())),
            slots: Arc::new(RwLock::new(HashMap::new())),
            capacity: CapacityCore::new(max_concurrent)
                .expect("pool capacity exceeds semaphore maximum"),
            runtime,
            #[cfg(test)]
            post_spawn_wait: Arc::new(RwLock::new(None)),
        }
    }

    /// The runtime this pool spawns on. A sibling pool built for a different
    /// class of caller drives the same runtime.
    #[must_use]
    pub fn runtime(&self) -> Arc<dyn RuntimeSpawner> {
        self.runtime.clone()
    }

    #[cfg(test)]
    pub async fn set_post_spawn_wait(&self, notify: Arc<Notify>) {
        *self.post_spawn_wait.write().await = Some(notify);
    }

    /// Allocate a slot. Returns `(agent_id, event_rx)` where `event_rx` is the
    /// channel emitting [`SubagentEvent`]s for the new spawn. The
    /// `event_tx` for inbound `lingxi_core::Event`s is owned by the slot
    /// table and reachable via [`Self::send_event`] in later milestones.
    pub async fn allocate(
        &self,
        ctx: SubagentContext,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        self.allocate_with_receipt(ctx, None).await
    }

    /// Allocate a slot and synchronously publish the fact that the runner was
    /// created by the runtime.  The receipt fires before the test hook and the
    /// slot-table insertion below can suspend.  This is intentionally separate
    /// from the async lifecycle observer: a slow observer must not make a real
    /// allocation look like a zero-allocation spawn to Fusion's quota logic.
    pub async fn allocate_with_receipt(
        &self,
        ctx: SubagentContext,
        allocation_receipt: Option<Arc<dyn Fn(AgentId) + Send + Sync>>,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        self.allocate_with_startup(ctx, allocation_receipt, None)
            .await
    }

    /// Reserve an identity before constructing any identity-scoped resources.
    pub(crate) fn reserve_identity(
        &self,
        agent_id: AgentId,
    ) -> Result<Arc<IdentityReservation>, PoolError> {
        if !self.identities.lock().unwrap().insert(agent_id) {
            return Err(PoolError::AgentAlreadyExists);
        }
        Ok(Arc::new(IdentityReservation {
            identities: self.identities.clone(),
            agent_id,
            claimed: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    /// Reserve a real slot while its runner waits for startup registration.
    pub async fn allocate_with_startup(
        &self,
        ctx: SubagentContext,
        allocation_receipt: Option<Arc<dyn Fn(AgentId) + Send + Sync>>,
        startup: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        self.allocate_with_reserved_identity(ctx, allocation_receipt, startup, None)
            .await
    }

    /// Consume an optional pre-construction reservation exactly once.
    pub(crate) async fn allocate_with_reserved_identity(
        &self,
        ctx: SubagentContext,
        allocation_receipt: Option<Arc<dyn Fn(AgentId) + Send + Sync>>,
        startup: Option<tokio::sync::oneshot::Receiver<()>>,
        reserved: Option<Arc<IdentityReservation>>,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        // Reserve capacity atomically before spawning the runner. A len/read
        // check can race when multiple parallel Agent tool calls allocate at
        // once and let all of them pass the same stale count.
        let permit = self
            .capacity
            .acquire_ordinary()
            .map_err(|_| PoolError::TooManyAgents)?;
        self.allocate_admitted(ctx, allocation_receipt, permit, startup, reserved)
            .await
    }

    pub(crate) async fn reserve_panel_group(
        &self,
        count: usize,
        deadline: tokio::time::Instant,
        cancel: platform_api::panel_pool::PanelAdmissionCancellation,
    ) -> Result<platform_api::PanelPoolLease, platform_api::SubagentSpawnError> {
        self.capacity
            .reserve_group(count, deadline, cancel)
            .await
            .map(|mut permits| {
                let drain = capacity::PoolGroupDrain::new(permits.len());
                for permit in &mut permits {
                    permit.track_group(drain.clone());
                }
                platform_api::PanelPoolLease::with_drain(
                    permits
                        .into_iter()
                        .map(platform_api::PanelPoolPermit::new)
                        .collect(),
                    drain,
                )
            })
            .map_err(|error| {
                platform_api::SubagentSpawnError::Runtime(format!(
                    "Fusion panel admission rejected: {error:?}"
                ))
            })
    }

    pub(crate) fn take_panel_permit(
        &self,
        permit: platform_api::PanelPoolPermit,
    ) -> Result<TrackedPoolPermit, PoolError> {
        let permit = permit
            .into_inner::<TrackedPoolPermit>()
            .ok_or(PoolError::WrongPool)?;
        if !self.capacity.owns(&permit) {
            return Err(PoolError::WrongPool);
        }
        Ok(permit)
    }

    /// Consume a permit this pool already granted, for a caller that was
    /// admitted as part of a whole panel group rather than one at a time.
    pub(crate) async fn allocate_admitted_with_receipt(
        &self,
        ctx: SubagentContext,
        allocation_receipt: Option<Arc<dyn Fn(AgentId) + Send + Sync>>,
        permit: TrackedPoolPermit,
        startup: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        self.allocate_admitted(ctx, allocation_receipt, permit, startup, None)
            .await
    }

    /// The one allocation body. Capacity is already held by the time it runs;
    /// the identity reservation and the startup gate are the caller's choice.
    async fn allocate_admitted(
        &self,
        ctx: SubagentContext,
        allocation_receipt: Option<Arc<dyn Fn(AgentId) + Send + Sync>>,
        permit: TrackedPoolPermit,
        startup: Option<tokio::sync::oneshot::Receiver<()>>,
        reserved: Option<Arc<IdentityReservation>>,
    ) -> Result<(AgentId, mpsc::Receiver<SubagentEvent>), PoolError> {
        if !self.capacity.owns(&permit) {
            return Err(PoolError::WrongPool);
        }
        let capacity_permit = Arc::new(permit);
        let agent_id = ctx.agent_id;
        let identity_reservation = match reserved {
            Some(reservation)
                if reservation.agent_id == agent_id
                    && Arc::ptr_eq(&reservation.identities, &self.identities) =>
            {
                reservation
            }
            Some(_) => return Err(PoolError::AgentAlreadyExists),
            None => self.reserve_identity(agent_id)?,
        };
        if identity_reservation
            .claimed
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(PoolError::AgentAlreadyExists);
        }
        let runner_identity_reservation = identity_reservation.clone();
        let (event_tx, event_rx) = mpsc::channel::<lingxi_core::Event>(100);
        let (out_tx, out_rx) = mpsc::channel::<SubagentEvent>(100);

        // Capture before spawn/first poll: cancellation acknowledgement is
        // not proof the accepted runner was destroyed. Both the local/slot
        // owner and the actual runner must release before capacity returns.
        let runner_capacity = capacity_permit.clone();
        let task = self
            .runtime
            .spawn(
                "subagent-state-machine",
                Box::pin(async move {
                    let _capacity = runner_capacity;
                    // Keep the ID reserved even when allocation is cancelled
                    // before the slot is published and cancel runs asynchronously.
                    let _identity_reservation = runner_identity_reservation;
                    if let Some(startup) = startup {
                        if startup.await.is_err() {
                            return;
                        }
                    }
                    crate::runner::run_subagent(ctx, event_rx, out_tx).await;
                }),
            )
            .await?;
        let mut cancel_guard = AllocateCancelGuard::new(self.runtime.clone(), task.clone());

        // The runtime has accepted and started the child task. Publish this
        // fact before any subsequent await; if the caller is cancelled while
        // waiting for the slot-table write, the receipt still prevents a
        // real child from being refunded as "never allocated".
        if let Some(receipt) = allocation_receipt {
            receipt(agent_id);
        }

        #[cfg(test)]
        if let Some(wait) = self.post_spawn_wait.read().await.clone() {
            wait.notified().await;
        }

        self.slots.write().await.insert(
            agent_id,
            StateMachineSlot {
                agent_id,
                task,
                event_tx,
                _capacity_permit: capacity_permit,
                _identity_reservation: identity_reservation,
            },
        );
        cancel_guard.disarm();
        Ok((agent_id, out_rx))
    }

    /// Deliver an inbound [`lingxi_core::Event`] into the slot owned by `agent_id`.
    ///
    /// Used by message-driven handlers (e.g. the in-process teammate) to push
    /// a [`lingxi_core::Event::UserMessage`] to a parked subagent, and to deliver
    /// [`lingxi_core::Event::UserExit`] / [`lingxi_core::Event::UserInterrupt`] for
    /// cooperative termination.
    ///
    /// A `read()` lock suffices: we only read the `event_tx` sender, and the
    /// mpsc channel is internally synchronized.
    pub async fn send_event(
        &self,
        agent_id: &AgentId,
        event: lingxi_core::Event,
    ) -> Result<(), PoolError> {
        let slots = self.slots.read().await;
        let slot = slots.get(agent_id).ok_or(PoolError::NoSuchAgent)?;
        slot.event_tx
            .send(event)
            .await
            .map_err(|_| PoolError::AgentGone)?;
        Ok(())
    }

    /// Remove the slot owned by `agent_id` and cancel its background task.
    pub async fn deallocate(&self, agent_id: &AgentId) -> Result<(), PoolError> {
        let slot = self.slots.write().await.remove(agent_id);
        if let Some(slot) = slot {
            self.runtime.cancel(&slot.task).await?;
        }
        Ok(())
    }

    /// Number of currently active slots.
    pub async fn slot_count(&self) -> usize {
        self.slots.read().await.len()
    }

    /// True once the runner for `agent_id` has actually reached its terminal
    /// state — either the slot is already gone, or the slot's inbound event
    /// channel is closed because [`crate::runner::run_subagent`] dropped its
    /// `event_rx` on return (the same signal [`Self::send_event`] surfaces as
    /// [`PoolError::AgentGone`]).
    ///
    /// Used by `SpawnDeallocGuard`'s early-drop path (`handle.rs`) to poll
    /// down the fixed cancel grace once the runner is genuinely done, instead
    /// of blindly holding the slot — and the capacity permit stored inside it
    /// (`_capacity_permit` above) — for the whole grace window regardless of
    /// how quickly the runner actually stopped.
    pub async fn agent_runner_finished(&self, agent_id: &AgentId) -> bool {
        match self.slots.read().await.get(agent_id) {
            Some(slot) => slot.event_tx.is_closed(),
            None => true,
        }
    }
}

/// Failure modes for [`StateMachinePool`] operations.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    /// A transferred capacity owner did not originate in this pool.
    #[error("panel permit belongs to a different pool")]
    WrongPool,
    /// A live or currently allocating runner already owns this stable identity.
    #[error("agent identity is already allocated")]
    AgentAlreadyExists,
    /// All `max_concurrent` slots are in use.
    #[error("too many agents — pool full")]
    TooManyAgents,
    /// No slot is currently allocated for the requested `agent_id`.
    #[error("no agent with the requested id")]
    NoSuchAgent,
    /// The slot exists but its inbound event channel is closed (the subagent
    /// runner future has dropped its receiver — effectively terminated).
    #[error("agent channel closed")]
    AgentGone,
    /// The underlying [`RuntimeSpawner`] rejected the spawn / cancel call.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::SubagentContext;
    use crate::definition::{
        AgentDefinition, AgentModel, AgentPermissionMode, AgentSource, AgentToolPolicy,
    };
    use crate::display::{AgentColor, AgentDisplay};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use test_harness::mocks::MockRuntimeSpawner;

    /// Build a minimal [`SubagentContext`] with `api_client = None`, so the
    /// allocated slot runs the legacy reducer-driven stub (no API calls).
    fn make_ctx() -> SubagentContext {
        SubagentContext {
            task_registry: None,
            agent_id: AgentId::new(),
            parent_agent_id: None,
            agent_name: None,
            team_name: None,
            agent_definition: AgentDefinition {
                cache_ttl: None,
                agent_type: "test".into(),
                when_to_use: String::new(),
                tools: AgentToolPolicy::All {
                    use_exact_tools: true,
                },
                max_turns: 1,
                model: AgentModel::Inherit,
                permission_mode: AgentPermissionMode::Bubble,
                source: AgentSource::BuiltIn,
                base_dir: "/tmp".into(),
                system_prompt: None,
                mcp_servers: vec![],
                frontmatter_hooks: vec![],
                icon: None,
                allowed_tools: vec![],
                worktree_requirement: None,
                disallowed_tools: vec![],
                skills: vec![],
                required_mcp_servers: vec![],
                background: false,
                isolation: None,
                memory: None,
                effort: None,
                initial_prompt: None,
                color: None,
                observer: None,
            },
            prompt_messages: vec![],
            fork_context_messages: None,
            allowed_tools: vec![],
            worktree_handle: None,
            cwd: None,
            is_async: false,
            persistent: false,
            can_show_permission_prompts: true,
            session_interactive: None,
            origin_session_id: None,
            mcp_clients: vec![],
            transcript_subdir: "/tmp".into(),
            transcript_fs: None,
            resumed_history: None,
            rendered_system_prompt: None,
            mobile_runtime_environment_reminder: None,
            mobile_runtime_workspace_reminder: None,
            content_replacement_state: None,
            agent_memory: None,
            display: AgentDisplay {
                color: AgentColor::Cyan,
                icon: None,
            },
            model_profile: None,
            api_client: None,
            tool_invoker: None,
            new_diagnostics_source: None,
            tool_schemas: vec![],
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            budget: None,
            hook_executor: None,
            strict_plugin_only_hooks: false,
            skill_loader: None,
            hook_session_id: protocol::SessionId::nil(),
            hook_cwd: std::path::PathBuf::new(),
            depth: 0,
            observer: None,
            permission_mode_override: None,
            frozen_command_denies: Vec::new(),
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
            refusal_fallback_chain: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reserved_identity_cannot_be_retargeted_or_claimed_twice() {
        let pool = StateMachinePool::new(Arc::new(MockRuntimeSpawner::default()), 3);
        let id = AgentId::new();
        let reservation = pool.reserve_identity(id).unwrap();
        assert!(matches!(
            pool.allocate_with_reserved_identity(make_ctx(), None, None, Some(reservation.clone()))
                .await,
            Err(PoolError::AgentAlreadyExists)
        ));
        assert_eq!(pool.slot_count().await, 0);
        let mut context = make_ctx();
        context.agent_id = id;
        let (release, startup) = tokio::sync::oneshot::channel();
        pool.allocate_with_reserved_identity(
            context,
            None,
            Some(startup),
            Some(reservation.clone()),
        )
        .await
        .unwrap();
        let mut duplicate = make_ctx();
        duplicate.agent_id = id;
        assert!(matches!(
            pool.allocate_with_reserved_identity(duplicate, None, None, Some(reservation))
                .await,
            Err(PoolError::AgentAlreadyExists)
        ));
        assert_eq!(pool.slot_count().await, 1);
        drop(release);
        pool.deallocate(&id).await.unwrap();
    }

    #[tokio::test]
    async fn stable_identity_collision_cannot_replace_a_live_or_prepared_runner() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 2);
        let context = make_ctx();
        let id = context.agent_id;
        let (release, startup) = tokio::sync::oneshot::channel();
        let (first_id, _events) = pool
            .allocate_with_startup(context, None, Some(startup))
            .await
            .unwrap();
        let mut duplicate = make_ctx();
        duplicate.agent_id = id;
        assert!(matches!(
            pool.allocate(duplicate).await,
            Err(PoolError::AgentAlreadyExists)
        ));
        assert_eq!(first_id, id);
        assert_eq!(pool.slot_count().await, 1);
        assert!(!pool.agent_runner_finished(&id).await);
        drop(release);
        pool.deallocate(&id).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while pool.identities.lock().unwrap().contains(&id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled runner releases its identity once it has actually stopped");
        let mut retry = make_ctx();
        retry.agent_id = id;
        assert!(
            pool.allocate(retry).await.is_ok(),
            "deallocation releases the identity reservation"
        );
        pool.deallocate(&id).await.unwrap();
    }

    #[tokio::test]
    async fn allocated_runner_waits_for_acknowledged_startup_gate() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 1);
        let (release, startup) = tokio::sync::oneshot::channel();
        let (id, mut events) = pool
            .allocate_with_startup(make_ctx(), None, Some(startup))
            .await
            .unwrap();
        pool.send_event(&id, lingxi_core::Event::UserExit)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), events.recv())
                .await
                .is_err(),
            "runner must not execute before its owner registration completes"
        );
        release.send(()).unwrap();
        assert!(matches!(
            tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
                .await
                .unwrap(),
            Some(SubagentEvent::Killed { .. })
        ));
        pool.deallocate(&id).await.unwrap();
    }

    #[tokio::test]
    async fn allocate_and_deallocate() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 5);

        let ctx = make_ctx();
        let aid = ctx.agent_id;
        let (id, _rx) = pool.allocate(ctx).await.unwrap();
        assert_eq!(id, aid);
        assert_eq!(pool.slot_count().await, 1);
        pool.deallocate(&aid).await.unwrap();
        assert_eq!(pool.slot_count().await, 0);
    }

    #[tokio::test]
    async fn admitted_group_transfers_slots_without_reacquiring() {
        let pool = StateMachinePool::new(Arc::new(MockRuntimeSpawner::default()), 2);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let lease = pool
            .reserve_panel_group(
                2,
                deadline,
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            )
            .await
            .unwrap();
        assert_eq!(pool.capacity.available_permits(), 0);
        assert_eq!(pool.slot_count().await, 0, "reservation is not allocation");
        assert!(matches!(
            pool.allocate(make_ctx()).await,
            Err(PoolError::TooManyAgents)
        ));
        let mut ids = Vec::new();
        for permit in lease.into_permits() {
            let permit = pool.take_panel_permit(permit).unwrap();
            let (id, _events) = pool
                .allocate_admitted_with_receipt(make_ctx(), None, permit, None)
                .await
                .unwrap();
            ids.push(id);
        }
        assert_eq!(pool.slot_count().await, 2);
        for id in ids {
            pool.deallocate(&id).await.unwrap();
        }
        let returned = pool
            .reserve_panel_group(
                2,
                deadline,
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            )
            .await
            .unwrap();
        drop(returned);
        assert_eq!(pool.capacity.available_permits(), 2);
    }

    #[tokio::test]
    async fn admitted_wrong_pool_and_unpolled_drop_return_original_capacity() {
        let pool = StateMachinePool::new(Arc::new(MockRuntimeSpawner::default()), 1);
        let other = StateMachinePool::new(Arc::new(MockRuntimeSpawner::default()), 1);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let permit = pool
            .reserve_panel_group(
                1,
                deadline,
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            )
            .await
            .unwrap()
            .into_permits()
            .pop()
            .unwrap();
        assert!(matches!(
            other.take_panel_permit(permit),
            Err(PoolError::WrongPool)
        ));
        assert_eq!(pool.capacity.available_permits(), 1);
        assert_eq!(other.capacity.available_permits(), 1);
        let permit = pool
            .reserve_panel_group(
                1,
                deadline,
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            )
            .await
            .unwrap()
            .into_permits()
            .pop()
            .unwrap();
        let permit = pool.take_panel_permit(permit).unwrap();
        drop(pool.allocate_admitted_with_receipt(make_ctx(), None, permit, None));
        assert_eq!(pool.capacity.available_permits(), 1);
        assert_eq!(pool.slot_count().await, 0);
    }

    /// The allocation fact must be published immediately after the runtime
    /// task is created, before the slot-table insertion can suspend.  Fusion
    /// uses this receipt for quota/accounting; waiting for the normal async
    /// lifecycle observer would leave a cancellation window in which a real
    /// child exists but the caller still believes that nothing was allocated.
    #[tokio::test]
    async fn allocation_receipt_precedes_slot_table_wait() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = Arc::new(StateMachinePool::new(runtime, 1));
        let wait = Arc::new(Notify::new());
        pool.set_post_spawn_wait(wait).await;
        let receipts = Arc::new(AtomicUsize::new(0));
        let receipt_counter = Arc::clone(&receipts);
        let receipt: Arc<dyn Fn(AgentId) + Send + Sync> = Arc::new(move |_agent_id| {
            receipt_counter.fetch_add(1, Ordering::SeqCst);
        });

        let allocation = tokio::spawn({
            let pool = Arc::clone(&pool);
            async move { pool.allocate_with_receipt(make_ctx(), Some(receipt)).await }
        });
        for _ in 0..100 {
            if receipts.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            receipts.load(Ordering::SeqCst),
            1,
            "allocation receipt must not wait for the post-spawn slot-table suspension"
        );

        // Drop the allocation future while it is parked before slot insertion.
        // The cancel guard owns the just-created runtime task, so this must not
        // leave a live pool slot behind even though the receipt already fired.
        allocation.abort();
        let _ = allocation.await;
        tokio::task::yield_now().await;
        assert_eq!(pool.slot_count().await, 0);
    }

    #[tokio::test]
    async fn dropped_allocation_keeps_capacity_until_runner_cancellation_finishes() {
        struct CancelGate {
            inner: MockRuntimeSpawner,
            entered: Semaphore,
            release: Semaphore,
        }
        #[async_trait::async_trait]
        impl RuntimeSpawner for CancelGate {
            async fn spawn(
                &self,
                name: &str,
                task: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
            ) -> Result<BackgroundTaskHandle, RuntimeError> {
                // Keep the accepted runner future alive independently of its
                // inbound channel, until the runtime actually cancels it.
                self.inner
                    .spawn(
                        name,
                        Box::pin(async move {
                            std::future::pending::<()>().await;
                            drop(task);
                        }),
                    )
                    .await
            }
            async fn sleep(&self, duration: std::time::Duration) {
                self.inner.sleep(duration).await;
            }
            async fn cancel(&self, task: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                self.inner.cancel(task).await
            }
        }
        let runtime = Arc::new(CancelGate {
            inner: MockRuntimeSpawner::default(),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        });
        let pool = Arc::new(StateMachinePool::new(runtime.clone(), 1));
        pool.set_post_spawn_wait(Arc::new(Notify::new())).await;
        let allocated = Arc::new(Semaphore::new(0));
        let receipt: Arc<dyn Fn(AgentId) + Send + Sync> = Arc::new({
            let allocated = allocated.clone();
            move |_| allocated.add_permits(1)
        });
        let task = tokio::spawn({
            let pool = pool.clone();
            async move { pool.allocate_with_receipt(make_ctx(), Some(receipt)).await }
        });
        allocated.acquire().await.unwrap().forget();
        task.abort();
        let _ = task.await;
        runtime.entered.acquire().await.unwrap().forget();
        let available_before_stop = pool.capacity.available_permits();
        // Always unblock cleanup, including on the failing baseline assertion.
        runtime.release.add_permits(1);
        assert_eq!(
            available_before_stop, 0,
            "the runner still owns physical capacity"
        );
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            pool.capacity.reserve_group(
                1,
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        drop(permit);
        assert_eq!(pool.slot_count().await, 0);
    }

    #[tokio::test]
    async fn cancellation_ack_does_not_release_a_live_runner_capacity() {
        type Runner = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
        #[derive(Default)]
        struct CooperativeRuntime {
            runner: std::sync::Mutex<Option<Runner>>,
        }
        #[async_trait::async_trait]
        impl RuntimeSpawner for CooperativeRuntime {
            async fn spawn(
                &self,
                name: &str,
                task: Runner,
            ) -> Result<BackgroundTaskHandle, RuntimeError> {
                *self.runner.lock().unwrap() = Some(task);
                Ok(BackgroundTaskHandle {
                    task_name: name.to_owned(),
                    task_id: 1,
                })
            }
            async fn sleep(&self, duration: std::time::Duration) {
                tokio::time::sleep(duration).await;
            }
            async fn cancel(&self, _: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
                // Acknowledgement requests shutdown, not future destruction.
                Ok(())
            }
        }
        let runtime = Arc::new(CooperativeRuntime::default());
        let pool = StateMachinePool::new(runtime.clone(), 1);
        let lease = pool
            .reserve_panel_group(
                1,
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                platform_api::panel_pool::PanelAdmissionCancellation::new(),
            )
            .await
            .unwrap();
        let (mut permits, drain) = lease.into_parts();
        let drain = drain.expect("production group carries a producer drain");
        let permit = pool.take_panel_permit(permits.pop().unwrap()).unwrap();
        let (id, _events) = pool
            .allocate_admitted_with_receipt(make_ctx(), None, permit, None)
            .await
            .unwrap();
        pool.deallocate(&id).await.unwrap();
        assert_eq!(pool.slot_count().await, 0);
        let available_before_drop = pool.capacity.available_permits();
        let mut draining = Box::pin(drain.wait());
        assert!(
            futures::poll!(&mut draining).is_pending(),
            "cancel ack cannot complete producer drain"
        );
        drop(runtime.runner.lock().unwrap().take());
        assert_eq!(
            available_before_drop, 0,
            "cancel ack is not runner destruction"
        );
        assert_eq!(pool.capacity.available_permits(), 1);
        draining.await;
    }

    /// `send_event` routes an inbound `lingxi_core::Event` into the slot's runner.
    /// We drive the stub runner (api_client = None): a `UserExit` delivered via
    /// `send_event` makes it emit `SubagentEvent::Killed`, proving the event
    /// reached the slot's `event_tx`.
    #[tokio::test]
    async fn send_event_delivers_to_slot() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 5);

        let ctx = make_ctx();
        let aid = ctx.agent_id;
        let (_id, mut out_rx) = pool.allocate(ctx).await.unwrap();

        pool.send_event(&aid, lingxi_core::Event::UserExit)
            .await
            .expect("send_event delivers to the live slot");

        // The stub runner's fast-path turns UserExit into Killed.
        let ev = out_rx.recv().await.expect("a SubagentEvent");
        assert!(
            matches!(ev, SubagentEvent::Killed { agent_id } if agent_id == aid),
            "UserExit routed through send_event yields Killed; got {ev:?}"
        );
    }

    /// `send_event` to an id with no slot is `NoSuchAgent`.
    #[tokio::test]
    async fn send_event_unknown_agent_errors() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 5);
        let err = pool
            .send_event(&AgentId::new(), lingxi_core::Event::UserInterrupt)
            .await
            .unwrap_err();
        assert!(matches!(err, PoolError::NoSuchAgent), "got {err:?}");
    }

    /// Allocating beyond `max_concurrent` is rejected with `TooManyAgents`. A
    /// core invariant of the fixed-capacity slot table.
    #[tokio::test]
    async fn allocate_over_capacity_rejects() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 1);

        // First slot fits.
        let (_id, _rx) = pool.allocate(make_ctx()).await.unwrap();
        assert_eq!(pool.slot_count().await, 1);

        // Second slot exceeds the cap.
        let err = pool.allocate(make_ctx()).await.unwrap_err();
        assert!(matches!(err, PoolError::TooManyAgents), "got {err:?}");
        assert_eq!(pool.slot_count().await, 1, "rejected spawn left no slot");
    }

    #[tokio::test]
    async fn parallel_allocations_cannot_race_past_capacity() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = Arc::new(StateMachinePool::new(runtime, 1));
        let (left, right) = tokio::join!(pool.allocate(make_ctx()), pool.allocate(make_ctx()));
        assert_eq!(usize::from(left.is_ok()) + usize::from(right.is_ok()), 1);
        assert!(
            matches!(left, Err(PoolError::TooManyAgents))
                || matches!(right, Err(PoolError::TooManyAgents))
        );
        assert_eq!(pool.slot_count().await, 1);
    }

    /// `send_event` to a slot whose runner has dropped its inbound receiver
    /// (the run future returned, e.g. after a terminal event) surfaces
    /// `AgentGone` — distinct from `NoSuchAgent` (slot absent). The slot is
    /// still in the map, so this exercises the closed-channel arm specifically.
    #[tokio::test]
    async fn send_event_to_dropped_runner_is_agent_gone() {
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let pool = StateMachinePool::new(runtime, 5);

        let aid = {
            let ctx = make_ctx();
            let id = ctx.agent_id;
            let (_id, mut out_rx) = pool.allocate(ctx).await.unwrap();
            // Drive the stub runner to termination: UserExit makes it emit
            // Killed and return, dropping its `event_rx`. We never deallocate,
            // so the slot stays in the map with a now-closed inbound channel.
            pool.send_event(&id, lingxi_core::Event::UserExit)
                .await
                .unwrap();
            // Wait for the runner to actually surface Killed and return so its
            // receiver is dropped before we probe the closed channel.
            let ev = out_rx.recv().await.expect("Killed event");
            assert!(matches!(ev, SubagentEvent::Killed { .. }), "got {ev:?}");
            // The out channel closes once the runner returns.
            while out_rx.recv().await.is_some() {}
            id
        };

        // The slot is still present (not deallocated) but the runner's
        // receiver is gone -> send fails with AgentGone.
        let err = pool
            .send_event(
                &aid,
                lingxi_core::Event::UserMessage {
                    message_id: protocol::MessageId::new(),
                    request_id: protocol::RequestId::new(),
                    content: "hello".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, PoolError::AgentGone), "got {err:?}");
    }
}
