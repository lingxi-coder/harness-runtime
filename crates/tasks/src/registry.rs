//! Central registry tracking running tasks.
//!
//! The registry owns the in-memory map of task IDs to [`TaskState`] and to
//! the [`BackgroundTaskHandle`]s returned by the [`RuntimeSpawner`].

use crate::handlers::{
    DreamHandler, InProcessTeammateHandler, LocalAgentHandler, LocalBashHandler,
    LocalFusionHandler, MonitorHandler, MonitorMcpHandler,
};
use crate::id::{generate_task_id, TaskType};
use crate::output_manager::TaskOutputManager;
use crate::state::{TaskState, TaskStateBase, TaskStatus};
use crate::task_trait::{Task, TaskContext, TaskError, TaskSpawnInput};
use agent::{StateMachinePool, SubagentApiClient};
use async_trait::async_trait;
use lingxi_core::host::team_spawn::{TeamSpawnError, TeamSpawnSeam};
use lingxi_core::host::{
    BackgroundTaskHandle, BudgetEnforcerHandle, FileSystem, FusionActivation, FusionCompletionSink,
    FusionExecutor, ProcessRunner, RuntimeSpawner, Sandbox, SubagentSpawner, ToolInvoker,
};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;

/// Tracks running tasks and dispatches lifecycle operations to handlers.
#[path = "observer.rs"]
mod observer;
#[path = "shell_adoption.rs"]
mod shell_adoption;

#[path = "human_messages.rs"]
mod human_messages;

#[path = "handback.rs"]
mod handback;

pub struct TaskRegistry {
    /// Weak self-reference installed once the composition root wraps the
    /// registry in `Arc`.  Cancellation-sensitive task transactions use it to
    /// outlive the caller's future without creating a reference cycle.
    self_ref: std::sync::OnceLock<std::sync::Weak<TaskRegistry>>,
    /// The session's observer pairing table, installed by the composition root.
    /// Shared with `ToolUseContext::observer_pairings` so `ObserverReport` and
    /// the spawn site below agree on the same pairings — an observer spawned
    /// here must be resolvable by the tool it reports through.
    observer_pairings:
        std::sync::OnceLock<Arc<lingxi_core::host::observer_pairing::ObserverPairings>>,
    human_messages: std::sync::Mutex<HashMap<String, human_messages::HumanInbox>>,
    /// Report acceptance and its receipt are one owned transaction. Stops and
    /// run replacement cross this fence before invalidating a live token.
    handback_transactions: tokio::sync::Mutex<()>,
    handback_runs: std::sync::Mutex<
        HashMap<lingxi_core::types::AgentId, lingxi_core::host::handback::HandbackRunToken>,
    >,
    peer_inboxes: std::sync::Mutex<HashMap<lingxi_core::types::AgentId, handback::PeerInbox>>,
    /// The live root owns admission. Its reverse link to this registry must
    /// not retain the root or its durable writer after session shutdown.
    reporting_admission:
        std::sync::OnceLock<std::sync::Weak<dyn lingxi_core::host::handback::ReportingAdmission>>,
    shell_adoption_gate: Arc<tokio::sync::Mutex<()>>,
    shell_transfer_mutation: tokio::sync::Mutex<()>,
    /// Allocated shells may exit before their background row is published.
    pending_bash_registration:
        std::sync::Mutex<HashMap<String, Option<(Option<i32>, bool, Option<String>)>>>,
    shell_transfer_fences: Arc<std::sync::Mutex<HashSet<String>>>,
    shell_adoption_runtime: Option<(
        Arc<dyn ProcessRunner>,
        Arc<dyn crate::handlers::TaskStatusSink>,
    )>,
    observer_activity_lock: tokio::sync::Mutex<()>,
    tasks: Arc<crate::lifecycle_store::TaskRows>,
    /// Active direct-Agent wait claims. Generations prevent a dropped older
    /// waiter from resetting a newer waiter's notification claim.
    agent_wait_claims: std::sync::Mutex<HashMap<String, AgentWaitClaimRecord>>,
    next_agent_wait_claim_generation: AtomicU64,
    shell_session_activity: std::sync::Mutex<ShellSessionActivity>,
    workflow_launch_reservations: Arc<std::sync::Mutex<HashSet<String>>>,
    workflow_session_filter: Arc<std::sync::RwLock<Option<String>>>,
    /// Alternate addresses accepted by task tools (`agent_id`, named async
    /// agents, `name@team`) mapped onto the canonical task id. Real task ids
    /// still win on lookup; aliases only bridge claude-code's Agent return
    /// surface to the task registry's `a…`/`t…` ids.
    aliases: Arc<RwLock<HashMap<String, String>>>,
    /// Per-type owners are interior-mutable only so shutdown can sever the
    /// registry -> handler side of the composition graph after every worker
    /// has stopped. Some handlers legitimately point back toward the registry
    /// through tool registries; retaining them forever would otherwise pin the
    /// old session's cost and persistence authority across a remount.
    handlers: RwLock<HashMap<TaskType, Arc<dyn Task>>>,
    agent_message_receivers: tokio::sync::Mutex<
        HashMap<String, Arc<dyn lingxi_core::host::task_registry::TaskMessageReceiver>>,
    >,
    handles: Arc<tokio::sync::Mutex<HashMap<String, BackgroundTaskHandle>>>,
    /// Handler-returned cleanup hooks keyed by task id. Handler-spawned tasks
    /// often own runtime work internally; preserving this hook lets the registry
    /// participate in the same teardown path instead of dropping the only
    /// synchronous cleanup handle.
    cleanups: Arc<tokio::sync::Mutex<HashMap<String, TaskCleanup>>>,
    /// Per-task handles that move a still-running FOREGROUND command to the
    /// background (claude-code `I_t`'s `t.background(e)`). Separate from
    /// `cleanups` because backgrounding is not teardown: the child keeps
    /// running, it just stops being awaited.
    backgrounders: Arc<
        tokio::sync::Mutex<
            HashMap<String, Arc<dyn lingxi_core::host::task_registry::TaskBackgrounder>>,
        >,
    >,
    /// Handler-spawned task ids → their [`TaskType`], so [`Self::kill`] can
    /// dispatch teardown to the owning handler ([`Task::kill`]). Distinct from
    /// `handles`, which tracks the [`create`](Self::create) /
    /// [`RuntimeSpawner`]-cancel path; handler-spawned workers (e.g. the
    /// persistent `InProcessTeammate`) manage their own runtime task internally
    /// and must be torn down through the handler, not by cancelling a
    /// `BackgroundTaskHandle` the registry never holds.
    spawned: Arc<RwLock<HashMap<String, TaskType>>>,
    /// Session-owned approved departure cleanup, weakly held to avoid cycles.
    teammate_departure_cleanup: RwLock<
        Option<std::sync::Weak<dyn lingxi_core::host::team_spawn::TeammateDepartureCleanup>>,
    >,
    /// External pane tasks must confirm process teardown before task tools
    /// publish a terminal status. Weak ownership avoids a registry/controller cycle.
    external_teammate_controller: RwLock<Option<std::sync::Weak<dyn TeamSpawnSeam>>>,
    external_teammate_tasks: RwLock<HashSet<String>>,
    in_process_stop_locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    runtime: Arc<dyn RuntimeSpawner>,
    fs: Arc<dyn FileSystem>,
    /// Owner of task spool files.
    pub output_manager: Arc<TaskOutputManager>,
    /// Best-effort seam to fire the `TaskCompleted` hook when a task reaches a
    /// terminal status. `None` (the default) => strict no-op; the orchestrator
    /// injects a real firer via [`with_task_completed_firer`](Self::with_task_completed_firer).
    /// Mirrors the `TeamSpawnSeam` decoupling: the `tasks` leaf cannot reach a
    /// live hook executor, so it calls through this narrow trait instead.
    task_completed_firer: std::sync::Mutex<hooks::OptionalTaskCompletedFirer>,
    /// Analytics sink for `tengu_agent_tool_terminated`; `None` ⇒ no emission.
    analytics_bus: Option<Arc<telemetry::AnalyticsBus>>,
    /// Host-owned writer for the actual coordinator member `isActive` field.
    /// Installed by the composition root; never inferred from task status.
    team_member_activity: std::sync::RwLock<
        Option<Arc<dyn lingxi_core::host::team_registry::TeamMemberActivityHandle>>,
    >,
    /// Best-effort seam to fire the `TaskCreated` hook when a task is created.
    /// Counterpart to [`task_completed_firer`](Self::task_completed_firer):
    /// `None` (the default) => strict no-op; the orchestrator injects a real
    /// firer via [`with_task_created_firer`](Self::with_task_created_firer).
    task_created_firer: std::sync::Mutex<hooks::OptionalTaskCreatedFirer>,
    /// Task ids of PERSISTENT agents that came to rest since the last drain —
    /// armed by [`mark_task_rested`](Self::mark_task_rested) (via the status
    /// sink's `notify_rest`), surfaced ONCE per rest by
    /// [`take_pending_task_notifications`](Self::take_pending_task_notifications)
    /// WITHOUT eviction (the still-alive agent re-arms on its next rest). Kept
    /// out of [`TaskStateBase`] to avoid a workspace-wide exhaustive-initializer
    /// churn for a field only this path reads.
    notification_revision: tokio::sync::watch::Sender<u64>,
    stopping_shells: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    pending_rest: Arc<RwLock<std::collections::HashMap<String, RestPayload>>>,
    /// Bounded live `monitor_ws` stdout events waiting for the next turn.
    pending_monitor_events: Arc<
        std::sync::Mutex<
            std::collections::VecDeque<lingxi_core::host::task_registry::TaskNotification>,
        >,
    >,
    /// Per-session running total of subagents spawned through the `Agent` tool
    /// (claude 2.1.212 `taskRegistry` `getTotalAgentSpawns` /
    /// `incrementTotalAgentSpawns`). The tool reads this before every spawn and
    /// rejects the launch once it reaches `CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION`
    /// (default 200), then bumps it on a cleared spawn. Interior-mutable so the
    /// shared `Arc<TaskRegistry>` the tool holds can count without a write lock.
    total_agent_spawns: AtomicU64,
    /// Session-wide WebSearch call counter — the `taskRegistry` `n` behind
    /// `getWebSearchCalls`/`incrementWebSearchCalls`/`resetWebSearchCalls` (parity
    /// 2.1.212). Session-global (shared across the main loop and its subagents,
    /// which route through the same registry), so the `WebSearch` tool enforces
    /// ONE budget per session. An `Arc<AtomicU32>` — cheap atomic reads/writes off
    /// the tool's hot path, no lock.
    web_search_calls: Arc<std::sync::atomic::AtomicU32>,
    /// Admission fence for host teardown. Spawn/register paths hold a shared
    /// guard through publication; shutdown takes the exclusive guard, closes
    /// admission, then snapshots every published owner without a race window.
    lifecycle_gate: Arc<RwLock<()>>,
    accepting_tasks: AtomicBool,
}

#[derive(Default)]
struct ShellSessionActivity {
    interactive: bool,
    busy: bool,
    last_interaction: Option<std::time::Instant>,
}

/// A stop's handler may publish Killed before the registry can stamp notified.
/// Keep that short window out of model drains; cancellation/error drops the
/// guard too, so no completion can remain suppressed after a failed stop.
struct ShellStopGuard {
    stopping: Arc<std::sync::Mutex<HashMap<String, usize>>>,
    task_id: String,
    revision: tokio::sync::watch::Sender<u64>,
}
impl Drop for ShellStopGuard {
    fn drop(&mut self) {
        let mut stopping = self
            .stopping
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = stopping.get_mut(&self.task_id) {
            *count -= 1;
            if *count == 0 {
                stopping.remove(&self.task_id);
            }
        }
        self.revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
}

/// The optional `<result>` / `<usage>` payload an agent carries when it comes to
/// rest (binary `enqueueAgentNotification` always passes both when a result
/// exists). Stashed at arm time so [`TaskRegistry::take_pending_task_notifications`]
/// can populate the notification — the live task itself stays `Running`.
#[derive(Clone, Default)]
struct RestPayload {
    /// The agent's final-text response → the `<result>` section.
    result: Option<String>,
    /// Run usage → the `<usage>` section.
    usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
    /// Persistent id of the resting agent. Used only for unnamed-owner
    /// fallback; never surfaced as a display name.
    agent_id: Option<lingxi_core::types::AgentId>,
    /// The resting agent's DISPLAY identity. A "came to rest" notification is
    /// deferred while this agent still owns live background children, and the
    /// owner match is by the same creator fields child task creation stamps.
    agent_name: Option<String>,
    team_name: Option<String>,
}

#[derive(Clone, Copy)]
struct AgentWaitClaimRecord {
    generation: u64,
    active_waiters: usize,
    agent_id: lingxi_core::types::AgentId,
}

/// A wait's notification claim is weakly tied to the registry. Dropping the
/// wait future schedules a generation-checked release without retaining the
/// session registry or its handlers.
pub(crate) struct AgentWaitNotificationClaim {
    registry: std::sync::Weak<TaskRegistry>,
    runtime: tokio::runtime::Handle,
    task_id: String,
    generation: u64,
    armed: bool,
}

impl AgentWaitNotificationClaim {
    pub(crate) async fn release(mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .release_agent_wait_claim(&self.task_id, self.generation)
                .await;
        }
        // The registry helper has no suspension point after its claim/refcount
        // mutation commits. Keep Drop armed while it waits for the row lock so
        // cancellation schedules the same generation-checked release.
        self.armed = false;
    }

    pub(crate) async fn finish(mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry
                .finish_agent_wait_claim(&self.task_id, self.generation)
                .await;
        }
        // As in `release`, this assignment follows the commit with no await in
        // between; cancellation while waiting leaves Drop responsible for the
        // finalizing handoff.
        self.armed = false;
    }
}

impl Drop for AgentWaitNotificationClaim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let registry = self.registry.clone();
        let task_id = self.task_id.clone();
        let generation = self.generation;
        drop(self.runtime.spawn(async move {
            if let Some(registry) = registry.upgrade() {
                registry
                    .release_agent_wait_claim(&task_id, generation)
                    .await;
            }
        }));
    }
}

fn clear_agent_finalizing_fact(agent: &mut crate::state::LocalAgentTaskState) -> bool {
    if matches!(
        agent.agent_list_lifecycle.finalizing,
        lingxi_core::host::task_registry::FieldPresence::Value(true)
    ) {
        agent.agent_list_lifecycle.finalizing =
            lingxi_core::host::task_registry::FieldPresence::Value(false);
        true
    } else {
        false
    }
}

type TaskCleanup = Arc<dyn Fn() + Send + Sync>;

/// Cancellation guard for the registry publication → TaskCreated → activation
/// handoff in [`TaskRegistry::spawn`].
///
/// Once the handler has prepared a worker, any cancellation before activation
/// must stop that worker and remove every registry artifact already published.
/// The cleanup hook is synchronous; Tokio-owned maps are removed on the current
/// runtime because `Drop` itself cannot await their locks.
struct SpawnPublicationGuard {
    task_id: String,
    tasks: Arc<crate::lifecycle_store::TaskRows>,
    spawned: Arc<RwLock<HashMap<String, TaskType>>>,
    cleanups: Arc<tokio::sync::Mutex<HashMap<String, TaskCleanup>>>,
    aliases: Arc<RwLock<HashMap<String, String>>>,
    cleanup: Option<TaskCleanup>,
    armed: bool,
}

impl SpawnPublicationGuard {
    fn new(registry: &TaskRegistry, task_id: String, cleanup: Option<TaskCleanup>) -> Self {
        Self {
            task_id,
            tasks: registry.tasks.clone(),
            spawned: registry.spawned.clone(),
            cleanups: registry.cleanups.clone(),
            aliases: registry.aliases.clone(),
            cleanup,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
        self.cleanup = None;
    }
}

impl Drop for SpawnPublicationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }

        // Stop handler-owned work immediately. The hook is idempotent and may
        // itself schedule async pool teardown on the current runtime.
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }

        let task_id = self.task_id.clone();
        let tasks = self.tasks.clone();
        let spawned = self.spawned.clone();
        let cleanups = self.cleanups.clone();
        let aliases = self.aliases.clone();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::error!(
                task_id,
                "task spawn cancelled without a Tokio runtime; registry rollback could not be scheduled"
            );
            return;
        };
        drop(runtime.spawn(async move {
            // Match publication lock order. Holding the task write lock while
            // removing secondary indexes prevents readers from observing a row
            // whose route/cleanup is only partially rolled back.
            let mut task_rows = tasks.write().await;
            let mut routes = spawned.write().await;
            let mut cleanup_rows = cleanups.lock().await;
            let mut alias_rows = aliases.write().await;
            task_rows.remove(&task_id);
            routes.remove(&task_id);
            cleanup_rows.remove(&task_id);
            alias_rows.retain(|_, canonical| canonical != &task_id);
        }));
    }
}

/// Durable handoff fields used to rebuild a checkpointed workflow after the
/// host process restarts. The workflow is registered as `Paused`; no worker is
/// spawned until the user explicitly invokes `Workflow` with its run id.
///
/// Deliberately carries no `scope` field. Every field here is read straight
/// from the on-disk checkpoint (`adopt.json`), which records whatever the
/// ORIGINAL caller supplied -- so `workflow_id` and `args` are exactly as
/// untrustworthy for minting Local App authority as they are everywhere else
/// in this crate (see [`crate::scope::LocalAppWorkflowTaskScope`]'s module
/// docs). Putting a `scope` field on this same struct would invite exactly
/// the mistake this type exists to avoid: deriving authority from the
/// checkpoint's own `workflow_id`/`args` instead of from state the Host
/// re-resolves on load. [`TaskRegistry::register_adopted_workflow_with_scope`]
/// takes the re-derived scope as a separate argument instead.
#[derive(Debug, Clone)]
pub struct AdoptedWorkflow {
    /// Original workflow task id.
    pub task_id: String,
    /// Session that owned this workflow when it was checkpointed.
    pub session_uuid: Option<String>,
    /// Display identity derived from the workflow metadata.
    pub workflow_id: String,
    /// Stable `wf_…` journal run id.
    pub run_id: String,
    /// Persisted deterministic script to rerun.
    pub script_path: String,
    /// Serialized workflow args, when present.
    pub args: Option<String>,
    /// Directory containing the run's `journal.jsonl`.
    pub transcript_dir: String,
    /// Human-readable workflow description.
    pub description: String,
    /// Original run start time.
    pub start_time: SystemTime,
}

/// Cancellation-safe ownership of one workflow run id during launch/adoption.
/// Dropping the token releases the id even if the surrounding async future is
/// aborted before it reaches its normal return path.
#[must_use = "dropping the reservation immediately reopens the workflow run id"]
#[derive(Debug)]
pub struct WorkflowRunReservation {
    reservations: Arc<std::sync::Mutex<HashSet<String>>>,
    run_id: Option<String>,
}

impl Drop for WorkflowRunReservation {
    fn drop(&mut self) {
        let Some(run_id) = self.run_id.take() else {
            return;
        };
        self.reservations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&run_id);
    }
}

impl TaskRegistry {
    /// Install the session's observer pairing table. Idempotent; a second call
    /// is ignored, like the other `OnceLock` seams here.
    pub fn set_observer_pairings(
        &self,
        table: Arc<lingxi_core::host::observer_pairing::ObserverPairings>,
    ) {
        let _ = self.observer_pairings.set(table);
    }

    /// The session's observer pairing table, when the host wired one.
    #[must_use]
    pub fn observer_pairings(
        &self,
    ) -> Option<&Arc<lingxi_core::host::observer_pairing::ObserverPairings>> {
        self.observer_pairings.get()
    }

    /// Install the weak self-reference used by cancellation-safe transactions.
    /// This is deliberately set-once, matching the deferred status sink.
    pub(crate) fn bind_self(&self, registry: &Arc<TaskRegistry>) {
        let _ = self.self_ref.set(Arc::downgrade(registry));
    }

    fn owned_self(&self) -> Option<Arc<TaskRegistry>> {
        self.self_ref.get().and_then(std::sync::Weak::upgrade)
    }

    pub(crate) fn record_permission_pause(
        &self,
        agent_id: lingxi_core::types::AgentId,
        milliseconds: u64,
    ) {
        if milliseconds == 0 {
            return;
        }
        let tasks = self.tasks.clone();
        // The prompt guard may run during future cancellation, so it cannot
        // await here. Retain the store until its serialized SDK write lands.
        tokio::spawn(async move {
            let mut rows = tasks.write().await;
            if let Some(state) = rows.values_mut().find(|state| {
                matches!(state,
                TaskState::InProcessTeammate(agent) if agent.agent_id == agent_id)
            }) {
                let base = state.base_mut();
                base.total_paused_ms = base.total_paused_ms.saturating_add(milliseconds);
            }
        });
    }

    /// Look up one handler through the interior-mutable table.
    pub(crate) async fn handler_for(&self, task_type: TaskType) -> Option<Arc<dyn Task>> {
        self.handlers.read().await.get(&task_type).cloned()
    }

    /// Construct an empty registry with no handlers yet registered.
    #[must_use]
    pub fn new(
        runtime: Arc<dyn RuntimeSpawner>,
        fs: Arc<dyn FileSystem>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            self_ref: std::sync::OnceLock::new(),
            observer_pairings: std::sync::OnceLock::new(),
            shell_adoption_gate: Arc::new(tokio::sync::Mutex::new(())),
            shell_transfer_mutation: tokio::sync::Mutex::new(()),
            shell_transfer_fences: Arc::new(std::sync::Mutex::new(HashSet::new())),
            human_messages: std::sync::Mutex::new(HashMap::new()),
            handback_transactions: tokio::sync::Mutex::new(()),
            handback_runs: std::sync::Mutex::new(HashMap::new()),
            peer_inboxes: std::sync::Mutex::new(HashMap::new()),
            reporting_admission: std::sync::OnceLock::new(),
            shell_adoption_runtime: None,
            observer_activity_lock: tokio::sync::Mutex::new(()),
            pending_bash_registration: std::sync::Mutex::new(HashMap::new()),
            tasks: Arc::new(crate::lifecycle_store::TaskRows::default()),
            agent_wait_claims: std::sync::Mutex::new(HashMap::new()),
            next_agent_wait_claim_generation: AtomicU64::new(1),
            shell_session_activity: std::sync::Mutex::new(ShellSessionActivity::default()),
            workflow_launch_reservations: Arc::new(std::sync::Mutex::new(HashSet::new())),
            workflow_session_filter: Arc::new(std::sync::RwLock::new(None)),
            aliases: Arc::new(RwLock::new(HashMap::new())),
            handlers: RwLock::new(HashMap::new()),
            agent_message_receivers: tokio::sync::Mutex::new(HashMap::new()),
            handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            cleanups: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            backgrounders: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            spawned: Arc::new(RwLock::new(HashMap::new())),
            external_teammate_controller: RwLock::new(None),
            teammate_departure_cleanup: RwLock::new(None),
            external_teammate_tasks: RwLock::new(HashSet::new()),
            in_process_stop_locks: tokio::sync::Mutex::new(HashMap::new()),
            runtime,
            fs,
            output_manager,
            task_completed_firer: std::sync::Mutex::new(None),
            analytics_bus: None,
            team_member_activity: std::sync::RwLock::new(None),
            task_created_firer: std::sync::Mutex::new(None),
            notification_revision: tokio::sync::watch::channel(0).0,
            stopping_shells: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_rest: Arc::new(RwLock::new(std::collections::HashMap::new())),
            pending_monitor_events: Arc::new(std::sync::Mutex::new(
                std::collections::VecDeque::new(),
            )),
            total_agent_spawns: AtomicU64::new(0),
            web_search_calls: Arc::new(std::sync::atomic::AtomicU32::new(0)),
            lifecycle_gate: Arc::new(RwLock::new(())),
            accepting_tasks: AtomicBool::new(true),
        }
    }

    /// Install the current session's coordinator team-file activity writer.
    /// The registry does not retain a coordinator or team-file implementation.
    pub fn set_team_member_activity_handle(
        &self,
        handle: Arc<dyn lingxi_core::host::team_registry::TeamMemberActivityHandle>,
    ) {
        *self
            .team_member_activity
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    }

    /// Publish an explicit activity fact for one exact in-process teammate.
    /// The identity and member address come from its registered task row, not
    /// caller-provided strings or a task status heuristic. Standalone rows have
    /// no coordinator member and therefore do not publish a team-file write.
    pub async fn set_team_member_active(
        &self,
        agent_id: lingxi_core::types::AgentId,
        active: bool,
    ) -> Result<(), TaskError> {
        let identity = {
            let tasks = self.tasks.read().await;
            tasks.values().find_map(|state| match state {
                TaskState::InProcessTeammate(teammate) if teammate.agent_id == agent_id => {
                    let base = &teammate.base;
                    Some(
                        match (
                            base.creator_team_name
                                .as_deref()
                                .filter(|team| !team.is_empty()),
                            base.creator_teammate_name
                                .as_deref()
                                .filter(|name| !name.is_empty()),
                        ) {
                            (Some(team), Some(member)) => {
                                Some((team.to_owned(), member.to_owned()))
                            }
                            _ => None,
                        },
                    )
                }
                _ => None,
            })
        };
        let Some(identity) = identity else {
            return Err(TaskError::NotFound(agent_id.as_uuid().to_string()));
        };
        let Some((team_name, member_name)) = identity else {
            // A matching standalone teammate has no coordinator member to
            // update, and the source does not create an implicit team row.
            return Ok(());
        };
        let handle = self
            .team_member_activity
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .ok_or_else(|| {
                TaskError::Internal("team member activity writer is not installed".into())
            })?;
        handle
            .set_active(&team_name, &member_name, active)
            .await
            .map_err(TaskError::Internal)
    }

    /// Bind the session's departure cleanup owner without retaining the session.
    pub async fn set_teammate_departure_cleanup(
        &self,
        owner: std::sync::Weak<dyn lingxi_core::host::team_spawn::TeammateDepartureCleanup>,
    ) {
        *self.teammate_departure_cleanup.write().await = Some(owner);
    }

    /// Whether this task still has approved departure work available for retry.
    pub async fn has_pending_teammate_departure(&self, task_id: &str) -> bool {
        let task_id = self.canonical_or_raw(task_id).await;
        let owner = self
            .teammate_departure_cleanup
            .read()
            .await
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        match owner {
            Some(owner) => owner.has_pending_departure(&task_id).await,
            None => false,
        }
    }

    /// Bind the host's external teammate teardown owner without retaining it.
    pub async fn set_external_teammate_controller(
        &self,
        controller: std::sync::Weak<dyn TeamSpawnSeam>,
    ) {
        *self.external_teammate_controller.write().await = Some(controller);
    }

    /// Mark a published pane task before making it visible as running.
    pub async fn register_external_teammate_task(&self, task_id: &str) {
        self.external_teammate_tasks
            .write()
            .await
            .insert(task_id.into());
    }

    /// Forget external routing only after the backend confirms termination.
    pub async fn unregister_external_teammate_task(&self, task_id: &str) {
        self.external_teammate_tasks.write().await.remove(task_id);
    }

    /// Session running total of `Agent`-tool subagent spawns (claude 2.1.212
    /// `getTotalAgentSpawns`). Read before every spawn to enforce the
    /// `CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION` cap.
    #[must_use]
    pub fn total_agent_spawns(&self) -> u64 {
        self.total_agent_spawns.load(Ordering::SeqCst)
    }

    /// Bump the session subagent-spawn counter by one (claude 2.1.212
    /// `incrementTotalAgentSpawns`), called once a spawn clears the cap gate.
    pub fn increment_total_agent_spawns(&self) {
        self.total_agent_spawns.fetch_add(1, Ordering::SeqCst);
    }

    /// Reserve one lifetime spawn atomically, rejecting once `cap` is reached.
    pub fn try_reserve_total_agent_spawn(&self, cap: u64) -> Result<u64, u64> {
        self.total_agent_spawns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < cap).then(|| current + 1)
            })
            .map(|previous| previous + 1)
    }

    /// Roll back a reservation for a launch rejected before pool allocation.
    pub fn release_total_agent_spawn_reservation(&self) {
        self.release_total_agent_spawn_reservations(1);
    }

    /// Reserve `n` lifetime spawn slots atomically.
    pub fn try_reserve_total_agent_spawns(&self, n: u64, cap: u64) -> Result<u64, u64> {
        if n == 0 {
            return Ok(self.total_agent_spawns());
        }
        self.total_agent_spawns
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                current.checked_add(n).filter(|&next| next <= cap)
            })
            .map(|previous| previous + n)
    }

    /// Release `n` previously reserved lifetime spawn slots.
    pub fn release_total_agent_spawn_reservations(&self, n: u64) {
        if n == 0 {
            return;
        }
        let _ =
            self.total_agent_spawns
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                    Some(current.saturating_sub(n))
                });
    }

    /// Current session-wide WebSearch call count — `getWebSearchCalls(){return n}`
    /// (parity 2.1.212).
    #[must_use]
    pub fn web_search_calls(&self) -> u32 {
        self.web_search_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Increment the session WebSearch counter — `incrementWebSearchCalls(){n++}`.
    pub fn increment_web_search_calls(&self) {
        self.web_search_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reset the session WebSearch counter — `resetWebSearchCalls(){n=0}`.
    pub fn reset_web_search_calls(&self) {
        self.web_search_calls
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    /// Inject the best-effort `TaskCompleted` hook firer. Default-`None`
    /// builder (the `RemoteTrigger`/seam pattern): existing `new()` callers and
    /// tests stay no-op; the composition root threads the orchestrator's firer
    /// here so a terminal status transition fires the `TaskCompleted` hook.
    #[must_use]
    pub fn with_task_completed_firer(mut self, firer: Arc<dyn hooks::TaskCompletedFirer>) -> Self {
        self.task_completed_firer = std::sync::Mutex::new(Some(firer));
        self
    }

    /// Wire the analytics bus so a killed background agent can report
    /// `tengu_agent_tool_terminated`.
    ///
    /// The event needs BOTH halves and neither side has both: the spawning tool
    /// knows the model / depth / built-in-ness, the registry knows `killed_by`.
    /// The tool stamps its half onto the task row at launch
    /// (`AgentTerminalOutcome::agent_depth` / `is_built_in`); this is where the
    /// two meet.
    #[must_use]
    pub fn with_analytics_bus(mut self, bus: Arc<telemetry::AnalyticsBus>) -> Self {
        self.analytics_bus = Some(bus);
        self
    }

    /// `tengu_agent_tool_terminated`, async twin (`src_163219561.js` @3558604).
    ///
    /// `reason` is derived from `killedBy` exactly as upstream does:
    /// `"parent"` → `parent_kill_async`, `"system"` → `system_kill_async`,
    /// anything else → `user_kill_async`.
    async fn emit_agent_tool_terminated(&self, state: &TaskState, killed_by: &str) {
        let (Some(bus), TaskState::LocalAgent(agent)) = (self.analytics_bus.as_ref(), state) else {
            return;
        };
        let mut md = telemetry::LogEventMetadata::new();
        md.insert(
            "agent_type".into(),
            telemetry::AnalyticsValue::String(agent.subagent_type.clone()),
        );
        md.insert(
            "model".into(),
            telemetry::AnalyticsValue::String(agent.outcome.model.clone().unwrap_or_default()),
        );
        md.insert(
            "duration_ms".into(),
            telemetry::AnalyticsValue::Int(
                agent
                    .base
                    .start_time
                    .elapsed()
                    .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
                    .unwrap_or(0),
            ),
        );
        md.insert("is_async".into(), telemetry::AnalyticsValue::Bool(true));
        if let Some(is_built_in) = agent.outcome.is_built_in {
            md.insert(
                "is_built_in_agent".into(),
                telemetry::AnalyticsValue::Bool(is_built_in),
            );
        }
        if let Some(depth) = agent.outcome.agent_depth {
            md.insert(
                "agent_depth".into(),
                telemetry::AnalyticsValue::Int(i64::from(depth)),
            );
        }
        let reason = match killed_by {
            "parent" => "parent_kill_async",
            "system" => "system_kill_async",
            _ => "user_kill_async",
        };
        md.insert(
            "reason".into(),
            telemetry::AnalyticsValue::String(reason.to_string()),
        );
        bus.log_event("tengu_agent_tool_terminated", md).await;
    }

    /// Inject the best-effort `TaskCreated` hook firer. Counterpart to
    /// [`with_task_completed_firer`](Self::with_task_completed_firer): existing
    /// `new()` callers and tests stay no-op; the composition root threads the
    /// orchestrator's firer here so creating a task fires the `TaskCreated`
    /// hook.
    #[must_use]
    pub fn with_task_created_firer(mut self, firer: Arc<dyn hooks::TaskCreatedFirer>) -> Self {
        self.task_created_firer = std::sync::Mutex::new(Some(firer));
        self
    }

    /// Register a per-type handler.
    pub fn register_handler(&mut self, task_type: TaskType, handler: Arc<dyn Task>) {
        self.handlers.get_mut().insert(task_type, handler);
    }

    /// Resolve `id_or_alias` to the canonical task id. A real task id wins over
    /// an alias collision; stale aliases are ignored and cleaned best-effort.
    pub async fn resolve_task_id(&self, id_or_alias: &str) -> Option<String> {
        if self.tasks.read().await.contains_key(id_or_alias) {
            return Some(id_or_alias.to_string());
        }
        let mapped = self.aliases.read().await.get(id_or_alias).cloned();
        if let Some(task_id) = mapped {
            if self.tasks.read().await.contains_key(&task_id) {
                return Some(task_id);
            }
            self.aliases.write().await.remove(id_or_alias);
        }
        None
    }

    async fn canonical_or_raw(&self, id_or_alias: &str) -> String {
        self.resolve_task_id(id_or_alias)
            .await
            .unwrap_or_else(|| id_or_alias.to_string())
    }

    /// Register an additional task address. Empty aliases and self aliases are
    /// ignored; later registrations intentionally win so a reused async-agent
    /// name points at the newest live task, matching claude-code's name map.
    pub async fn register_task_alias(
        &self,
        task_id: &str,
        alias: impl Into<String>,
    ) -> Result<(), TaskError> {
        if !self.tasks.read().await.contains_key(task_id) {
            return Err(TaskError::NotFound(task_id.to_string()));
        }
        let alias = alias.into();
        if alias.is_empty() || alias == task_id {
            return Ok(());
        }
        self.aliases
            .write()
            .await
            .insert(alias, task_id.to_string());
        Ok(())
    }

    /// Create a new task entry. Allocates the output file and returns the
    /// generated task ID. Spawning the actual worker is done by handlers.
    pub async fn create(
        &self,
        task_type: TaskType,
        input: TaskSpawnInput,
        description: String,
    ) -> Result<String, TaskError> {
        let _admission = self.lifecycle_gate.read().await;
        if !self.accepting_tasks.load(Ordering::Acquire) {
            return Err(TaskError::Internal("task registry is shutting down".into()));
        }
        let id = generate_task_id(task_type);
        let path = self
            .output_manager
            .allocate(&id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        // Keep a copy of the description for the `TaskCreated` fire below — the
        // original is moved into `base` here.
        let description_for_hook = description.clone();
        let base = TaskStateBase {
            id: id.clone(),
            task_type,
            status: TaskStatus::Pending,
            description,
            tool_use_id: None,
            start_time: SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: path,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        // Build a default state per type; production stores real fields.
        #[allow(clippy::match_same_arms)]
        let state = match task_type {
            TaskType::LocalBash => TaskState::LocalBash(crate::state::LocalBashTaskState {
                is_adopted: false,
                caller: None,
                base,
                command: String::new(),
                pid: None,
                exit_code: None,
                stop_cause: None,
                cwd: None,
                is_backgrounded: None,
            }),
            // A `local_agent` row must carry the agent-shaped state, not the
            // `_` fallback's `LocalBash`: `bind_agent_id` and every other
            // agent-only registry operation match on the variant, so a row
            // created through this path would otherwise report `local_agent`
            // on the wire (`state_to_record` reads `base.task_type`) while
            // refusing every agent operation with `Unsupported`.
            TaskType::LocalAgent => {
                let (agent_id, subagent_type, prompt, is_backgrounded) = match &input {
                    TaskSpawnInput::LocalAgent {
                        agent_id,
                        subagent_type,
                        prompt,
                        is_backgrounded,
                        ..
                    } => (
                        *agent_id,
                        subagent_type.clone(),
                        prompt.clone(),
                        *is_backgrounded,
                    ),
                    _ => (
                        lingxi_core::types::AgentId::nil(),
                        String::new(),
                        String::new(),
                        false,
                    ),
                };
                TaskState::LocalAgent(crate::state::LocalAgentTaskState {
                    handback: None,
                    handback_history: Vec::new(),

                    agent_spawn_provenance: Default::default(),
                    // Generic task-create rows are placeholders, not a Native
                    // Agent launch; keep their lifecycle facts missing until
                    // the actual spawn path binds a real Agent identity.
                    agent_list_lifecycle: Default::default(),
                    spawned_description: None,
                    is_parked: false,
                    is_observer: false,
                    observed_agent_id: None,
                    forked_skill_name: None,
                    base,
                    agent_id,
                    subagent_type,
                    prompt,
                    error: None,
                    messages: vec![],
                    pending_messages: vec![],
                    is_backgrounded,
                    outcome: Default::default(),
                })
            }
            TaskType::AutoModeScan => {
                TaskState::AutoModeScan(crate::state::AutoModeScanTaskState { base })
            }
            TaskType::Dream => TaskState::Dream(crate::state::DreamTaskState {
                base,
                iteration_count: 0,
                max_iterations: None,
            }),
            TaskType::Monitor => TaskState::Monitor(crate::state::MonitorTaskState {
                base,
                command: String::new(),
                exit_code: None,
                stdout_bytes: None,
            }),
            TaskType::InProcessTeammate => {
                let TaskSpawnInput::InProcessTeammate {
                    agent_id,
                    name,
                    team_name,
                    ..
                } = &input
                else {
                    return Err(TaskError::Internal(
                        "teammate row requires teammate identity".into(),
                    ));
                };
                let mut base = base;
                base.creator_teammate_name = Some(name.clone());
                base.creator_team_name = Some(team_name.clone());
                TaskState::InProcessTeammate(crate::state::InProcessTeammateTaskState {
                    child_model: None,
                    child_model_profile: None,
                    handback: None,
                    handback_history: Vec::new(),

                    agent_spawn_provenance: Default::default(),
                    spawned_agent_type: None,
                    spawned_description: None,
                    is_idle: false,
                    awaiting_plan_approval: false,
                    base,
                    agent_id: *agent_id,
                    pending_messages: vec![],
                })
            }
            _ => TaskState::LocalBash(crate::state::LocalBashTaskState {
                is_adopted: false,
                caller: None,
                base,
                command: String::new(),
                pid: None,
                exit_code: None,
                stop_cause: None,
                cwd: None,
                is_backgrounded: None,
            }),
        };
        self.tasks.write().await.insert(id.clone(), state);
        if task_type == TaskType::InProcessTeammate {
            for alias in aliases_for_spawn(&input) {
                self.aliases.write().await.insert(alias, id.clone());
            }
        }
        // Best-effort `TaskCreated` fire (claude-code `executeTaskCreatedHooks`,
        // fired from `TaskCreateTool`). No-op when no firer is registered.
        self.fire_task_created(&id, task_type, &description_for_hook)
            .await;
        Ok(id)
    }

    /// Best-effort `TaskCreated` hook fire for a newly-created task. Runs
    /// WITHOUT holding the registry lock (callers drop their write guard before
    /// invoking) so a slow/blocking hook never stalls other task operations.
    /// No-op when no firer is registered.
    ///
    /// Wire payload (`TaskCreatedHookInputSchema`): `task_subject` sources from
    /// the task's [`TaskType`] taxonomy bucket (the M-surface task state carries
    /// no distinct `subject` field), `task_description` from `description`.
    /// `teammate_name` / `team_name` are not stored on the task state, so they
    /// ride as `None` (the same documented gap as the `TaskCompleted` fire).
    async fn fire_task_created(&self, task_id: &str, task_type: TaskType, description: &str) {
        let firer = self
            .task_created_firer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(firer) = firer {
            let (teammate_name, team_name) = self
                .get(task_id)
                .await
                .map(|state| {
                    let base = state.base();
                    (
                        base.creator_teammate_name.clone(),
                        base.creator_team_name.clone(),
                    )
                })
                .unwrap_or((None, None));
            firer
                .fire(hooks::TaskCreatedFire {
                    task_id: task_id.to_string(),
                    task_subject: format!("{task_type:?}"),
                    task_description: Some(description.to_string()),
                    teammate_name,
                    team_name,
                })
                .await;
        }
    }

    /// Register /auto-mode-setup reconnaissance with a TaskStop cancellation hook.
    pub async fn register_auto_mode_scan(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, TaskError> {
        let id = generate_task_id(TaskType::AutoModeScan);
        let output_file = self
            .output_manager
            .allocate(&id)
            .await
            .map_err(|error| TaskError::Io(error.to_string()))?;
        let state = TaskState::AutoModeScan(crate::state::AutoModeScanTaskState {
            base: TaskStateBase {
                id: id.clone(),
                task_type: TaskType::AutoModeScan,
                status: TaskStatus::Running,
                description: "scanning for auto-mode setup".into(),
                tool_use_id: None,
                start_time: SystemTime::now(),
                end_time: None,
                total_paused_ms: 0,
                output_file,
                evict_after: None,
                output_offset: 0,
                // Oracle skipTranscript + ambient: this scan has no chat notification.
                notified: true,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
            },
        });
        let cleanup: TaskCleanup = Arc::new(move || cancel.cancel());
        // Install cancellation before publishing the row to /tasks.
        self.cleanups.lock().await.insert(id.clone(), cleanup);
        self.tasks.write().await.insert(id.clone(), state);
        Ok(id)
    }

    /// Register a backgrounded MCP tool call (claude-code 2.1.212
    /// `callMcpToolWithAutoBackground`'s `i.register(NZu(...))`). Inserts a
    /// `running` [`crate::state::McpTaskState`] with `mcpStatus:"working"`,
    /// keyed on a freshly-minted `k…` id, and stores a cancel cleanup so a later
    /// [`kill`](Self::kill) aborts the still-running in-flight call (the port
    /// equivalent of the state's `abortController` + the poll loop's
    /// `cancelTask` on `status==="killed"`). Returns the minted id.
    pub async fn register_mcp_task(
        &self,
        server_name: String,
        tool_name: String,
        tool_use_id: Option<String>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, TaskError> {
        self.register_mcp_task_owned(
            server_name,
            tool_name,
            tool_use_id,
            None,
            None,
            None,
            cancel,
        )
        .await
    }

    pub(crate) async fn register_mcp_task_owned(
        &self,
        server_name: String,
        tool_name: String,
        tool_use_id: Option<String>,
        creator_teammate_name: Option<String>,
        creator_team_name: Option<String>,
        creator_agent_id: Option<lingxi_core::types::AgentId>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<String, TaskError> {
        let _admission = self.lifecycle_gate.read().await;
        if !self.accepting_tasks.load(Ordering::Acquire) {
            return Err(TaskError::Internal("task registry is shutting down".into()));
        }
        let id = generate_task_id(TaskType::McpTask);
        let path = self
            .output_manager
            .allocate(&id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        // Description mirrors `NZu`'s `${serverName}/${toolName}` (also what
        // `background_message` interpolates as the tool label).
        let description = format!("{server_name}/{tool_name}");
        let description_for_hook = description.clone();
        let base = TaskStateBase {
            id: id.clone(),
            task_type: TaskType::McpTask,
            // `NZu` seeds `status:"running"` (distinct from the `create`
            // placeholder's `Pending`); the call is already in flight.
            status: TaskStatus::Running,
            description,
            tool_use_id,
            start_time: SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: path,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
        };
        let state = TaskState::McpTask(crate::state::McpTaskState {
            saved_hint: None,
            base,
            server_name,
            tool_name,
            // `NZu` seeds `mcpStatus:"working"`.
            mcp_status: "working".to_string(),
            status_message: None,
            // A running call has produced nothing yet.
            result_text: None,
        });
        self.tasks.write().await.insert(id.clone(), state);
        // Cancel-on-kill hook: `kill` runs this cleanup (no handler / no
        // `BackgroundTaskHandle` for an mcp_task), which fires the token the
        // caller races its in-flight call against.
        let cleanup: TaskCleanup = Arc::new(move || cancel.cancel());
        self.cleanups.lock().await.insert(id.clone(), cleanup);
        self.fire_task_created(&id, TaskType::McpTask, &description_for_hook)
            .await;
        Ok(id)
    }

    /// Settle a backgrounded MCP tool call once its detached `tools/call`
    /// resolves (claude-code `callMcpToolWithAutoBackground`'s `E` +
    /// `p.then(…)`). Writes `result_text` into the task spool so the drained
    /// `<task-notification>`'s `output-file` carries the real result, then marks
    /// the task terminal (`completed`/`failed`) and updates `mcpStatus`.
    ///
    /// A task already terminal (e.g. killed via `TaskStop`, or already settled)
    /// is left untouched — the binary's `if(O.notified) return O` guard — so a
    /// race between kill and settle never resurrects a killed task.
    ///
    /// Returns `Ok(true)` when THIS call won the terminal transition (the
    /// binary's `E` callback observing `k = true` after `i.update`), and
    /// `Ok(false)` when the task was already terminal so the update no-op'd
    /// (`if (O.notified) return O`). Callers use this to gate the
    /// `mcp_auto_background` outcome counter — a killed / already-settled task
    /// must never re-emit it.
    /// Register a shell command the caller is about to background, minting the
    /// id and creating the output file the process runner will append to.
    ///
    /// This is the port of claude-code `Xne` (2.1.263 `src_160988549.js`
    /// @4281167): the shell's own task identity becomes a `local_bash` record
    /// with `status: "running"`, so the `backgroundTaskId` handed to the model
    /// resolves in `TaskOutput` / `TaskStop` / `TaskList`, and the terminal
    /// transition produces the completion `<task-notification>`.
    ///
    /// The registry owns the id space and the output directory, so the caller
    /// must take both from here rather than minting its own — a second id space
    /// is exactly the defect this closes.
    ///
    /// # Errors
    /// Returns [`TaskError::Io`] when the output file cannot be allocated.
    pub async fn allocate_bash_output(&self) -> Result<(String, std::path::PathBuf), TaskError> {
        let id = generate_task_id(TaskType::LocalBash);
        let path = self
            .output_manager
            .allocate(&id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        self.pending_bash_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), None);
        Ok((id, path))
    }

    /// Delete an allocated shell output file that was never needed, because the
    /// command finished in the foreground and its output was returned inline
    /// (claude-code `deleteOutputFile` under `outputFileRedundant`).
    pub async fn discard_bash_output(&self, task_id: &str) {
        let Ok(path) = self.output_manager.path_for(task_id) else {
            return;
        };
        if self.tasks.read().await.contains_key(task_id) {
            // A record was registered against this identity after all; its file
            // is live output, not a redundant allocation.
            return;
        }
        self.pending_bash_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
        let _ = self.output_manager.discard(&path).await;
    }

    /// Register a `local_bash` record for an identity already minted by
    /// [`Self::allocate_bash_output`].
    ///
    /// # Errors
    /// Returns [`TaskError::Io`] when the output path cannot be derived.
    pub async fn register_background_bash(
        &self,
        id: String,
        command: String,
        description: String,
        tool_use_id: Option<String>,
        cwd: Option<String>,
        creator_agent_id: Option<lingxi_core::types::AgentId>,
    ) -> Result<(String, std::path::PathBuf), TaskError> {
        self.register_bash_row(
            id,
            command,
            description,
            tool_use_id,
            cwd,
            creator_agent_id,
            true,
        )
        .await
    }

    /// Shared row builder for the two `local_bash` registration entry points.
    /// `backgrounded` is claude-code's `isBackgrounded`: `true` for `Xne` (an
    /// explicit background spawn or a timed-out command), `false` for `U6t`
    /// (the 2 s foreground arming).
    #[allow(clippy::too_many_arguments)]
    async fn register_bash_row(
        &self,
        id: String,
        command: String,
        description: String,
        tool_use_id: Option<String>,
        cwd: Option<String>,
        creator_agent_id: Option<lingxi_core::types::AgentId>,
        backgrounded: bool,
    ) -> Result<(String, std::path::PathBuf), TaskError> {
        let path = self
            .output_manager
            .path_for(&id)
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let description_for_hook = description.clone();
        let base = TaskStateBase {
            id: id.clone(),
            task_type: TaskType::LocalBash,
            status: TaskStatus::Running,
            description,
            tool_use_id,
            start_time: SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: path.clone(),
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id,
        };
        let caller = (!backgrounded).then(|| {
            if base.creator_agent_id.is_some() {
                "agent"
            } else {
                "turn"
            }
            .to_string()
        });
        let state = TaskState::LocalBash(crate::state::LocalBashTaskState {
            is_adopted: false,
            caller,
            base,
            command,
            pid: None,
            exit_code: None,
            stop_cause: None,
            cwd,
            // This entry point only fires for a shell that is actually being
            // backgrounded (claude-code `Xne` registers `isBackgrounded: true`).
            // The 2 s foreground arming registers `false` through
            // [`Self::register_foreground_bash`].
            is_backgrounded: Some(backgrounded),
        });
        let (created, early_terminal, supervised) = {
            let mut rows = self.tasks.write().await;
            // Acquire every awaited lock before consuming the receipt or
            // mutating state: cancellation cannot strand a half-publication.
            let mut cleanups = self.cleanups.lock().await;
            // The producer marks supervision before acquiring this same task
            // lock to cache its receipt. Read the flag before consuming it.
            let supervised = self
                .output_manager
                .shell_terminal_is_supervised(&path)
                .await;
            let created = match rows.get_mut(&id) {
                Some(TaskState::LocalBash(existing)) => {
                    let TaskState::LocalBash(incoming) = &state else {
                        unreachable!()
                    };
                    if existing.command != incoming.command
                        || existing.base.creator_agent_id != incoming.base.creator_agent_id
                        || matches!((&existing.base.tool_use_id, &incoming.base.tool_use_id), (Some(old), Some(new)) if old != new)
                    {
                        return Err(TaskError::Internal("conflicting shell registration".into()));
                    }
                    // U6t arming and later Xne backgrounding name one process.
                    // Preserve its terminal result, PID, caller and clocks.
                    existing.is_backgrounded =
                        Some(existing.is_backgrounded == Some(true) || backgrounded);
                    if existing.base.tool_use_id.is_none() {
                        existing.base.tool_use_id = incoming.base.tool_use_id.clone();
                    }
                    if incoming.cwd.is_some() {
                        existing.cwd = incoming.cwd.clone();
                    }
                    false
                }
                Some(_) => {
                    return Err(TaskError::Internal(
                        "shell identity belongs to another task type".into(),
                    ));
                }
                None => {
                    rows.insert(id.clone(), state);
                    true
                }
            };
            let early_exit = self
                .pending_bash_registration
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
                .flatten();
            if created && early_exit.is_some() {
                rows.checkpoint();
            }
            let early_terminal = early_exit.and_then(|(code, killed, stop_cause)| {
                let entry = rows.get_mut(&id).expect("just registered shell");
                if entry.base().status.is_terminal() {
                    return None;
                }
                let TaskState::LocalBash(shell) = entry else {
                    unreachable!()
                };
                if let Some(code) = code {
                    shell.exit_code = Some(code);
                }
                shell.stop_cause = stop_cause;
                shell.base.status = if killed {
                    TaskStatus::Killed
                } else if code == Some(0) {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
                stamp_terminal_clock(entry, false);
                cleanups.remove(&id);
                Some((entry.clone(), code, killed))
            });
            (created, early_terminal, supervised)
        };
        if let Some((terminal, code, killed)) = early_terminal {
            self.bump_notification_revision();
            let created_firer = created
                .then(|| {
                    self.task_created_firer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone()
                })
                .flatten();
            let completed_firer = self
                .task_completed_firer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let output = self.output_manager.clone();
            let output_path = path.clone();
            let task_id = id.clone();
            let base = terminal.base().clone();
            // Scheduling is synchronous. Dropping this registration future at
            // a hook/marker await detaches the lifecycle job, not its receipt.
            let job = tokio::spawn(async move {
                let marker = async {
                    if !supervised {
                        let trailer = if killed {
                            "\n[killed]\n".to_string()
                        } else {
                            format!(
                                "\n[exited with code {}]\n",
                                code.map_or_else(|| "unknown".into(), |code| code.to_string())
                            )
                        };
                        output.append_shell_terminal(&output_path, &trailer).await;
                    }
                    output.evict_writer(&output_path).await;
                };
                let created = async {
                    if let Some(firer) = created_firer {
                        firer
                            .fire(hooks::TaskCreatedFire {
                                task_id: task_id.clone(),
                                task_subject: "LocalBash".into(),
                                task_description: Some(description_for_hook),
                                teammate_name: base.creator_teammate_name.clone(),
                                team_name: base.creator_team_name.clone(),
                            })
                            .await;
                    }
                };
                tokio::join!(marker, created);
                if let Some(firer) = completed_firer {
                    let status = match base.status {
                        TaskStatus::Completed => Some("completed"),
                        TaskStatus::Failed => Some("failed"),
                        _ => None,
                    };
                    if let Some(status) = status {
                        firer
                            .fire(hooks::TaskCompletedFire {
                                task_id,
                                status: status.into(),
                                task_subject: base.description.clone(),
                                task_description: Some(base.description),
                                teammate_name: base.creator_teammate_name,
                                team_name: base.creator_team_name,
                            })
                            .await;
                    }
                }
            });
            job.await.map_err(|error| {
                TaskError::Internal(format!("shell registration lifecycle task: {error}"))
            })?;
        } else if created {
            self.fire_task_created(&id, TaskType::LocalBash, &description_for_hook)
                .await;
        }
        Ok((id, path))
    }

    /// Install stop control for a registered task without requiring an OS pid.
    /// Foreground agents use this before their startup gate opens.
    ///
    /// # Errors
    /// Returns [`TaskError::NotFound`] when the id is unknown.
    pub async fn bind_background_killer(
        &self,
        task_id: &str,
        killer: Arc<dyn lingxi_core::host::task_registry::TaskKiller>,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let cleanup: TaskCleanup = Arc::new(move || {
            let killer = killer.clone();
            tokio::spawn(async move { killer.kill().await });
        });
        // Keep terminal publication and binding atomic, in tasks -> cleanups
        // lock order, so a late binding cannot revive a finished task's control.
        let map = self.tasks.read().await;
        let entry = map
            .get(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        if entry.base().status.is_terminal() {
            return Ok(());
        }
        self.cleanups.lock().await.insert(task_id, cleanup);
        Ok(())
    }

    /// Record the OS pid of an already-registered background shell and install
    /// the cleanup that kills it, so `TaskStop` reaches a child this registry
    /// did not spawn itself.
    ///
    /// # Errors
    /// Returns [`TaskError::NotFound`] when the id is unknown.
    pub async fn bind_background_bash_process(
        &self,
        task_id: &str,
        pid: Option<u32>,
        killer: Arc<dyn lingxi_core::host::task_registry::TaskKiller>,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let cleanup: TaskCleanup = Arc::new(move || {
            let killer = killer.clone();
            tokio::spawn(async move { killer.kill().await });
        });
        // Match terminal publication's tasks -> cleanups lock order. A child
        // completing during binding must never leave a stale PID killer behind.
        let mut map = self.tasks.write().await;
        let entry = map
            .get_mut(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        let TaskState::LocalBash(bash) = entry else {
            return Err(TaskError::Unsupported);
        };
        if bash.base.status.is_terminal() {
            return Ok(());
        }
        let mut cleanups = self.cleanups.lock().await;
        bash.pid = pid;
        cleanups.insert(task_id, cleanup);
        Ok(())
    }

    /// Register a still-running FOREGROUND shell (claude-code `U6t`, fired by
    /// the Bash poll loop once the command has run for `cnr` = 2000 ms).
    ///
    /// The row is `running` with `is_backgrounded = Some(false)`: visible to
    /// `/tasks` and addressable by Ctrl+B / background-all, but not a background
    /// task. [`Self::unregister_foreground_bash`] withdraws it again when the
    /// command finishes in the foreground.
    ///
    /// # Errors
    /// Returns [`TaskError`] if the record cannot be published.
    pub async fn register_foreground_bash(
        &self,
        task_id: &str,
        registration: lingxi_core::host::task_registry::BackgroundBashRegistration,
        auto_background_armed: bool,
    ) -> Result<(), TaskError> {
        // `autoBackgroundArmed` rides on the record in claude-code purely so the
        // UI can say whether the deadline will background or kill. Nothing in
        // this engine reads it yet, so it is not stored — recording a field no
        // consumer reads would be the "named, computed, never wired" shape.
        let _ = auto_background_armed;
        self.register_bash_row(
            task_id.to_string(),
            registration.command,
            registration.description,
            registration.tool_use_id,
            registration.cwd,
            registration.creator_agent_id,
            false,
        )
        .await
        .map(|_| ())
    }

    pub async fn bind_agent_message_receiver(
        &self,
        task_id: &str,
        receiver: Arc<dyn lingxi_core::host::task_registry::TaskMessageReceiver>,
    ) -> Result<(), TaskError> {
        let id = self.canonical_or_raw(task_id).await;
        if !self.tasks.read().await.contains_key(&id) {
            return Err(TaskError::NotFound(id));
        }
        self.agent_message_receivers
            .lock()
            .await
            .insert(id, receiver);
        Ok(())
    }

    /// `fln`: expose an allocated foreground agent to task controls.
    pub async fn register_foreground_agent(
        &self,
        registration: lingxi_core::host::task_registry::ForegroundAgentRegistration,
    ) -> Result<TaskState, TaskError> {
        let id = generate_task_id(TaskType::LocalAgent);
        let path = self
            .output_manager
            .allocate(&id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        let base = TaskStateBase {
            id: id.clone(),
            task_type: TaskType::LocalAgent,
            status: TaskStatus::Running,
            description: registration.description.clone(),
            tool_use_id: registration.tool_use_id,
            start_time: SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: path,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: registration.creator_teammate_name,
            creator_team_name: registration.creator_team_name,
            creator_agent_id: registration.creator_agent_id,
        };
        let state = TaskState::LocalAgent(crate::state::LocalAgentTaskState {
            handback: None,
            handback_history: Vec::new(),
            agent_spawn_provenance: registration.agent_spawn_provenance.clone(),
            agent_list_lifecycle: lingxi_core::host::task_registry::AgentListLocalLifecycleFacts::initialized_local_agent(),
            spawned_description: Some(registration.description.clone()),
            base,
            agent_id: registration.agent_id,
            subagent_type: registration.agent_type,
            prompt: registration.prompt,
            error: None,
            messages: Vec::new(),
            pending_messages: Vec::new(),
            is_backgrounded: false,
            is_parked: false,
            is_observer: false,
            observed_agent_id: None,
            forked_skill_name: None,
            outcome: Default::default(),
        });
        self.tasks.write().await.insert(id.clone(), state.clone());
        {
            let mut aliases = self.aliases.write().await;
            aliases.insert(registration.agent_id.to_string(), id.clone());
            aliases.insert(registration.agent_id.as_uuid().to_string(), id.clone());
        }
        self.fire_task_created(&id, TaskType::LocalAgent, &state.base().description)
            .await;
        self.bump_notification_revision();
        Ok(state)
    }

    /// Preserve the actual one-shot pool runner when a foreground call becomes
    /// background work, or its own background tasks have not been consumed.
    pub async fn park_foreground_agent(
        &self,
        agent_id: lingxi_core::types::AgentId,
        outcome: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) -> bool {
        let Some((id, backgrounded)) = ({
            let map = self.tasks.read().await;
            map.values().find_map(|state| match state {
                TaskState::LocalAgent(agent)
                    if agent.agent_id == agent_id
                        && !state.is_terminated()
                        && (agent.is_backgrounded
                            || self.has_live_background_children_locked(
                                &map,
                                Some(agent_id),
                                None,
                                None,
                            )) =>
                {
                    Some((agent.base.id.clone(), agent.is_backgrounded))
                }
                _ => None,
            })
        }) else {
            return false;
        };
        if !backgrounded {
            // Controls are installed by the foreground startup gate. If a
            // standalone host omits them, retaining the caller is still safer
            // than deallocating an owner with live children.
            self.background_task(&id).await;
        }
        let result = outcome.result.clone();
        let usage = outcome.usage.clone();
        let run = outcome.handback.as_ref().map(|state| state.run);
        self.set_agent_outcome(&id, outcome).await;
        self.mark_task_rested_with_finalizing(
            &id,
            result,
            usage,
            Some(agent_id),
            None,
            None,
            run,
            true,
        )
        .await;
        self.get(&id).await.is_some_and(|state| state.is_parked())
    }

    /// `mln`: a foreground completion is consumed directly by the caller.
    pub async fn unregister_foreground_agent(&self, task_id: &str) {
        let _report_transaction = self.handback_transactions.lock().await;
        let id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if matches!(map.get(&id), Some(TaskState::LocalAgent(agent)) if agent.outcome.killed_by.as_deref() == Some("user"))
        {
            // A user-addressable stopped task keeps its original ID/recipe.
            drop(map);
            self.agent_message_receivers.lock().await.remove(&id);
            self.backgrounders.lock().await.remove(&id);
            return;
        }
        if !matches!(map.get(&id), Some(TaskState::LocalAgent(agent)) if !agent.is_backgrounded) {
            return;
        }
        let output = map
            .remove(&id)
            .map(|state| state.base().output_file.clone());
        drop(map);
        if let Some(output) = output {
            if let Some(handler) = self.handler_for(TaskType::LocalAgent).await {
                handler.forget_resume_recipe(&id);
            }
            self.output_manager.release_output_state(&output).await;
        }
        self.backgrounders.lock().await.remove(&id);
        self.agent_message_receivers.lock().await.remove(&id);
        self.cleanups.lock().await.remove(&id);
        self.aliases.write().await.retain(|_, target| target != &id);
        self.bump_notification_revision();
    }

    /// Withdraw an armed foreground row because the command finished in the
    /// foreground.
    ///
    /// claude-code `W6t`: `if(!bp(o)||o.isBackgrounded||o.notified) return;
    /// r.remove(e)`. A row that was backgrounded in the meantime is left alone —
    /// it owns a real child now and will produce its own completion
    /// notification.
    pub async fn unregister_foreground_bash(&self, task_id: &str) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        self.pending_bash_registration
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&task_id);
        let Some(TaskState::LocalBash(bash)) = map.get(&task_id) else {
            return;
        };
        if bash.is_backgrounded == Some(true) || bash.base.notified {
            return;
        }
        let output = map
            .remove(&task_id)
            .map(|state| state.base().output_file.clone());
        drop(map);
        if let Some(output) = output {
            self.output_manager.release_output_state(&output).await;
        }
    }

    /// Attach the handle that moves an armed foreground shell to the background
    /// on demand (claude-code holds the live `shellCommand` on the record and
    /// calls `t.background(e)`).
    ///
    /// # Errors
    /// Returns [`TaskError::NotFound`] when the id is unknown.
    pub async fn bind_background_requester(
        &self,
        task_id: &str,
        requester: Arc<dyn lingxi_core::host::task_registry::TaskBackgrounder>,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        if !self.tasks.read().await.contains_key(&task_id) {
            return Err(TaskError::NotFound(task_id));
        }
        self.backgrounders.lock().await.insert(task_id, requester);
        Ok(())
    }

    /// claude-code `Upt` — is this task backgroundable right now?
    ///
    /// For a shell: not already backgrounded, and there is a live command behind
    /// it (`Boolean(e.shellCommand)`; here, a bound requester). A terminal row
    /// has nothing to background.
    fn task_is_backgroundable(state: &TaskState) -> bool {
        match state {
            TaskState::LocalBash(bash) => {
                bash.is_backgrounded != Some(true) && !bash.base.status.is_terminal()
            }
            TaskState::LocalAgent(agent) => !agent.is_backgrounded && !state.is_terminated(),
            _ => false,
        }
    }

    /// claude-code `H_t` — whether anything can be backgrounded, i.e. whether
    /// the Ctrl+B affordance should be offered at all.
    pub async fn has_backgroundable_tasks(&self) -> bool {
        if lingxi_core::host::env::background_tasks_disabled() {
            return false;
        }
        let map = self.tasks.read().await;
        let requesters = self.backgrounders.lock().await;
        map.iter()
            .any(|(id, state)| Self::task_is_backgroundable(state) && requesters.contains_key(id))
    }

    /// claude-code `Wer` → `I_t` — move one still-running foreground shell to
    /// the background. Returns whether it moved.
    ///
    /// Order matters and is the oracle's: ask the command to detach FIRST
    /// (`if(!t.background(e)) return!1`), and only then flip `isBackgrounded`.
    /// Flipping first would leave a row claiming to be a background task if the
    /// request could not be delivered.
    pub async fn background_task(&self, task_id: &str) -> bool {
        self.background_task_with_reason(
            task_id,
            lingxi_core::host::task_registry::TaskBackgroundReason::User,
        )
        .await
    }

    async fn background_task_with_reason(
        &self,
        task_id: &str,
        reason: lingxi_core::host::task_registry::TaskBackgroundReason,
    ) -> bool {
        if lingxi_core::host::env::background_tasks_disabled() {
            return false;
        }
        let task_id = self.canonical_or_raw(task_id).await;
        {
            let map = self.tasks.read().await;
            match map.get(&task_id) {
                Some(state) if Self::task_is_backgroundable(state) => {}
                _ => return false,
            }
        }
        let Some(requester) = self.backgrounders.lock().await.get(&task_id).cloned() else {
            return false;
        };
        {
            let mut map = self.tasks.write().await;
            match map.get_mut(&task_id) {
                Some(TaskState::LocalBash(_)) => {}
                // `s9` publishes the agent's background state before waking
                // its waiting tool call; that call may finish immediately.
                Some(TaskState::LocalAgent(agent)) => agent.is_backgrounded = true,
                _ => return false,
            }
        }
        requester.background_with_reason(reason).await;
        if let Some(TaskState::LocalBash(bash)) = self.tasks.write().await.get_mut(&task_id) {
            bash.is_backgrounded = Some(true);
        }
        self.bump_notification_revision();
        true
    }

    /// claude-code `bjn` + `JFe` — the two rosters a "no task found" message
    /// names.
    ///
    /// `bjn` lists a teammate by `identity.agentId`, which upstream is the
    /// `name@team` string. Here that form lives in the alias map (built at
    /// spawn by `aliases_for_spawn`), so the roster reverse-looks-it-up and
    /// falls back to the raw agent id when a teammate has no team.
    pub async fn not_found_rosters(
        &self,
        caller_agent_id: Option<&str>,
        named_agent_ids: &[String],
    ) -> lingxi_core::host::task_registry::TaskNotFoundRosters {
        let map = self.tasks.read().await;
        let aliases = self.aliases.read().await;
        // task id → its most addressable alias (`name@team` beats a bare name
        // beats the raw id), matching what the model is told to pass back.
        let mut addressable: HashMap<&str, &str> = HashMap::new();
        for (alias, task_id) in aliases.iter() {
            let better = addressable
                .get(task_id.as_str())
                .is_none_or(|current| !current.contains('@') && alias.contains('@'));
            if better {
                addressable.insert(task_id.as_str(), alias.as_str());
            }
        }

        let mut running_teammates = Vec::new();
        let mut background_agents = Vec::new();
        for (id, state) in map.iter() {
            match state {
                TaskState::InProcessTeammate(teammate)
                    if teammate.base.status == TaskStatus::Running =>
                {
                    running_teammates.push(
                        addressable
                            .get(id.as_str())
                            .map_or_else(|| teammate.agent_id.to_string(), ToString::to_string),
                    );
                }
                TaskState::LocalAgent(agent)
                    if (agent.base.status == TaskStatus::Running || agent.is_parked)
                        && agent.is_backgrounded
                        && Some(id.as_str()) != caller_agent_id
                        && Some(agent.agent_id.to_string().as_str()) != caller_agent_id
                        && Some(agent.agent_id.as_uuid().to_string().as_str())
                            != caller_agent_id
                        && agent.subagent_type != "main-session"
                        && !agent.is_observer
                        && !named_agent_ids.iter().any(|named| named == id) =>
                {
                    // `p.description ? `${p.id} (${Vn(p.description)})` : p.id`.
                    background_agents.push(if agent.base.description.is_empty() {
                        id.clone()
                    } else {
                        format!(
                            "{id} ({})",
                            lingxi_core::host::display::sanitize_display(&agent.base.description)
                        )
                    });
                }
                _ => {}
            }
        }
        running_teammates.sort();
        background_agents.sort();
        lingxi_core::host::task_registry::TaskNotFoundRosters {
            running_teammates,
            background_agents,
        }
    }

    /// claude-code `Ode` — move the task that owns `tool_use_id` to the
    /// background. Returns whether it moved.
    ///
    /// The oracle scans for the FIRST row with that `toolUseId` and returns
    /// early from the scan whether or not it could be backgrounded, so a
    /// terminal row does not fall through to some other task that happens to
    /// share the id.
    pub async fn background_task_for_tool_use(&self, tool_use_id: &str) -> bool {
        let target = {
            let map = self.tasks.read().await;
            map.iter()
                .find(|(_, state)| state.base().tool_use_id.as_deref() == Some(tool_use_id))
                .map(|(id, state)| (id.clone(), Self::task_is_backgroundable(state)))
        };
        match target {
            Some((id, true)) => self.background_task(&id).await,
            _ => false,
        }
    }

    /// claude-code `zM` — move everything backgroundable to the background and
    /// report how many moved.
    ///
    /// The oracle runs two passes over one snapshot: shells first, agents second.
    pub async fn background_all_tasks(&self) -> usize {
        self.background_all_tasks_with_reason(
            lingxi_core::host::task_registry::TaskBackgroundReason::User,
        )
        .await
    }

    /// Background-all preserves its originating trigger for tool results.
    pub async fn background_all_tasks_with_reason(
        &self,
        reason: lingxi_core::host::task_registry::TaskBackgroundReason,
    ) -> usize {
        let mut targets = {
            let map = self.tasks.read().await;
            map.iter()
                .filter(|(_, state)| Self::task_is_backgroundable(state))
                .map(|(id, state)| (id.clone(), !matches!(state, TaskState::LocalBash(_))))
                .collect::<Vec<_>>()
        };
        targets.sort_by_key(|(_, is_agent)| *is_agent);
        let mut moved = 0;
        for (id, _) in targets {
            if self.background_task_with_reason(&id, reason).await {
                moved += 1;
            }
        }
        self.tasks.feature("task_local_shell_background_all", None);
        moved
    }

    /// Kill the background shells a finishing agent started, and report how
    /// many were still running.
    ///
    /// claude-code sweeps these in its agent-run cleanup (`nHn`), which is what
    /// makes the Bash tool's promise true: a synchronous subagent is told its
    /// backgrounded command "is terminated when you give your final response".
    /// Only shells owned by THIS agent are touched, so the main session's and
    /// other agents' commands are untouched.
    ///
    /// The sweep is silent. `nHn` (`src_160988549.js` @3376778) is two halves —
    /// kill the running rows, then `dG((r)=>r.agentId===e)`, where `dG` is
    /// `pendingNotificationQueue.dequeueAllMatching`. Note the two halves range
    /// over DIFFERENT populations: the kill loop is gated on
    /// `status==="running"`, but the dequeue drops every pending notification
    /// addressed to the exiting agent, whatever produced it.
    ///
    /// This engine has no per-agent notification queue — it derives
    /// notifications from state (terminal AND not `notified`) — so the
    /// equivalent is to stamp `notified` on both populations: the rows this
    /// sweep kills, and the agent's shells that had already finished on their
    /// own with their completion still undrained. Without the first, tidying up
    /// after a subagent spits a `<task-notification> … was stopped` into the
    /// MAIN session for every shell it left running; without the second it does
    /// the same for every command the subagent finished but never read.
    ///
    /// Note this is the opposite of a user-initiated `TaskStop`, where the stop
    /// notification is the point; the oracle drops these because nothing is
    /// left to read them.
    pub async fn kill_background_shells_for_agent(
        &self,
        agent_id: lingxi_core::types::AgentId,
    ) -> usize {
        // Two lists, because `nHn` is two halves over two different
        // populations. The kill loop is gated on `status==="running"`, but
        // `dG((r)=>r.agentId===e)` afterwards drops EVERY pending notification
        // addressed to the exiting agent — including one from a shell that had
        // already finished on its own and whose completion nobody has read yet.
        let (live, already_terminal): (Vec<String>, Vec<String>) = {
            let map = self.tasks.read().await;
            let mut live = Vec::new();
            let mut already_terminal = Vec::new();
            for state in map.values() {
                let base = state.base();
                if state.base().task_type != TaskType::LocalBash
                    || base.creator_agent_id != Some(agent_id)
                {
                    continue;
                }
                if !lingxi_core::host::task_activity::is_live_shell_task(
                    &crate::handle::state_to_record(state),
                ) {
                    // Only rows that still owe a notification; a drained one has
                    // nothing left to suppress.
                    if !base.notified {
                        already_terminal.push(base.id.clone());
                    }
                } else {
                    live.push(base.id.clone());
                }
            }
            (live, already_terminal)
        };
        let mut killed = 0;
        for id in live {
            if self.kill(&id).await.is_ok() {
                killed += 1;
                // Belt and braces for the race `kill` deliberately excludes: it
                // stamps only a row THIS kill ended (`was_live`), so a shell
                // that settled between the snapshot above and the kill would
                // otherwise keep its notification. `dG` drops that one too.
                let _ = self.mark_notified(&id).await;
            }
        }
        // The rest of `dG`'s reach: finished-on-their-own shells the kill loop
        // never touches. Without this the sweep still leaked a
        // `<task-notification>` into the MAIN session for every command the
        // subagent happened to complete but not read.
        //
        // The explicit queue now carries recipients, so mirror dG's queue-wide
        // removal as well (monitor events and shell advisories included).
        self.pending_monitor_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|notification| notification.recipient_agent_id != Some(agent_id));
        for id in already_terminal {
            let _ = self.mark_notified(&id).await;
        }
        killed
    }

    /// THn quiet: claim once, transition terminal, publish only the SDK event.
    pub async fn publish_task_stopped(&self, task_id: &str) {
        let id = self.canonical_or_raw(task_id).await;
        let event = {
            let mut map = self.tasks.write().await;
            let Some(state) = map.get_mut(&id) else {
                return;
            };
            let Some(event) = shell_stopped_sdk_event(state) else {
                return;
            };
            state.base_mut().notified = true;
            event
        };
        let _ = self.set_status(&id, TaskStatus::Killed).await;
        self.tasks.emit(event);
    }

    pub fn subscribe_task_lifecycle(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<serde_json::Value> {
        self.tasks.subscribe()
    }

    /// Main-host interaction state for `jer`. Unwired hosts cannot reap.
    pub fn update_shell_session_activity(
        &self,
        interactive: bool,
        busy: bool,
        user_interaction: bool,
    ) {
        let mut state = self
            .shell_session_activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.interactive = interactive;
        state.busy = busy;
        if user_interaction {
            state.last_interaction = Some(std::time::Instant::now());
        }
    }

    /// Advisory prompt-stall event: the shell remains running and completion
    /// remains unclaimed. The process watchdog guarantees once per command.
    pub async fn notify_bash_stall(&self, task_id: &str, tail: &str) {
        {
            let map = self.tasks.read().await;
            let Some(TaskState::LocalBash(shell)) = map.get(task_id) else {
                return;
            };
            if shell.base.status != TaskStatus::Running || shell.is_backgrounded == Some(false) {
                return;
            }
            let notification = lingxi_core::host::task_registry::TaskNotification {
                task_id: task_id.to_string(),
                task_type: "local_bash".into(),
                recipient_agent_id: shell.base.creator_agent_id,
                status: "running".into(),
                description: shell.base.description.clone(),
                tool_use_id: shell.base.tool_use_id.clone(),
                output_path: Some(shell.base.output_file.to_string_lossy().into_owned()),
                result: Some(tail.to_string()),
                ..Default::default()
            };
            self.pending_monitor_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(notification);
        }
        self.bump_notification_revision();
        self.tasks.feature("task_local_shell_stall_detected", None);
    }

    /// Claim a shell stop on OS memory pressure. Session activity and every
    /// task are checked together before publishing a single stop notification.
    pub async fn claim_bash_memory_pressure_stop(&self, task_id: &str) -> bool {
        if std::env::var("LINGXI_CODE_DISABLE_BG_SHELL_PRESSURE_REAP")
            .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        {
            return false;
        }
        let (notification, cleanup) = {
            let mut map = self.tasks.write().await;
            let mut cleanups = self.cleanups.lock().await;
            let policy = self
                .shell_session_activity
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !policy.interactive
                || policy.busy
                || !policy
                    .last_interaction
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(1800))
            {
                return false;
            }
            if map.values().any(|state| {
                lingxi_core::host::task_activity::is_active_delegated_task(
                    &crate::handle::state_to_record(state),
                )
            }) {
                return false;
            }
            let Some(TaskState::LocalBash(shell)) = map.get_mut(task_id) else {
                return false;
            };
            if shell.base.status != TaskStatus::Running
                || shell.base.notified
                || shell.is_backgrounded == Some(false)
            {
                return false;
            }
            // A row can be visible before its native killer is bound. Do not
            // publish a stop until someone can actually take responsibility.
            let Some(cleanup) = cleanups.remove(task_id) else {
                return false;
            };
            shell.base.status = TaskStatus::Killed;
            shell.base.notified = true;
            shell.base.end_time = Some(SystemTime::now());
            shell.base.evict_after = Some(SystemTime::now() + Duration::from_secs(30));
            let notification = lingxi_core::host::task_registry::TaskNotification {
                task_id: task_id.to_string(),
                task_type: "local_bash".into(),
                recipient_agent_id: shell.base.creator_agent_id,
                status: "killed".into(),
                description: shell.base.description.clone(),
                tool_use_id: shell.base.tool_use_id.clone(),
                output_path: Some(shell.base.output_file.to_string_lossy().into_owned()),
                killed_by: Some("memory_pressure".into()),
                ..Default::default()
            };
            self.pending_monitor_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(notification.clone());
            (notification, cleanup)
        };
        // Transfer the actual kill before any cancellable output I/O. This
        // callback owns its spawned kill even if the polling caller disappears.
        cleanup();
        self.bump_notification_revision();
        self.tasks.feature("task_local_shell_pressure_reap", None);
        if let Some(path) = notification.output_path {
            let output_manager = self.output_manager.clone();
            let finish = tokio::spawn(async move {
                let path = std::path::Path::new(&path);
                let _ = output_manager
                    .append_shell_terminal(path, "\n[killed]\n")
                    .await;
                output_manager.evict_writer(path).await;
            });
            // Dropping this join handle detaches the finalizer, not its work.
            let _ = finish.await;
        }
        true
    }

    /// Assign shell terminal output to the independent native writer.
    pub async fn mark_shell_supervised(&self, id: &str) {
        let id = self.canonical_or_raw(id).await;
        if let Ok(output) = self.output_manager.path_for(&id) {
            self.output_manager.mark_supervised_terminal(&output).await;
        }
    }

    /// Settle a background shell task once its child has been reaped.
    ///
    /// Terminal status follows claude-code `Fpt`: an interrupted/killed child is
    /// `killed`, exit code `0` is `completed`, anything else (including an
    /// unknown code) is `failed`. The exit code is written first so the drained
    /// `<task-notification>` can render `(exit code N)`.
    ///
    /// # Errors
    /// Returns [`TaskError::NotFound`] when the id is unknown.
    pub async fn settle_background_bash(
        &self,
        task_id: &str,
        exit_code: Option<i32>,
        killed: bool,
    ) -> Result<(), TaskError> {
        self.settle_background_bash_with_cause(task_id, exit_code, killed, None)
            .await
    }

    /// Settle the background lifetime limit with the oracle's `deadline` cause.
    pub async fn settle_background_bash_deadline(
        &self,
        task_id: &str,
        exit_code: Option<i32>,
    ) -> Result<(), TaskError> {
        self.settle_background_bash_with_cause(task_id, exit_code, true, Some("deadline".into()))
            .await
    }

    async fn settle_background_bash_with_cause(
        &self,
        task_id: &str,
        exit_code: Option<i32>,
        killed: bool,
        stop_cause: Option<String>,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        {
            let rows = self.tasks.write().await;
            if !rows.contains_key(&task_id) {
                if let Some(pending) = self
                    .pending_bash_registration
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get_mut(&task_id)
                {
                    // Only a live allocation can retain a pre-registration receipt.
                    pending.get_or_insert((exit_code, killed, stop_cause));
                }
                return Ok(());
            }
        }
        if let Some(code) = exit_code {
            let _ = self.set_bash_exit_code(&task_id, code).await;
        }
        let status = if killed {
            TaskStatus::Killed
        } else if exit_code == Some(0) {
            TaskStatus::Completed
        } else {
            TaskStatus::Failed
        };
        let output_file = {
            let mut map = self.tasks.write().await;
            match map.get_mut(&task_id) {
                Some(entry) if entry.base().status.is_terminal() => return Ok(()),
                Some(entry) => {
                    if let TaskState::LocalBash(shell) = entry {
                        shell.stop_cause = stop_cause;
                    }
                    entry.base_mut().end_time = Some(SystemTime::now());
                    Some(entry.base().output_file.clone())
                }
                // No record: the identity was allocated for a command that was
                // never backgrounded, so there is nothing to settle.
                None => return Ok(()),
            }
        };
        // claude-code closes a background shell's output file with a status
        // trailer so a later `Read` shows how it ended (`Bpt`:
        // `s1e(e, "\n[" + (killed ? "killed" : "exited with code " + (code ?? "unknown")) + "]\n")`,
        // 2.1.263 `src_160988549.js` @4279785).
        if let Some(output_file) = output_file {
            let trailer = if killed {
                "\n[killed]\n".to_string()
            } else {
                let code = exit_code.map_or_else(|| "unknown".to_string(), |c| c.to_string());
                format!("\n[exited with code {code}]\n")
            };
            self.output_manager
                .append_shell_terminal(&output_file, &trailer)
                .await;
        }
        self.set_status(&task_id, status).await?;
        Ok(())
    }

    pub async fn settle_mcp_task(
        &self,
        task_id: &str,
        result_text: &str,
        failed: bool,
    ) -> Result<bool, TaskError> {
        self.settle_mcp_task_with_hint(task_id, result_text, failed, None)
            .await
    }

    pub async fn settle_mcp_task_with_hint(
        &self,
        task_id: &str,
        result_text: &str,
        failed: bool,
        saved_hint: Option<&str>,
    ) -> Result<bool, TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        // Guard + recover the spool path under a read lock.
        let output_file = {
            let map = self.tasks.read().await;
            match map.get(&task_id) {
                Some(s) if s.base().status.is_terminal() => return Ok(false),
                Some(s) => s.base().output_file.clone(),
                None => return Err(TaskError::NotFound(task_id)),
            }
        };
        // Persist the real result so the notification's `output-file` carries
        // it AND `TaskOutput` can serve it later. The inline `<result>` in the
        // notification is stamped separately, below, from the state — claude's
        // `F` is handed `resultText` directly and a renderer cannot read a file.
        let _ = self.output_manager.append(&output_file, result_text).await;
        // The call has settled, so it is no longer cancellable — drop the
        // cancel hook before the atomic terminal transition below (this also
        // makes the task drain-eligible once it goes terminal). Removing it
        // first keeps the `.await` on `cleanups.lock()` OUT of the write guard.
        self.cleanups.lock().await.remove(&task_id);
        let status = if failed {
            TaskStatus::Failed
        } else {
            TaskStatus::Completed
        };
        // Terminal transition + `mcpStatus`, done ATOMICALLY under ONE write
        // guard — the binary's single-closure `i.update(_, O => { if (O.notified)
        // return O; … })`. `end_time`, `mcpStatus`, AND the terminal status flip
        // all land under the same held guard, so a concurrent kill either wins
        // fully (settle's re-check below sees `Killed`/terminal and no-ops) or
        // loses fully — never a torn half-settled state. This is why settle can
        // no longer overwrite a kill (nor fire a spurious `TaskCompleted`): the
        // status write is inside the guard, not a separate `set_status`
        // acquisition split off by the `cleanups.lock()` await above.
        let settled_base = {
            let mut map = self.tasks.write().await;
            let Some(state) = map.get_mut(&task_id) else {
                return Err(TaskError::NotFound(task_id));
            };
            // Re-check terminal under the write lock: a kill may have raced in
            // (set `Killed` / `mcpStatus:"cancelled"`) after the read guard
            // above. If so, leave it untouched — the `if (O.notified) return O`
            // no-op — so a killed task is never resurrected as `Completed`.
            if state.base().status.is_terminal() {
                return Ok(false);
            }
            state.base_mut().end_time = Some(SystemTime::now());
            if let TaskState::McpTask(m) = state {
                m.mcp_status = if failed {
                    "failed".to_string()
                } else {
                    "completed".to_string()
                };
                // Only a COMPLETED call carries a result body upstream
                // (`F`'s `status==="completed" ? resultText ?? "" : …`); a
                // failure renders from `statusMessage` instead, so storing the
                // text here would put it somewhere nothing reads and leave a
                // failed row looking like it had an answer.
                if !failed {
                    m.result_text = Some(result_text.to_string());
                    m.saved_hint = saved_hint.map(str::to_string);
                }
            }
            state.base_mut().status = status;
            state.base().clone()
            // `map` write-guard drops here — the best-effort hook fire below
            // runs WITHOUT holding the registry lock (mirrors `set_status`).
        };
        // Best-effort `TaskCompleted` fire on the terminal transition THIS
        // settle just won (claude-code `executeTaskCompletedHooks`). Because the
        // status write above is atomic, a raced kill can never reach this fire:
        // it either wins the guard (settle no-ops before here) or loses it (and
        // its `Killed` transition, which has no claude-code counterpart, never
        // fires). Only `Completed` / `Failed` fire; no-op without a firer.
        let firer = self
            .task_completed_firer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(firer) = firer {
            let status_str = if failed { "failed" } else { "completed" };
            firer
                .fire(hooks::TaskCompletedFire {
                    task_id: task_id.to_string(),
                    status: status_str.to_string(),
                    task_subject: settled_base.description.clone(),
                    task_description: Some(settled_base.description.clone()),
                    teammate_name: settled_base.creator_teammate_name.clone(),
                    team_name: settled_base.creator_team_name.clone(),
                })
                .await;
        }
        Ok(true)
    }

    /// Spawn a task by dispatching to its registered per-type handler.
    ///
    /// Unlike [`create`](Self::create) — which only inserts a `Pending`
    /// placeholder row and never runs anything — `spawn` looks up the handler
    /// for `task_type`, runs [`Task::spawn`], and tracks the resulting task
    /// under the **handler-generated** `task_id` (e.g.
    /// `InProcessTeammateHandler` mints `t…` ids internally), which it returns.
    /// This is the load-bearing dispatch the implicit teammate service relies
    /// on to start a real worker.
    ///
    /// The typed [`TaskState`] is built from the real `input` fields (not the
    /// `AgentId::nil()` / empty-string placeholder `create` uses), and the
    /// task is recorded in the spawned-id index so [`kill`](Self::kill) can
    /// route teardown back to the owning handler.
    ///
    /// # Errors
    ///
    /// Returns [`TaskError::UnknownType`] if no handler is registered for
    /// `task_type`, [`TaskError::Io`] if the spool allocation fails, or
    /// whatever error the handler's [`Task::spawn`] surfaces.
    pub async fn spawn(
        &self,
        task_type: TaskType,
        input: TaskSpawnInput,
        description: String,
    ) -> Result<String, TaskError> {
        self.spawn_with_aliases(task_type, input, description, &[])
            .await
    }

    /// Publish restored task aliases in the same transaction as the row,
    /// before opening the handler's startup gate.
    pub async fn spawn_with_aliases(
        &self,
        task_type: TaskType,
        input: TaskSpawnInput,
        description: String,
        restored_aliases: &[String],
    ) -> Result<String, TaskError> {
        let _admission = self.lifecycle_gate.read().await;
        if !self.accepting_tasks.load(Ordering::Acquire) {
            return Err(TaskError::Internal("task registry is shutting down".into()));
        }
        // 1. Look up the per-type handler first — a missing handler is a clean
        //    error before any side effects (spool allocation, state insert).
        let handler = self
            .handlers
            .read()
            .await
            .get(&task_type)
            .ok_or(TaskError::UnknownType)?
            .clone();

        // 2. Dispatch to the handler. The handler allocates its OWN spool +
        //    `task_id` and prepares the real worker; the returned id is the one
        //    callers (and `kill`) key on. Handlers with an activation barrier
        //    do not start external work until the commit point below.
        let ctx = TaskContext {
            fs: self.fs.clone(),
            runtime: self.runtime.clone(),
        };
        let mut handle = handler.spawn(input.clone(), ctx).await?;
        let id = handle.task_id.clone();
        let cleanup = handle.cleanup.clone();
        let mut publication_guard = SpawnPublicationGuard::new(self, id.clone(), cleanup.clone());

        // 3. Recover the spool path the handler ALREADY allocated (the path is a
        //    deterministic function of the id) and insert the typed state built
        //    from the REAL input fields (not `create`'s placeholders), so
        //    `list()` / `get()` reflect a live spawned task.
        //
        //    We deliberately do NOT call `allocate(&id)` a second time here: the
        //    handler created the spool (and its worker may already be appending
        //    to it), so a re-allocate would either error on the exclusive
        //    (`O_EXCL`) create or — worse, pre-fix — truncate output the worker
        //    just wrote (the T4 double-allocate truncate race). `path_for` only
        //    reconstructs the path; it touches no bytes.
        let path = self
            .output_manager
            .path_for(&id)
            .map_err(|e| TaskError::Io(e.to_string()))?;
        // Keep a copy of the description for the `TaskCreated` fire below — the
        // original is moved into `base` here.
        let description_for_hook = description.clone();
        let base = TaskStateBase {
            id: id.clone(),
            task_type,
            status: TaskStatus::Running,
            description,
            tool_use_id: None,
            start_time: SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: path,
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        };
        let mut state = state_for_spawn(base, &input);
        if let (Some((model, profile)), TaskState::InProcessTeammate(teammate)) =
            (handle.teammate_model_route.as_ref(), &mut state)
        {
            teammate.child_model = Some(model.clone());
            teammate.child_model_profile.clone_from(profile);
        }
        if let TaskState::LocalAgent(agent) = &mut state {
            if agent.is_observer {
                let marker_path = agent.base.output_file.with_extension("observer.json");
                let marker_path = marker_path.to_string_lossy();
                let marker =
                    serde_json::json!({"agentType": agent.subagent_type, "isObserver": true})
                        .to_string();
                self.fs
                    .write_file(&marker_path, &marker)
                    .await
                    .map_err(|e| TaskError::Io(e.to_string()))?;
                let saved = self
                    .fs
                    .read_file(&marker_path, None, None)
                    .await
                    .map_err(|e| TaskError::Io(e.to_string()))?;
                if saved.content != marker {
                    return Err(TaskError::Io("observer marker read-back failed".into()));
                }
                agent.base.notified = true;
            }
        }
        // Fusion handlers resolve their live settings before returning the
        // prepared handle. Publish that captured summary with the state before
        // activation so print-mode waiters cannot observe a task without its
        // per-run deadline, and never need to reload mutable settings.
        if let (Some(summary), TaskState::LocalFusion(fusion)) =
            (handle.fusion_prepared_summary.as_ref(), &mut state)
        {
            fusion.run_id = Some(summary.identity.run_id.to_string());
            fusion.effective_timeout_ms = Some(summary.duration_ms);
            fusion.planned_panels = summary.planned_panels;
        }
        // 4. Publish every registry artifact as one transaction from the point
        //    of view of task readers. Holding the task write lock while the
        //    secondary guards are acquired prevents `list` / `get` / `kill`
        //    from observing a row before its handler route and cleanup exist.
        //    All guards are acquired before the first insert, so cancellation
        //    while waiting for a guard leaves no partial registry artifacts.
        let restoring_agent = matches!(&input, TaskSpawnInput::LocalAgent { spawn_request: Some(request), .. } if request.resumed_history.is_some());
        let mut spawn_aliases = aliases_for_spawn(&input);
        spawn_aliases.extend(restored_aliases.iter().cloned());
        {
            let mut tasks = self.tasks.write().await;
            let mut spawned = self.spawned.write().await;
            let mut cleanups = self.cleanups.lock().await;
            let mut aliases = self.aliases.write().await;
            if restored_aliases.iter().any(|alias| {
                alias.is_empty() || tasks.contains_key(alias) || aliases.contains_key(alias)
            }) || (restoring_agent
                && spawn_aliases
                    .iter()
                    .any(|alias| tasks.contains_key(alias) || aliases.contains_key(alias)))
            {
                return Err(TaskError::Internal(
                    "restored task or agent alias is already in use".into(),
                ));
            }

            tasks.insert(id.clone(), state);
            spawned.insert(id.clone(), task_type);
            if let Some(cleanup) = cleanup {
                cleanups.insert(id.clone(), cleanup);
            }
            for alias in spawn_aliases {
                if !alias.is_empty() && alias != id {
                    aliases.insert(alias, id.clone());
                }
            }
        }

        // Best-effort `TaskCreated` fire — the production task-creation path
        // (alongside `create`'s placeholder path). Both insert a new task row,
        // so both fire. Runs after the write guards drop. No-op when no firer is
        // registered.
        self.fire_task_created(&id, task_type, &description_for_hook)
            .await;

        // This is the commit point for handler-owned workers. Capture the
        // activation timestamps BEFORE the state write below: any scheduling
        // delay after this point belongs to the already-running Fusion budget,
        // while the awaited TaskCreated hook above does not. The Fusion worker
        // receives the same monotonic instant through its activation callback.
        let activation = FusionActivation::now();
        let activation_start_time = SystemTime::now();
        let fusion_deadline = handle.fusion_prepared_summary.as_ref().and_then(|summary| {
            activation
                .activated_at
                .checked_add(std::time::Duration::from_millis(summary.duration_ms))
        });
        if handle.fusion_prepared_summary.is_some() {
            let mut tasks = self.tasks.write().await;
            if let Some(task) = tasks.get_mut(&id) {
                task.base_mut().start_time = activation_start_time;
                if let TaskState::LocalFusion(fusion) = task {
                    fusion.fusion_activation_deadline = fusion_deadline;
                }
            }
        }
        // No await follows activation, so callers cannot observe a partially
        // committed successful spawn.
        handle.activate_at(activation);
        publication_guard.disarm();

        Ok(id)
    }

    /// Look up a task by ID.
    pub async fn get(&self, task_id: &str) -> Option<TaskState> {
        let task_id = self.canonical_or_raw(task_id).await;
        self.tasks.read().await.get(&task_id).cloned()
    }

    /// Return all known tasks.
    pub async fn list(&self) -> Vec<TaskState> {
        self.tasks.ordered_states().await
    }

    /// Scope workflow rows in `TaskList` / `/workflows` to one session.
    /// `None` leaves all workflows visible (desktop / single-session behavior).
    pub fn set_workflow_session_filter(&self, session_uuid: Option<String>) {
        let mut guard = self
            .workflow_session_filter
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = session_uuid;
    }

    pub(crate) fn workflow_visible_in_current_session(&self, state: &TaskState) -> bool {
        let Some(current_session) = self
            .workflow_session_filter
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        else {
            return true;
        };
        match state {
            TaskState::LocalWorkflow(workflow) => {
                workflow.session_uuid.as_deref() == Some(current_session.as_str())
            }
            _ => true,
        }
    }

    fn workflow_run_id_blocks_launch(status: TaskStatus) -> bool {
        matches!(status, TaskStatus::Pending | TaskStatus::Running)
    }

    /// Atomically reserve `run_id` for a new workflow launch so another launch
    /// cannot pass the live-run check before the task row exists.
    pub async fn try_reserve_workflow_run_id(
        &self,
        run_id: &str,
    ) -> Result<WorkflowRunReservation, TaskError> {
        {
            let mut reservations = self
                .workflow_launch_reservations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !reservations.insert(run_id.to_string()) {
                return Err(TaskError::Internal(format!(
                    "workflow run id {run_id} is already launching"
                )));
            }
        }
        let reservation = WorkflowRunReservation {
            reservations: self.workflow_launch_reservations.clone(),
            run_id: Some(run_id.to_string()),
        };
        let tasks = self.tasks.read().await;
        if let Some((task_id, status)) = tasks.iter().find_map(|(task_id, state)| match state {
            TaskState::LocalWorkflow(workflow)
                if workflow.run_id.as_deref() == Some(run_id)
                    && Self::workflow_run_id_blocks_launch(workflow.base.status) =>
            {
                Some((task_id.clone(), workflow.base.status))
            }
            _ => None,
        }) {
            let state = match status {
                TaskStatus::Pending => "pending",
                TaskStatus::Running => "running",
                _ => "live",
            };
            return Err(TaskError::Internal(format!(
                "workflow run id {run_id} is already {state} on task {task_id}"
            )));
        }
        drop(tasks);
        Ok(reservation)
    }

    /// Find a live `local_workflow` task whose effective run id equals
    /// `run_id`, returning its task id. Backs claude-code's resume gate
    /// (Workflow validateInput errorCode 3): a `resumeFromRunId` naming a
    /// still-running workflow must be rejected — two runs sharing a run id
    /// would race on the same journal.
    pub async fn find_running_workflow_by_run_id(&self, run_id: &str) -> Option<String> {
        let tasks = self.tasks.read().await;
        tasks.iter().find_map(|(task_id, state)| match state {
            TaskState::LocalWorkflow(w)
                if Self::workflow_run_id_blocks_launch(w.base.status)
                    && w.run_id.as_deref() == Some(run_id) =>
            {
                Some(task_id.clone())
            }
            _ => None,
        })
    }

    /// Return non-terminal local-app workflow tasks that hold authority over
    /// `app_id`. Delete flows use this as a guard before removing the app
    /// directory.
    ///
    /// Reads each task's typed [`crate::scope::LocalAppWorkflowTaskScope`]
    /// (design §18 Phase -1 step 8 / §8.1) instead of matching `workflow_id`
    /// against this crate's (since-deleted) `LOCAL_APP_BUILD_WORKFLOWS` array
    /// and then parsing `app_id` out of caller-supplied `args` JSON. That
    /// used to be a genuine forgery
    /// vector: a custom workflow could declare a `workflow_id` naming one of
    /// this crate's two real build workflows and an `args.app_id` for
    /// whichever app the forger chose, and this guard would block that
    /// app's delete on the forger's say-so alone (§8.1: a custom workflow
    /// must get nothing "即使伪造 `meta.name` 或 `args.app_id`"). `workflow_id`
    /// and `args` are now IGNORED here entirely -- a task blocks `app_id`'s
    /// delete iff it carries `Some(scope)` whose `scope.app_id() == app_id`
    /// (`scope.blocks_delete()`, which every purpose satisfies -- see that
    /// method's doc comment).
    ///
    /// A task with `scope: None` never blocks any app's delete. That is a
    /// deliberate choice, not an oversight: an unscoped row is one the Host
    /// never vouched for, and treating it as blocking would either (a) match
    /// by `workflow_id`/`args` again -- reopening the exact forgery vector
    /// above -- or (b) block deleting every app while any unscoped workflow
    /// runs, which a forged workflow could trivially exploit as a
    /// denial-of-service against unrelated apps. Denying by default is the
    /// only option that does not reintroduce caller-controlled authority.
    ///
    /// A scope reaches a row through
    /// [`crate::task_trait::TaskSpawnInput::LocalWorkflow`]'s `scope` field,
    /// minted by the Host that resolved the app -- so a genuine in-flight
    /// build DOES block its app's delete. See that field's doc comment for
    /// what a `Some` proves.
    /// The active workspace lease (checked independently by callers, e.g.
    /// `engine-mobile`'s `handle_delete_app`) remains a second guard for a
    /// `Build`-purpose task that has actually acquired it; this guard's job
    /// is the wider one -- also covering `UseTest`/`McpAuthoring` scopes,
    /// which never take that lease at all. `registry_test.rs`'s
    /// `a_build_purpose_blocks_delete_at_the_guard`,
    /// `a_use_test_purpose_still_blocks_delete_at_the_guard` and
    /// `an_mcp_authoring_purpose_still_blocks_delete_at_the_guard` pin one
    /// purpose each HERE, at the guard, which is a stronger claim than
    /// `scope.rs`'s `every_purpose_blocks_delete` (that one only proves the
    /// predicate's answer, not that this function asks it).
    ///
    /// Two of the three arms are reachable in production: `for_build` (three
    /// mints in `engine-mobile/src/workflow_support.rs`) and
    /// `for_mcp_authoring` (one, in `launch`). `for_use_test` has no
    /// production mint -- but not because its workflow is unbuilt: the use-test
    /// workflow script ships, and nothing mints its scope. That arm is
    /// live contract and dead traffic, exercised only by the tests named
    /// above. See `scope.rs`'s `blocks_delete` for the per-constructor
    /// line numbers.
    pub async fn find_nonterminal_local_app_workflows(&self, app_id: &str) -> Vec<String> {
        let tasks = self.tasks.read().await;
        tasks
            .iter()
            .filter_map(|(task_id, state)| match state {
                TaskState::LocalWorkflow(workflow)
                    if !workflow.base.status.is_terminal()
                        && workflow.scope.as_ref().is_some_and(|scope| {
                            scope.blocks_delete() && scope.app_id() == app_id
                        }) =>
                {
                    Some(task_id.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Attach the on-disk resume identity to a newly launched workflow row.
    pub async fn set_workflow_resume_metadata(
        &self,
        task_id: &str,
        script_path: String,
        transcript_dir: std::path::PathBuf,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut tasks = self.tasks.write().await;
        let state = tasks
            .get_mut(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        let TaskState::LocalWorkflow(workflow) = state else {
            return Err(TaskError::Internal(format!(
                "task {task_id} is not a local workflow"
            )));
        };
        workflow.script_path = Some(script_path);
        workflow.transcript_dir = Some(transcript_dir);
        Ok(())
    }

    /// Rebuild one workflow checkpoint without starting its script, with NO
    /// Local App authority (`scope: None`). This is the mobile equivalent of
    /// Claude Code's `registerAdoptedWorkflowTask`, and it is what a caller
    /// that has not independently re-validated the checkpoint against
    /// Host-owned state should call -- it reproduces the exact behavior this
    /// method has always had.
    ///
    /// See [`Self::register_adopted_workflow_with_scope`] for the sibling
    /// entry point a caller uses once it HAS re-resolved authority, and
    /// [`AdoptedWorkflow`]'s doc comment for why `scope` is never derived from
    /// the checkpoint's own fields here.
    pub async fn register_adopted_workflow(
        &self,
        adopted: AdoptedWorkflow,
    ) -> Result<(), TaskError> {
        self.register_adopted_workflow_with_scope(adopted, None)
            .await
    }

    /// Rebuild one workflow checkpoint, stamping `scope` on the resulting task
    /// row exactly as
    /// [`crate::task_trait::TaskSpawnInput::LocalWorkflow`]'s `scope` field
    /// does for a freshly spawned workflow.
    ///
    /// # The residual this closes
    ///
    /// [`Self::register_adopted_workflow`] always stamped `scope: None` on an
    /// adopted row, because [`AdoptedWorkflow`] is built straight from the
    /// on-disk checkpoint (`adopt.json`), and that checkpoint's fields are
    /// exactly as untrustworthy here as they are everywhere else in this
    /// crate: they record whatever the ORIGINAL caller supplied, so a custom
    /// workflow's checkpoint carries a forged `args.app_id` just as
    /// faithfully as a real build's. An adopted row is `Paused` --
    /// non-terminal -- so `scope: None` meant a Local App build in flight
    /// across an engine restart stopped blocking its app's delete, even
    /// though the pre-scope, name-matching guard this crate used to have DID
    /// block it. That is the gap this method closes.
    ///
    /// It closes the gap by taking `scope` as a SEPARATE argument rather than
    /// a field on [`AdoptedWorkflow`] itself, so the type that carries
    /// checkpoint-recorded (untrusted) fields can never be mistaken for
    /// Host-vouched authority. The caller (today, `engine-mobile`'s
    /// `MobileWorkflowCheckpointStore::adopt_session`) is responsible for
    /// RE-DERIVING `scope` from state it re-resolves on load -- the same
    /// app-must-exist / must-be-scaffolded / must-be-pinned checks
    /// `apply_materialized_local_app_collections_with_identity` applies to a
    /// live launch, run again here against the checkpoint's persisted script
    /// bytes, with the checkpoint's `app_id` used only as a lookup key, never
    /// as authority by itself (see that function's doc comment for the full
    /// reasoning, which applies unchanged). Passing `None` is always safe --
    /// it reproduces [`Self::register_adopted_workflow`]'s existing behavior
    /// -- so a caller that cannot or does not want to re-validate loses
    /// nothing by omitting scope.
    pub async fn register_adopted_workflow_with_scope(
        &self,
        adopted: AdoptedWorkflow,
        scope: Option<crate::scope::LocalAppWorkflowTaskScope>,
    ) -> Result<(), TaskError> {
        let valid_task_id = adopted.task_id.len() == 9
            && adopted
                .task_id
                .starts_with(TaskType::LocalWorkflow.id_prefix())
            && adopted
                .task_id
                .bytes()
                .skip(1)
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
        if !valid_task_id {
            return Err(TaskError::Internal(format!(
                "invalid adopted workflow task id {:?}",
                adopted.task_id
            )));
        }
        if !adopted.run_id.starts_with("wf_") {
            return Err(TaskError::Internal(format!(
                "invalid adopted workflow run id {:?}",
                adopted.run_id
            )));
        }

        let _reservation = self.try_reserve_workflow_run_id(&adopted.run_id).await?;
        let output_file = match self.output_manager.allocate(&adopted.task_id).await {
            Ok(path) => path,
            Err(crate::output_manager::OutputError::AlreadyExists(_)) => self
                .output_manager
                .path_for(&adopted.task_id)
                .map_err(|error| TaskError::Io(error.to_string()))?,
            Err(error) => return Err(TaskError::Io(error.to_string())),
        };
        let mut tasks = self.tasks.write().await;
        if let Some(existing) = tasks.get(&adopted.task_id) {
            if matches!(
                existing,
                TaskState::LocalWorkflow(workflow)
                    if Self::workflow_run_id_blocks_launch(workflow.base.status)
            ) {
                return Err(TaskError::Internal(format!(
                    "cannot adopt workflow {}: live task {} already exists",
                    adopted.run_id, adopted.task_id
                )));
            }
        }
        if let Some(existing_task_id) = tasks.iter().find_map(|(task_id, state)| match state {
            TaskState::LocalWorkflow(workflow)
                if workflow.run_id.as_deref() == Some(adopted.run_id.as_str())
                    && Self::workflow_run_id_blocks_launch(workflow.base.status) =>
            {
                Some(task_id.clone())
            }
            _ => None,
        }) {
            return Err(TaskError::Internal(format!(
                "cannot adopt workflow {}: live task {} already owns that run id",
                adopted.run_id, existing_task_id
            )));
        }
        let state = TaskState::LocalWorkflow(crate::state::LocalWorkflowTaskState {
            base: TaskStateBase {
                id: adopted.task_id.clone(),
                task_type: TaskType::LocalWorkflow,
                status: TaskStatus::Paused,
                description: adopted.description,
                tool_use_id: None,
                start_time: adopted.start_time,
                end_time: None,
                total_paused_ms: 0,
                output_file,
                evict_after: None,
                output_offset: 0,
                notified: true,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: None,
            },
            session_uuid: adopted.session_uuid,
            workflow_id: adopted.workflow_id,
            script: String::new(),
            resume_from_run_id: Some(adopted.run_id.clone()),
            args: adopted.args,
            run_id: Some(adopted.run_id),
            script_path: Some(adopted.script_path),
            transcript_dir: Some(std::path::PathBuf::from(adopted.transcript_dir)),
            current_step: 0,
            outcome: Default::default(),
            // `scope` is exactly what the caller passed to
            // `register_adopted_workflow_with_scope` -- `None` when reached
            // via the plain `register_adopted_workflow` wrapper (preserving
            // that method's pre-existing behavior), or whatever
            // `adopt_session` re-derived from Host-owned state for this
            // specific checkpoint. See this method's doc comment for the
            // residual this closes and why `scope` travels as a separate
            // argument rather than a field on `AdoptedWorkflow`. Note it is
            // still never PERSISTED (see `LocalWorkflowTaskState::scope`'s
            // doc comment) -- it is re-derived fresh on every adoption, same
            // as a live spawn re-derives it fresh on every launch.
            scope,
        });
        tasks.insert(adopted.task_id, state);
        Ok(())
    }

    /// Remove the paused predecessor after an explicit resume has successfully
    /// spawned a replacement task for the same workflow run in the same session.
    pub async fn remove_paused_workflow_by_run_id(&self, session_uuid: &str, run_id: &str) {
        self.tasks.write().await.retain(|_, state| {
            !matches!(
                state,
                TaskState::LocalWorkflow(workflow)
                    if workflow.base.status == TaskStatus::Paused
                        && workflow.session_uuid.as_deref() == Some(session_uuid)
                        && workflow.run_id.as_deref() == Some(run_id)
            )
        });
    }

    /// Record a shell-backed task's child exit code (M8 cc2.1.198 "Task
    /// panels: no stuck Running"): the worker's status sink reports the exit
    /// code alongside the terminal status once the process ends; `output()`
    /// projects it as `exit_code`/`done`. Non-bash variants are a benign
    /// no-op (only bash and monitor children carry an OS exit code). `NotFound` for an
    /// unknown id — the sink swallows it (racing teardown tolerance).
    /// Record a monitor's total stdout byte count (claude-code
    /// `pipedStdoutBytes`), which selects its completion summary.
    ///
    /// # Errors
    /// Returns [`TaskError::NotFound`] when the id is unknown.
    pub async fn set_monitor_stdout_bytes(
        &self,
        task_id: &str,
        bytes: u64,
    ) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        let entry = map
            .get_mut(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        if let TaskState::Monitor(m) = entry {
            m.stdout_bytes = Some(bytes);
        }
        Ok(())
    }

    pub async fn set_bash_exit_code(&self, task_id: &str, exit_code: i32) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        let entry = map
            .get_mut(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        match entry {
            TaskState::LocalBash(b) => b.exit_code = Some(exit_code),
            TaskState::Monitor(m) => m.exit_code = Some(exit_code),
            _ => {}
        }
        Ok(())
    }

    /// Queue one live `monitor_ws` stdout event for the next turn-boundary
    /// drain. The queue is deliberately bounded: a detached/high-volume
    /// producer must not grow session memory without limit while the model is
    /// busy. The monitor handler performs its own token-bucket suppression;
    /// this final cap protects the registry boundary as well.
    pub async fn enqueue_monitor_event(
        &self,
        task_id: &str,
        event: &str,
        housekeeping: bool,
    ) -> Result<(), TaskError> {
        const MAX_PENDING_MONITOR_EVENTS: usize = 1_024;

        let task_id = self.canonical_or_raw(task_id).await;
        {
            let tasks = self.tasks.read().await;
            let state = tasks
                .get(&task_id)
                .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
            let TaskState::Monitor(monitor) = state else {
                return Err(TaskError::Unsupported);
            };
            if monitor.base.status.is_terminal() {
                return Ok(());
            }
            let notification = lingxi_core::host::task_registry::TaskNotification {
                task_id: monitor.base.id.clone(),
                recipient_agent_id: monitor.base.creator_agent_id,
                task_type: "monitor_ws".to_string(),
                status: "running".to_string(),
                description: monitor.base.description.clone(),
                tool_use_id: monitor.base.tool_use_id.clone(),
                output_path: Some(monitor.base.output_file.to_string_lossy().into_owned()),
                result: Some(event.to_string()),
                // `GM(…,{isHousekeeping})` — a harness line about the monitor,
                // not script output. The renderer suppresses the per-event
                // push hint for these.
                monitor_housekeeping: housekeeping,
                ..Default::default()
            };
            let mut queue = self
                .pending_monitor_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.len() >= MAX_PENDING_MONITOR_EVENTS {
                queue.pop_front();
            }
            queue.push_back(notification);
        }
        self.bump_notification_revision();
        Ok(())
    }

    /// Test-only: force a `local_bash` task into a known status + exit code so
    /// `output()`'s `status`/`exit_code`/`done` projection can be exercised
    /// deterministically (`set_status` cannot set `exit_code`). Panics if the
    /// id is unknown or not a `local_bash` task.
    #[cfg(test)]
    pub(crate) async fn force_bash_terminal_for_test(
        &self,
        task_id: &str,
        status: TaskStatus,
        exit_code: Option<i32>,
    ) {
        let mut map = self.tasks.write().await;
        match map.get_mut(task_id) {
            Some(TaskState::LocalBash(b)) => {
                b.base.status = status;
                b.exit_code = exit_code;
            }
            _ => panic!("expected a local_bash task with id {task_id}"),
        }
    }

    /// Test-only: inject a fully-built [`TaskState`] into the task map under its
    /// own id. Lets tests exercise per-variant `output()` projections (e.g. the
    /// `local_agent` clean-result branch) without spinning up the variant's
    /// real handler.
    #[cfg(test)]
    /// Seed an alias → task-id mapping the way `aliases_for_spawn` would.
    #[cfg(test)]
    pub(crate) async fn register_alias_for_test(&self, alias: &str, task_id: &str) {
        self.aliases
            .write()
            .await
            .insert(alias.to_string(), task_id.to_string());
    }

    pub(crate) async fn insert_state_for_test(&self, state: TaskState) {
        let id = state.base().id.clone();
        self.tasks.write().await.insert(id, state);
    }

    async fn fire_task_completed_hook(
        &self,
        task_id: &str,
        status: TaskStatus,
        updated: &TaskState,
    ) {
        let firer = self
            .task_completed_firer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(firer) = firer {
            let status_str = match status {
                TaskStatus::Completed => Some("completed"),
                TaskStatus::Failed => Some("failed"),
                _ => None,
            };
            if let Some(status_str) = status_str {
                let base = updated.base();
                firer
                    .fire(hooks::TaskCompletedFire {
                        task_id: task_id.to_string(),
                        status: status_str.to_string(),
                        task_subject: base.description.clone(),
                        task_description: Some(base.description.clone()),
                        teammate_name: base.creator_teammate_name.clone(),
                        team_name: base.creator_team_name.clone(),
                    })
                    .await;
            }
        }
    }

    /// Publish the actual pool identity before the prepared agent is activated.
    pub async fn set_agent_display(&self, id: &str, model: String, effort: Option<String>) {
        let id = self.canonical_or_raw(id).await;
        if let Some(TaskState::LocalAgent(agent)) = self.tasks.write().await.get_mut(&id) {
            agent.outcome.model = (!model.is_empty()).then_some(model);
            agent.outcome.effort = effort;
        }
    }
    /// Publish a terminal-pane worker's admitted route in its parent task row.
    pub async fn publish_teammate_model_selection(
        &self,
        id: &str,
        selection: lingxi_core::host::team_spawn::TeammateModelSelection,
    ) -> Result<(), TeamSpawnError> {
        if selection.model.is_empty() {
            return Err(TeamSpawnError::Internal("teammate admitted an empty model selection".into()));
        }
        let id = self.canonical_or_raw(id).await;
        match self.tasks.write().await.get_mut(&id) {
            Some(TaskState::InProcessTeammate(teammate)) => {
                teammate.child_model = Some(selection.model);
                teammate.child_model_profile = selection.model_profile;
                Ok(())
            }
            Some(_) => Err(TeamSpawnError::Unsupported("task is not an in-process teammate".into())),
            None => Err(TeamSpawnError::NotFound(id)),
        }
    }
    pub async fn completed_agent_visible(&self, id: &str) -> bool {
        let map = self.tasks.read().await;
        let Some(TaskState::LocalAgent(agent)) = map.get(id) else {
            return false;
        };
        agent.base.status == TaskStatus::Completed
            && agent.is_backgrounded
            && !agent.is_observer
            && agent.subagent_type != "main-session"
            && agent.base.evict_after != Some(SystemTime::UNIX_EPOCH)
            && !self.has_live_background_children_locked(&map, Some(agent.agent_id), None, None)
    }

    pub async fn bind_agent_id(
        &self,
        task_id: &str,
        agent_id: lingxi_core::types::AgentId,
    ) -> Result<(), TaskError> {
        let canonical = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        let state = map
            .get_mut(&canonical)
            .ok_or_else(|| TaskError::NotFound(canonical.clone()))?;
        if state.is_terminated() {
            return Err(TaskError::TerminatedTask);
        }
        let TaskState::LocalAgent(agent) = state else {
            return Err(TaskError::Unsupported);
        };
        agent.agent_id = agent_id;
        let mut aliases = self.aliases.write().await;
        aliases.insert(agent_id.to_string(), canonical.clone());
        aliases.insert(agent_id.as_uuid().to_string(), canonical);
        drop(aliases);
        drop(map);
        self.bump_notification_revision();
        Ok(())
    }

    /// Record an authoritative idle event without ending the persistent task.
    pub async fn set_teammate_idle(&self, task_id: &str) -> Result<(), TaskError> {
        let id = self.canonical_or_raw(task_id).await;
        let mut tasks = self.tasks.write().await;
        let state = tasks
            .get_mut(&id)
            .ok_or_else(|| TaskError::NotFound(id.clone()))?;
        if let TaskState::InProcessTeammate(teammate) = state {
            if !teammate.base.status.is_terminal() {
                teammate.is_idle = true;
            }
        }
        drop(tasks);
        self.bump_notification_revision();
        Ok(())
    }

    /// Update one real local-Agent source fact consumed by the Native
    /// `$.agent.list` status reducer. Identity is the exact `AgentId`; no
    /// status, parked bit, or neighboring fact is used to infer an update.
    pub async fn update_agent_list_local_fact(
        &self,
        agent_id: lingxi_core::types::AgentId,
        update: lingxi_core::host::task_registry::AgentListLocalFactUpdate,
    ) -> Result<(), TaskError> {
        let mut tasks = self.tasks.write().await;
        let Some(agent) = tasks.values_mut().find_map(|state| match state {
            TaskState::LocalAgent(agent) if agent.agent_id == agent_id => Some(agent),
            _ => None,
        }) else {
            return Err(TaskError::NotFound(agent_id.as_uuid().to_string()));
        };
        use lingxi_core::host::task_registry::{AgentListLocalFactUpdate, FieldPresence};
        match update {
            AgentListLocalFactUpdate::IsIdle(value) => {
                agent.agent_list_lifecycle.is_idle = FieldPresence::Value(value);
            }
            AgentListLocalFactUpdate::Finalizing(value) => {
                agent.agent_list_lifecycle.finalizing = FieldPresence::Value(value);
            }
            AgentListLocalFactUpdate::KeepaliveReason { reason, active } => {
                agent.agent_list_lifecycle.keepalive_reasons =
                    match std::mem::take(&mut agent.agent_list_lifecycle.keepalive_reasons) {
                        FieldPresence::Value(mut reasons) => {
                            if active {
                                if !reasons.contains(&reason) {
                                    reasons.push(reason.clone());
                                }
                            } else {
                                reasons.retain(|existing| existing != &reason);
                            }
                            FieldPresence::Value(reasons)
                        }
                        FieldPresence::Null | FieldPresence::Missing if active => {
                            FieldPresence::Value(vec![reason])
                        }
                        other => other,
                    };
            }
        }
        drop(tasks);
        self.bump_notification_revision();
        Ok(())
    }

    pub async fn set_awaiting_plan_approval(
        &self,
        task_id: &str,
        awaiting: bool,
    ) -> Result<(), TaskError> {
        let id = self.canonical_or_raw(task_id).await;
        let mut tasks = self.tasks.write().await;
        let state = tasks
            .get_mut(&id)
            .ok_or_else(|| TaskError::NotFound(id.clone()))?;
        if let TaskState::InProcessTeammate(teammate) = state {
            teammate.awaiting_plan_approval = awaiting && !teammate.base.status.is_terminal();
        }
        Ok(())
    }

    /// Force `task_id`'s status to `status`. Returns
    /// [`TaskError::NotFound`] if the id is unknown. Only Bash and Agent
    /// variants currently carry a writable `status` field in the M1 surface;
    /// other variants are no-ops on the variant but still return the
    /// (possibly unchanged) state for the caller's consumption.
    pub async fn set_status(
        &self,
        task_id: &str,
        status: TaskStatus,
    ) -> Result<TaskState, TaskError> {
        self.set_status_with_agent_finalizing(task_id, status, None)
            .await
    }

    /// Complete the Native local-Agent status and its `finalizing` fact in a
    /// single row mutation. Native emits this intermediate completed state
    /// before the result notification, then clears the fact in its `finally`.
    pub async fn set_agent_status_with_finalizing(
        &self,
        task_id: &str,
        status: TaskStatus,
        finalizing: bool,
    ) -> Result<TaskState, TaskError> {
        if status != TaskStatus::Completed {
            return Err(TaskError::Unsupported);
        }
        self.set_status_with_agent_finalizing(task_id, status, Some(finalizing))
            .await
    }

    async fn set_status_with_agent_finalizing(
        &self,
        task_id: &str,
        status: TaskStatus,
        agent_finalizing: Option<bool>,
    ) -> Result<TaskState, TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        if matches!(status, TaskStatus::Failed | TaskStatus::Killed) {
            self.invalidate_handback_for_task(&task_id).await;
        }
        let updated = {
            let mut map = self.tasks.write().await;
            if agent_finalizing.is_some() {
                match map.get(&task_id) {
                    None => return Err(TaskError::NotFound(task_id.clone())),
                    Some(TaskState::LocalAgent(_)) => {}
                    Some(_) => return Err(TaskError::Unsupported),
                }
            }
            // Answered BEFORE the mutable borrow below: the keepalive question
            // is about the WHOLE map (does any live row name this agent as its
            // creator?), which cannot be asked while one entry is borrowed
            // mutably.
            let holds_children = match (status.is_terminal(), map.get(&task_id)) {
                (true, Some(TaskState::LocalAgent(agent))) => {
                    self.has_live_background_children_locked(&map, Some(agent.agent_id), None, None)
                }
                _ => false,
            };
            let entry = map
                .get_mut(&task_id)
                .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
            // Task lifecycle is absorbing at the first terminal transition.
            // In particular, a late handler failure must not overwrite an
            // explicit kill (or fire TaskCompleted after Killed already won).
            if entry.is_terminated() {
                return Ok(entry.clone());
            }
            match entry {
                TaskState::LocalBash(b) => {
                    if status.is_terminal() {
                        let mut cleanups = self.cleanups.lock().await;
                        b.base.status = status;
                        cleanups.remove(&task_id);
                    } else {
                        b.base.status = status;
                    }
                }
                TaskState::LocalAgent(a) => {
                    a.is_parked = false;
                    a.base.status = status;
                    if let Some(finalizing) = agent_finalizing {
                        a.agent_list_lifecycle.finalizing =
                            lingxi_core::host::task_registry::FieldPresence::Value(finalizing);
                    }
                    if status.is_terminal() {
                        // `bind_background_killer` inserts a cleanup for
                        // foreground agents too, and only the LocalBash arm
                        // above used to withdraw it — so every completed agent
                        // left a permanent entry that `shutdown_background_tasks`
                        // later walked and `kill`ed one by one.
                        self.cleanups.lock().await.remove(&task_id);
                    }
                    if !status.is_terminal() {
                        a.base.end_time = None;
                        a.base.evict_after = None;
                        a.base.notified = false;
                    }
                    if status == TaskStatus::Running {
                        // Turn-local diagnostics must not survive a resumed run.
                        // Retain the last answer/usage and worktree coordinates.
                        a.outcome.max_turns_reached = None;
                        a.error = None;
                    }
                }
                TaskState::RemoteAgent(r) => r.base.status = status,
                TaskState::InProcessTeammate(t) => {
                    t.base.status = status;
                    t.is_idle = false;
                    if status.is_terminal() {
                        t.awaiting_plan_approval = false;
                    }
                }
                TaskState::LocalWorkflow(w) => w.base.status = status,
                TaskState::MonitorMcp(m) => m.base.status = status,
                TaskState::Monitor(m) => m.base.status = status,
                TaskState::McpTask(m) => m.base.status = status,
                TaskState::AutoModeScan(s) => s.base.status = status,
                TaskState::Dream(d) => d.base.status = status,
                TaskState::LocalFusion(f) => f.base.status = status,
            }
            if status.is_terminal() {
                stamp_terminal_clock(entry, holds_children);
            }
            let updated = entry.clone();
            if matches!(updated, TaskState::LocalAgent(_)) {
                self.pending_rest.write().await.remove(&task_id);
            }
            updated
            // `map` write-guard drops here — the best-effort hook fire below
            // runs WITHOUT holding the registry lock so a slow/blocking hook
            // never stalls other task operations.
        };

        self.bump_notification_revision();
        if updated.is_terminated() {
            self.output_manager
                .evict_writer(&updated.base().output_file)
                .await;
        }
        self.fire_task_completed_hook(&task_id, status, &updated)
            .await;

        Ok(updated)
    }

    /// Mark a task as having had its terminal output consumed by a reader
    /// (claude-code `TaskOutputTool` `updateTaskState(task_id, t => ({ ...t,
    /// notified: true }))`, fired from both the non-blocking terminal branch and
    /// the blocking terminal branch). Setting `notified` suppresses a later
    /// duplicate `<task-notification>` for a task the model has already seen.
    ///
    /// Terminal tasks are retained after notification so `TaskList`,
    /// `TaskOutput`, and `TaskStop` can still address completed background work
    /// until an explicit cleanup/delete path removes it.
    ///
    /// Returns [`TaskError::NotFound`] if the id is unknown.
    pub async fn mark_notified(&self, task_id: &str) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        let entry = map
            .get_mut(&task_id)
            .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
        if self
            .shell_transfer_fences
            .lock()
            .unwrap()
            .contains(&task_id)
        {
            return Ok(());
        }
        entry.base_mut().notified = true;
        if entry.is_parked() {
            self.pending_rest.write().await.remove(&task_id);
        }
        Ok(())
    }

    /// Publish a completed turn and its persistent park atomically with the
    /// pending rest payload. A parked runner remains resumable after notification
    /// and eviction passes. Truly terminated tasks cannot be re-armed.
    pub async fn mark_task_rested(
        &self,
        task_id: &str,
        result: Option<String>,
        usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
        agent_id: Option<lingxi_core::types::AgentId>,
        agent_name: Option<String>,
        team_name: Option<String>,
        run: Option<lingxi_core::host::handback::HandbackRunKey>,
    ) {
        self.mark_task_rested_with_finalizing(
            task_id, result, usage, agent_id, agent_name, team_name, run, false,
        )
        .await;
    }

    /// Publish a persistent Agent rest and its Native completed/finalizing
    /// facts atomically. The notification drain clears `finalizing` after it
    /// builds the matching result notification. Observer rests suppress that
    /// notification and finish through their own digest handoff.
    #[allow(clippy::too_many_arguments)]
    pub async fn mark_task_rested_with_finalizing(
        &self,
        task_id: &str,
        result: Option<String>,
        usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
        agent_id: Option<lingxi_core::types::AgentId>,
        agent_name: Option<String>,
        team_name: Option<String>,
        run: Option<lingxi_core::host::handback::HandbackRunKey>,
        finalizing: bool,
    ) {
        let Some(task_id) = self.resolve_task_id(task_id).await else {
            return;
        };
        let mut map = self.tasks.write().await;
        let Some(state) = map.get_mut(&task_id) else {
            return;
        };
        if run.is_some_and(|expected| {
            state
                .handback()
                .is_none_or(|current| current.run != expected)
        }) {
            return;
        }
        if state.is_terminated() {
            return;
        }
        let TaskState::LocalAgent(agent) = state else {
            return;
        };
        if agent.outcome.killed_by.is_some() {
            return;
        }
        agent.is_parked = true;
        agent.base.status = TaskStatus::Completed;
        agent.agent_list_lifecycle.finalizing =
            lingxi_core::host::task_registry::FieldPresence::Value(finalizing);
        agent.base.end_time = Some(SystemTime::now());
        agent.base.evict_after = None;
        agent.base.notified = false;
        if agent.is_observer {
            agent.base.notified = true;
            let digest = (!agent.pending_messages.is_empty())
                .then(|| std::mem::take(&mut agent.pending_messages).join("\n\n"));
            if digest.is_some() {
                agent.is_parked = false;
                agent.base.status = TaskStatus::Running;
            }
            drop(map);
            self.bump_notification_revision();
            if let Some(digest) = digest {
                if let Err(error) = self.deliver_observer_digest(&task_id, digest).await {
                    tracing::warn!(task_id, %error, "observer activity delivery failed");
                }
            }
            self.finish_observer_rest_handoff(&task_id).await;
            return;
        }
        self.pending_rest.write().await.insert(
            task_id,
            RestPayload {
                result,
                usage,
                agent_id,
                agent_name,
                team_name,
            },
        );
        drop(map);
        self.bump_notification_revision();
    }

    fn has_live_background_children_locked(
        &self,
        map: &HashMap<String, TaskState>,
        rested_agent_id: Option<lingxi_core::types::AgentId>,
        agent_name: Option<&str>,
        team_name: Option<&str>,
    ) -> bool {
        fn holds_work(
            map: &HashMap<String, TaskState>,
            queued: &HashSet<lingxi_core::types::AgentId>,
            state: &TaskState,
            visited: &mut HashSet<lingxi_core::types::AgentId>,
        ) -> bool {
            if matches!(state, TaskState::LocalAgent(agent) if agent.is_observer)
                || matches!(state, TaskState::AutoModeScan(_))
            {
                return false;
            }
            if !state.base().status.is_terminal() {
                return true;
            }
            // Completion keeps its owner alive until the recipient has folded
            // the notification, even though the child model has already ended.
            if !state.base().notified {
                return true;
            }
            let TaskState::LocalAgent(agent) = state else {
                return false;
            };
            if !agent.is_parked || !visited.insert(agent.agent_id) {
                return false;
            }
            queued.contains(&agent.agent_id)
                || map.values().any(|child| {
                    child.base().creator_agent_id == Some(agent.agent_id)
                        && holds_work(map, queued, child, visited)
                })
        }
        let queued: HashSet<_> = self
            .pending_monitor_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|note| note.recipient_agent_id)
            .collect();
        let mut visited = HashSet::new();
        if let Some(rested_agent_id) = rested_agent_id {
            if queued.contains(&rested_agent_id) {
                return true;
            }
            return map.values().any(|state| {
                let base = state.base();
                base.creator_agent_id == Some(rested_agent_id)
                    && holds_work(map, &queued, state, &mut visited)
            });
        }

        let Some(agent_name) = agent_name.filter(|name| !name.is_empty()) else {
            return false;
        };
        map.values().any(|state| {
            let base = state.base();
            base.creator_teammate_name.as_deref() == Some(agent_name)
                && base.creator_team_name.as_deref() == team_name
                && holds_work(map, &queued, state, &mut visited)
        })
    }

    /// Drain the terminal tasks not yet surfaced to the model, marking each
    /// `notified` while retaining it so a completion is reported exactly once.
    /// Returns a [`TaskNotification`]
    /// snapshot per drained task, in registry-iteration order.
    ///
    /// This is the turn-boundary equivalent of claude-code's per-task-type
    /// completion callbacks (`enqueueShellNotification` / `enqueueAgentNotification`
    /// / …): a task that reaches a terminal status is reported once and then
    /// `notified`. A task ALREADY `notified` (its output consumed via
    /// `TaskOutput`/`TaskStop`) is skipped, so the model never sees a duplicate.
    /// Pending / running tasks are left untouched.
    ///
    /// Field mapping per type (mirroring the per-type `enqueue*Notification`):
    /// - `local_bash` carries `exit_code`;
    /// - `local_agent` carries `error` (its `failed` reason);
    /// - every type carries `tool_use_id` (when launched from a tool call) and
    ///   the spool `output_path`.
    /// Record a terminating `local_agent`'s notification payload onto its state
    /// ([`crate::state::AgentOutcomeState::merge`] semantics: `Some`
    /// overwrites, `None` leaves the stored value).
    ///
    /// `outcome.error` lands on the state's own `error` field — the failed
    /// summary's `{error}` (claude `error || 'Unknown error'`) reads from there,
    /// and until this existed nothing in production ever wrote it, so EVERY
    /// failed background agent reported `Unknown error`.
    ///
    /// Callers must invoke this BEFORE the terminal `set_status`; see
    /// [`lingxi_core::host::task_registry::TaskRegistryHandle::set_agent_outcome`].
    pub async fn set_agent_outcome(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalAgent(agent)) = map.get_mut(&task_id) {
            if let Some(handback) = &outcome.handback {
                if agent
                    .handback
                    .as_ref()
                    .is_some_and(|current| current.run != handback.run)
                {
                    return;
                }
                // A late terminal projection cannot overwrite a newer run or
                // manufacture a receipt after its live authority was stopped.
                if agent.handback.as_ref().is_some_and(|current| {
                    current.run == handback.run
                        && current.active == handback.active
                        && current.receipt == handback.receipt
                }) {
                    agent.handback = Some(handback.clone());
                }
            }
            if let Some(error) = outcome.error.clone() {
                agent.error = Some(error);
            }
            agent.outcome.merge(outcome);
        }
    }

    /// Replace the local Agent's conversation snapshot with the runner's
    /// settled history. The final snapshot includes admitted transcript
    /// rewrites and excludes rows removed by server fallback/tombstones.
    pub async fn replace_agent_transcript_messages(
        &self,
        task_id: &str,
        messages: Vec<lingxi_core::types::ConversationMessage>,
    ) -> Result<(), TaskError> {
        let task_id = self
            .resolve_task_id(task_id)
            .await
            .unwrap_or_else(|| task_id.to_owned());
        let mut rows = self.tasks.write().await;
        let Some(state) = rows.get_mut(&task_id) else {
            return Err(TaskError::NotFound(task_id));
        };
        let TaskState::LocalAgent(agent) = state else {
            return Err(TaskError::Unsupported);
        };
        agent.messages = messages;
        Ok(())
    }

    /// Native K3's one-time notification claim, restricted to the exact raw
    /// launch identity currently stored on this local-agent row.
    pub(crate) async fn claim_agent_wait_notification(
        &self,
        task_id: &str,
        raw_agent_id: &str,
    ) -> Result<
        Option<AgentWaitNotificationClaim>,
        lingxi_core::host::task_registry::TaskRegistryError,
    > {
        let registry = self.self_ref.get().cloned().ok_or_else(|| {
            lingxi_core::host::task_registry::TaskRegistryError::Internal(
                "task registry self binding is unavailable for wait cleanup".into(),
            )
        })?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| {
            lingxi_core::host::task_registry::TaskRegistryError::Internal(format!(
                "task wait requires a Tokio runtime: {error}"
            ))
        })?;
        let mut rows = self.tasks.write().await;
        let Some(agent_id) = rows.get(task_id).and_then(|state| match state {
            TaskState::LocalAgent(agent)
                if agent.agent_id.as_uuid().to_string() == raw_agent_id =>
            {
                Some(agent.agent_id)
            }
            _ => None,
        }) else {
            return Ok(None);
        };
        let generation = {
            let mut claims = self
                .agent_wait_claims
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match claims.get_mut(task_id) {
                Some(claim) if claim.agent_id == agent_id => {
                    claim.active_waiters = claim.active_waiters.saturating_add(1);
                    claim.generation
                }
                _ => {
                    let generation = self
                        .next_agent_wait_claim_generation
                        .fetch_add(1, Ordering::Relaxed);
                    claims.insert(
                        task_id.to_owned(),
                        AgentWaitClaimRecord {
                            generation,
                            active_waiters: 1,
                            agent_id,
                        },
                    );
                    generation
                }
            }
        };
        let Some(TaskState::LocalAgent(agent)) = rows.get_mut(task_id) else {
            // The task lock is held, so this can only be reached if a future
            // state variant breaks the identity predicate above.
            self.agent_wait_claims
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(task_id);
            return Ok(None);
        };
        agent.base.notified = true;
        Ok(Some(AgentWaitNotificationClaim {
            registry,
            runtime,
            task_id: task_id.to_owned(),
            generation,
            armed: true,
        }))
    }

    /// Finish the direct Agent-tool handoff for a completion whose K3 claim
    /// already suppressed the registry notification. The terminal waiter is
    /// the result handoff, so clear `finalizing` only after it has captured the
    /// settled outcome.
    async fn release_agent_wait_claim(&self, task_id: &str, generation: u64) {
        let changed = {
            let mut rows = self.tasks.write().await;
            let current_agent_id = rows.get(task_id).and_then(|state| match state {
                TaskState::LocalAgent(agent) => Some(agent.agent_id),
                _ => None,
            });
            let matches_claim = {
                let mut claims = self
                    .agent_wait_claims
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(claim) = claims.get_mut(task_id) else {
                    return;
                };
                if claim.generation != generation {
                    return;
                }
                if current_agent_id != Some(claim.agent_id) {
                    claims.remove(task_id);
                    return;
                }
                if claim.active_waiters > 1 {
                    claim.active_waiters -= 1;
                    return;
                }
                claims.remove(task_id);
                true
            };
            if !matches_claim {
                return;
            }
            let Some(TaskState::LocalAgent(agent)) = rows.get_mut(task_id) else {
                return;
            };
            if agent.base.status.is_terminal() && agent.base.notified {
                // A terminal claim cannot be reset: the completion path has
                // already suppressed its normal notification. Complete the
                // producer-finally fact instead of leaving agent.list at a
                // permanent running projection.
                clear_agent_finalizing_fact(agent)
            } else if !agent.base.status.is_terminal() && agent.base.notified {
                // qOt restores notification eligibility when the last live
                // waiter leaves before the task has settled.
                agent.base.notified = false;
                true
            } else {
                false
            }
        };
        if changed {
            self.bump_notification_revision();
        }
    }

    async fn finish_agent_wait_claim(&self, task_id: &str, generation: u64) {
        let changed = {
            let mut rows = self.tasks.write().await;
            let current_agent_id = rows.get(task_id).and_then(|state| match state {
                TaskState::LocalAgent(agent) => Some(agent.agent_id),
                _ => None,
            });
            let matches_claim = {
                let mut claims = self
                    .agent_wait_claims
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(claim) = claims.get(task_id) else {
                    return;
                };
                if claim.generation == generation && current_agent_id == Some(claim.agent_id) {
                    claims.remove(task_id);
                    true
                } else if claim.generation == generation {
                    claims.remove(task_id);
                    false
                } else {
                    false
                }
            };
            if !matches_claim {
                return;
            }
            let Some(TaskState::LocalAgent(agent)) = rows.get_mut(task_id) else {
                return;
            };
            if agent.base.status.is_terminal() && agent.base.notified {
                clear_agent_finalizing_fact(agent)
            } else if !agent.base.status.is_terminal() && agent.base.notified {
                agent.base.notified = false;
                true
            } else {
                false
            }
        };
        if changed {
            self.bump_notification_revision();
        }
    }

    /// Observer rests suppress the ordinary task notification (and may instead
    /// deliver a digest), so their finalizing transition has its own handoff.
    async fn finish_observer_rest_handoff(&self, task_id: &str) {
        let changed = {
            let mut rows = self.tasks.write().await;
            let Some(TaskState::LocalAgent(agent)) = rows.get_mut(task_id) else {
                return;
            };
            if !agent.is_observer || !agent.base.notified {
                return;
            }
            clear_agent_finalizing_fact(agent)
        };
        if changed {
            self.bump_notification_revision();
        }
    }

    /// Record a workflow's terminal payload before the status transition opens
    /// it to the notification drain.
    pub async fn set_workflow_outcome(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
    ) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalWorkflow(workflow)) = map.get_mut(&task_id) {
            workflow.outcome = outcome;
        }
    }

    /// Record a Fusion run's sanitized final text before the terminal status
    /// opens it to the notification drain.
    pub async fn set_fusion_outcome(&self, task_id: &str, run_id: String, final_text: String) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalFusion(fusion)) = map.get_mut(&task_id) {
            fusion.run_id = Some(run_id);
            fusion.final_text = Some(final_text);
        }
    }

    /// Record a Fusion run's failure reason before the terminal `Failed`
    /// status opens it to the notification drain. Without this a failed
    /// `local_fusion` task notified as bare "failed" with no `<error>`
    /// section and no reason folded into the summary.
    pub async fn set_fusion_error(&self, task_id: &str, error: String) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalFusion(fusion)) = map.get_mut(&task_id) {
            fusion.error = Some(error);
        }
    }

    /// Record a Fusion run's current progress-stage label (F005) — e.g.
    /// "Running panels 2/3" — surfaced on the `local_fusion` task DTO so a
    /// UI polling task state sees the same progress the Agent-tool path
    /// forwards as `subagent_activity`. Best-effort: a since-evicted task is
    /// a benign no-op.
    pub async fn set_fusion_stage(&self, task_id: &str, stage: String) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalFusion(fusion)) = map.get_mut(&task_id) {
            fusion.stage = Some(stage);
        }
    }

    /// Record the typed publication receipt for a Fusion result. The
    /// computational answer and terminal task status are intentionally kept
    /// even when publication fails, while the legacy boolean is derived only
    /// from the one receipt that proves a durable append.
    pub async fn set_fusion_publication(
        &self,
        task_id: &str,
        receipt: lingxi_core::host::FusionPublicationReceipt,
    ) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalFusion(fusion)) = map.get_mut(&task_id) {
            fusion.publication_status = receipt.status;
            fusion.publication_error = receipt.error;
        }
    }

    /// Record a Fusion run's egress profiles and usage summary alongside its
    /// terminal payload, before the terminal status opens it to the
    /// notification drain.
    pub async fn set_fusion_egress_and_usage(
        &self,
        task_id: &str,
        egress_profiles: Vec<String>,
        usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
    ) {
        let task_id = self.canonical_or_raw(task_id).await;
        let mut map = self.tasks.write().await;
        if let Some(TaskState::LocalFusion(fusion)) = map.get_mut(&task_id) {
            fusion.egress_profiles = egress_profiles;
            fusion.usage = usage;
        }
    }

    /// Atomically publish a Fusion run's terminal payload and terminal status.
    pub async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        run_id: String,
        final_text: String,
        status: TaskStatus,
    ) -> Result<TaskState, TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let updated = {
            let mut map = self.tasks.write().await;
            let entry = map
                .get_mut(&task_id)
                .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
            let TaskState::LocalFusion(fusion) = entry else {
                return Ok(entry.clone());
            };
            if fusion.base.status.is_terminal() {
                return Ok(entry.clone());
            }
            fusion.run_id = Some(run_id);
            fusion.final_text = Some(final_text);
            fusion.base.status = status;
            entry.clone()
        };

        // The terminal flip is what makes the notification drainable, so it
        // must move the revision like every other terminal path: an idle host
        // waits on this to start the turn where the parent model synthesizes
        // from the material.
        self.bump_notification_revision();
        self.fire_task_completed_hook(&task_id, status, &updated)
            .await;
        Ok(updated)
    }

    /// Atomically publish a workflow's terminal payload and terminal status.
    ///
    /// This closes the outcome→status race: a concurrent kill/drain cannot
    /// observe a non-terminal workflow after its terminal payload already
    /// landed and then overwrite it as `Killed`.
    pub async fn finish_workflow_terminal(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        status: TaskStatus,
    ) -> Result<TaskState, TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let updated = {
            let mut map = self.tasks.write().await;
            let entry = map
                .get_mut(&task_id)
                .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
            let TaskState::LocalWorkflow(workflow) = entry else {
                return Ok(entry.clone());
            };
            if workflow.base.status.is_terminal() {
                return Ok(entry.clone());
            }
            workflow.outcome = outcome;
            workflow.base.status = status;
            entry.clone()
        };

        self.fire_task_completed_hook(&task_id, status, &updated)
            .await;
        Ok(updated)
    }

    /// Atomically commit a Host-validated Local App Build/UseTest success.
    ///
    /// This is intentionally not a generic workflow terminal primitive.  It
    /// accepts only a [`TaskState::LocalWorkflow`] row carrying an authenticated
    /// Build or UseTest scope, and it exists because those two workflows have
    /// two local publications that must linearize with the absorbing task
    /// state: the canonical result projection and the Host's already-prepared
    /// active QA receipt pointer.
    ///
    /// Expensive QA/evidence validation must happen before this method.  While
    /// the registry write lock is held, this method writes a bounded unverified
    /// spool placeholder, runs `publish_prepared`, makes canonical success
    /// authoritative only after publication, then assigns the exact resulting
    /// outcome/status. A competing kill therefore wins before these
    /// publications (and the closure is not called), or observes the committed
    /// terminal state afterward. `publish_prepared` must not call back into
    /// this registry, perform UI/network work, scan QA artifacts, or do large
    /// cleanup; those operations would introduce lock inversion or make the
    /// critical section unbounded. The returned boolean is true only when
    /// this call committed the prepared publication; an already-terminal row
    /// is returned with false so callers cannot emit a success summary for a
    /// publication closure that never ran.
    pub async fn commit_local_app_workflow_terminal<F, Fut>(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        publish_prepared: F,
    ) -> Result<(TaskState, bool), TaskError>
    where
        F: FnOnce() -> Fut + Send,
        Fut: std::future::Future<Output = Result<(), String>> + Send,
    {
        let canonical_result = outcome.result.clone().ok_or_else(|| {
            TaskError::Internal(
                "Local App terminal commit requires a canonical result payload".into(),
            )
        })?;
        let task_id = self.canonical_or_raw(task_id).await;
        let (updated, publication_committed) = {
            let mut map = self.tasks.write().await;
            let entry = map
                .get_mut(&task_id)
                .ok_or_else(|| TaskError::NotFound(task_id.clone()))?;
            let TaskState::LocalWorkflow(workflow) = entry else {
                return Err(TaskError::Unsupported);
            };
            let eligible = workflow.scope.as_ref().is_some_and(|scope| {
                matches!(
                    scope.purpose(),
                    crate::scope::LocalAppWorkflowPurpose::Build
                        | crate::scope::LocalAppWorkflowPurpose::UseTest
                )
            });
            if !eligible {
                return Err(TaskError::Unsupported);
            }
            if workflow.base.status.is_terminal() {
                return Ok((entry.clone(), false));
            }

            let output_file = workflow.base.output_file.clone();
            let canonical_result_error = self
                .output_manager
                .validate_terminal_result(&output_file, &canonical_result)
                .err()
                .map(|error| {
                    format!(
                        "local_app_completion_unverified: terminal spool replacement failed: {error}"
                    )
                });
            let publication_pending = serde_json::json!({
                "ok": false,
                "error": "local_app_completion_unverified: QA publication pending",
                "verified": false,
            })
            .to_string();
            let mut publication_committed = false;
            let commit_error = match canonical_result_error {
                Some(error) => Some(error),
                None => match self
                    .output_manager
                    .replace_terminal_result(&output_file, &publication_pending)
                    .await
                {
                    Ok(()) => match publish_prepared().await {
                        Ok(()) => {
                            publication_committed = true;
                            if let Err(error) = self
                                .output_manager
                                .replace_terminal_result_authoritative(
                                    &output_file,
                                    &canonical_result,
                                )
                                .await
                            {
                                tracing::warn!(
                                    task_id = %task_id,
                                    %error,
                                    "canonical Local App result is authoritative in memory; physical spool persistence failed after Host publication"
                                );
                            }
                            None
                        }
                        Err(error) => Some(format!(
                            "local_app_completion_unverified: QA publication failed: {error}"
                        )),
                    },
                    Err(error) => Some(format!(
                        "local_app_completion_unverified: terminal spool replacement failed: {error}"
                    )),
                },
            };

            if let Some(mut reason) = commit_error {
                let failure_payload = serde_json::json!({
                    "ok": false,
                    "error": reason,
                    "verified": false,
                })
                .to_string();
                // Make the failed projection authoritative before retrying
                // the filesystem rewrite. The physical spool still contains
                // only an unverified placeholder if this retry also fails.
                if let Err(error) = self
                    .output_manager
                    .replace_terminal_result_fail_closed(&output_file, &failure_payload)
                    .await
                {
                    reason.push_str(&format!(
                        "; terminal failure spool replacement failed: {error}"
                    ));
                }
                workflow.outcome = lingxi_core::host::task_registry::WorkflowTerminalOutcome {
                    result: None,
                    error: Some(reason),
                    ..outcome
                };
                workflow.base.status = TaskStatus::Failed;
            } else {
                workflow.outcome = outcome;
                workflow.base.status = TaskStatus::Completed;
            }
            (entry.clone(), publication_committed)
        };

        let status = updated.base().status;
        self.bump_notification_revision();
        if updated.is_terminated() {
            self.output_manager
                .evict_writer(&updated.base().output_file)
                .await;
        }
        self.fire_task_completed_hook(&task_id, status, &updated)
            .await;
        Ok((updated, publication_committed))
    }

    /// [`Self::kill`] with the stop initiator recorded (`"parent"` / `"user"`).
    ///
    /// The reason is stamped BEFORE the kill dispatch so a handler that flips
    /// the task to `Killed` synchronously cannot let the drain observe a
    /// terminal task whose `killed_by` has not landed yet.
    /// Worker liveness is independent of the retained task's terminal status.
    pub async fn has_live_task_loop(&self, task_id: &str) -> bool {
        let canonical = self.canonical_or_raw(task_id).await;
        let task_type = self.spawned.read().await.get(&canonical).copied();
        let handler = match task_type {
            Some(kind) => self.handler_for(kind).await,
            None => None,
        };
        match handler {
            Some(handler) => handler.has_live_worker(&canonical).await,
            None => false,
        }
    }

    /// Process ownership is captured from actual pool allocation receipts.
    pub async fn process_owner_ids(&self, task_id: &str) -> Vec<lingxi_core::types::AgentId> {
        let canonical = self.canonical_or_raw(task_id).await;
        if let Some(TaskState::LocalAgent(agent)) = self.get(&canonical).await {
            return vec![agent.agent_id];
        }
        let task_type = self.spawned.read().await.get(&canonical).copied();
        let handler = match task_type {
            Some(kind) => self.handler_for(kind).await,
            None => None,
        };
        match handler {
            Some(handler) => handler.process_owner_ids(&canonical).await,
            None => Vec::new(),
        }
    }

    pub async fn kill_with_reason(&self, task_id: &str, killed_by: &str) -> Result<(), TaskError> {
        // Stop is the other half of the human-resume transaction. Keep the
        // teardown alive if the UI/turn caller is cancelled while worktree or
        // handler cleanup is awaiting.
        if let Some(registry) = self.owned_self() {
            let task_id = task_id.to_owned();
            let killed_by = killed_by.to_owned();
            return tokio::spawn(async move {
                registry.kill_with_reason_inner(&task_id, &killed_by).await
            })
            .await
            .map_err(|error| {
                TaskError::Internal(format!("task stop transaction joined: {error}"))
            })?;
        }
        self.kill_with_reason_inner(task_id, killed_by).await
    }

    async fn kill_with_reason_inner(
        &self,
        task_id: &str,
        killed_by: &str,
    ) -> Result<(), TaskError> {
        let canonical = self.canonical_or_raw(task_id).await;
        self.invalidate_handback_for_task(&canonical).await;
        let _human_resume = if killed_by == "user" {
            // Cancel before waiting: an in-progress human prepare must release
            // this same gate, then old teardown completes before a new epoch runs.
            Some(
                self.invalidate_human_messages(&canonical)
                    .lock_owned()
                    .await,
            )
        } else {
            None
        };
        let mut was_live_agent = false;
        if let Some(TaskState::LocalAgent(agent)) = self.tasks.write().await.get_mut(&canonical) {
            // Same reverse-race guard `kill` applies to the status: a task that
            // already finished on its own is not "stopped by" anyone, and
            // stamping one would leave a completed task carrying a stop
            // initiator.
            if !agent.base.status.is_terminal() || agent.is_parked {
                agent.outcome.killed_by = Some(killed_by.to_string());
                was_live_agent = true;
            }
        }
        // claude-code captures `ue = GS(I)` BEFORE the kill, because the kill
        // clears the keepalive reasons the gate reads. Same here: resolve the
        // cascade target while the task is still resting.
        // `tengu_agent_tool_terminated` (async twin). Emitted only when this
        // call is what stopped the agent — the same reverse-race guard the
        // `killed_by` stamp uses, so a task that had already finished on its own
        // is not reported as terminated by anyone.
        if was_live_agent {
            if let Some(state) = self.tasks.read().await.get(&canonical) {
                self.emit_agent_tool_terminated(state, killed_by).await;
            }
        }
        let cascade_from = self.resting_agent_holding_children(&canonical).await;
        let result = self.kill(task_id).await;
        if let Some(agent_id) = cascade_from {
            self.cascade_stop_descendants(agent_id, killed_by).await;
        }
        if killed_by == "user" {
            self.tasks.feature(
                "task_stop_user",
                result.as_ref().err().map(|error| match error {
                    TaskError::NotFound(_) => "not_found",
                    _ => "unexpected",
                }),
            );
        }
        result
    }

    /// claude-code `GS(e)` — `type==="local_agent" && status==="completed" &&
    /// keepaliveReasons.size > 0`: an agent that already came to rest but is
    /// held open by live background children. Returns its agent id.
    ///
    /// The park is independent of the once-only pending notification. The
    /// child keepalive reasons are derived from the registry's ownership edges.
    ///
    /// Both halves are load-bearing. Cascading on live children ALONE would kill
    /// the children of an agent that is still actively working, which the oracle
    /// does not do; cascading on rest alone would fire for an agent with nothing
    /// left to hold open.
    async fn resting_agent_holding_children(
        &self,
        canonical: &str,
    ) -> Option<lingxi_core::types::AgentId> {
        // Lock order is tasks → pending_rest everywhere else in this file; keep
        // it.
        let map = self.tasks.read().await;
        let TaskState::LocalAgent(agent) = map.get(canonical)? else {
            return None;
        };
        if !agent.is_parked {
            return None;
        }
        let agent_id = agent.agent_id;
        self.has_live_background_children_locked(&map, Some(agent_id), None, None)
            .then_some(agent_id)
    }

    /// claude-code's cascade block (`rY` @3598005): every still-live
    /// `local_agent` DESCENDANT of the stopped agent is stopped with it.
    ///
    /// Without this, stopping a resting parent leaves its children running with
    /// nothing left to report to — the parent that would have collected them is
    /// gone.
    ///
    /// Two things the oracle gets for free that this has to build:
    ///
    /// 1. **A parent index.** The oracle walks `r[p]` because a `local_agent`'s
    ///    task id IS its agent id. Here they are independent (`generate_task_id`
    ///    vs `LocalAgentTaskState::agent_id`), so the walk needs an explicit
    ///    `agent_id → task_id` map or it silently stops after one level and
    ///    grandchildren survive.
    /// 2. **Silence.** `CI(Me.id,r)` stamps `notified` before each child kill,
    ///    so the oracle emits nothing per cascaded child. This engine derives
    ///    notifications from `terminal && !notified`, so without the same stamp
    ///    a cascade would push one `Agent "…" was stopped by Claude` into the
    ///    parent's session per descendant — the hazard already documented on
    ///    [`Self::kill_background_shells_for_agent`].
    ///
    /// Children are stopped serially and their errors ignored, as the oracle
    /// does: one dead child must not abort the rest of the cascade.
    async fn cascade_stop_descendants(
        &self,
        stopped_agent_id: lingxi_core::types::AgentId,
        killed_by: &str,
    ) {
        let victims: Vec<String> = {
            let map = self.tasks.read().await;
            let by_agent: HashMap<lingxi_core::types::AgentId, &TaskState> = map
                .values()
                .filter_map(|state| match state {
                    TaskState::LocalAgent(agent) => Some((agent.agent_id, state)),
                    _ => None,
                })
                .collect();
            map.values()
                .filter_map(|state| {
                    let TaskState::LocalAgent(agent) = state else {
                        return None;
                    };
                    if agent.agent_id == stopped_agent_id || state.is_terminated() {
                        return None;
                    }
                    // `hVe`: walk UP the parent chain, with the oracle's
                    // visited-set cycle guard.
                    let mut seen = std::collections::HashSet::new();
                    let mut parent = agent.base.creator_agent_id;
                    while let Some(pid) = parent {
                        if pid == stopped_agent_id {
                            return Some(agent.base.id.clone());
                        }
                        if !seen.insert(pid) {
                            break;
                        }
                        parent = by_agent.get(&pid).and_then(|s| s.base().creator_agent_id);
                    }
                    None
                })
                .collect()
        };
        for id in victims {
            // `CI` before the kill, exactly as the oracle orders it.
            let _ = self.mark_notified(&id).await;
            let _ = Box::pin(self.kill_with_reason_inner(&id, killed_by)).await;
        }
    }

    async fn activate_notification_recipient_locked(
        &self,
        map: &mut HashMap<String, TaskState>,
        recipient: Option<lingxi_core::types::AgentId>,
        has_notifications: bool,
    ) -> bool {
        let mut activated = false;
        if has_notifications {
            if let Some(recipient) = recipient {
                let mut resumed_ids = Vec::new();
                for state in map.values_mut() {
                    match state {
                        TaskState::LocalAgent(agent)
                            if agent.agent_id == recipient && agent.is_parked =>
                        {
                            agent.is_parked = false;
                            agent.base.status = TaskStatus::Running;
                            agent.base.end_time = None;
                            agent.base.evict_after = None;
                            agent.base.notified = false;
                            agent.outcome.max_turns_reached = None;
                            agent.error = None;
                            resumed_ids.push(agent.base.id.clone());
                            activated = true;
                        }
                        TaskState::InProcessTeammate(agent)
                            if agent.agent_id == recipient && agent.is_idle =>
                        {
                            agent.is_idle = false;
                            activated = true;
                        }
                        _ => {}
                    }
                }
                let mut rest = self.pending_rest.write().await;
                for id in resumed_ids {
                    rest.remove(&id);
                }
            }
        }
        activated
    }

    fn notification_recipient(
        state: &TaskState,
        map: &HashMap<String, TaskState>,
    ) -> Option<lingxi_core::types::AgentId> {
        let owner = match state.handback().map(|state| state.recipient) {
            Some(lingxi_core::host::handback::HandbackRecipient::Main { .. }) => return None,
            Some(lingxi_core::host::handback::HandbackRecipient::Agent { agent_id, .. }) => {
                agent_id
            }
            None => state.base().creator_agent_id?,
        };
        // lLe/IRe: an agent/workflow whose owner no longer has a live runner
        // reports to the main session instead of stranding the completion.
        if matches!(
            state,
            TaskState::LocalAgent(_) | TaskState::LocalWorkflow(_)
        ) {
            let live_owner = map.values().any(|candidate| match candidate {
                TaskState::LocalAgent(agent) => {
                    agent.agent_id == owner
                        && (agent.base.status == TaskStatus::Running || agent.is_parked)
                }
                TaskState::InProcessTeammate(agent) => {
                    agent.agent_id == owner && !agent.base.status.is_terminal()
                }
                _ => false,
            });
            if !live_owner {
                return None;
            }
        }
        Some(owner)
    }

    pub(crate) fn bump_notification_revision(&self) {
        self.notification_revision
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }

    /// Subscribe before checking pending work to avoid a check/wait race.
    pub fn subscribe_task_notifications(&self) -> tokio::sync::watch::Receiver<u64> {
        self.notification_revision.subscribe()
    }

    /// Test readiness without consuming any recipient’s notifications.
    pub async fn has_pending_task_notifications_for(
        &self,
        recipient: Option<lingxi_core::types::AgentId>,
    ) -> bool {
        if self
            .pending_monitor_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|n| {
                n.recipient_agent_id == recipient
                    && !self
                        .shell_transfer_fences
                        .lock()
                        .unwrap()
                        .contains(&n.task_id)
            })
        {
            return true;
        }
        let map = self.tasks.read().await;
        if map.iter().any(|(id, state)| matches!(state, TaskState::LocalAgent(agent) if Some(agent.agent_id) == recipient && !state.is_terminated()) && self.has_human_messages(id)) { return true; }

        if map.values().any(|s| {
            Self::notification_recipient(s, &map) == recipient
                && s.base().status.is_terminal()
                && !s.base().notified
                && !self
                    .shell_transfer_fences
                    .lock()
                    .unwrap()
                    .contains(&s.base().id)
                && !self
                    .stopping_shells
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .contains_key(&s.base().id)
                && !matches!(s, TaskState::LocalAgent(a) if a.is_parked || a.is_observer)
        }) {
            return true;
        }
        self.pending_rest.read().await.iter().any(|(id, payload)| {
            map.get(id).is_some_and(|s| {
                Self::notification_recipient(s, &map) == recipient
                    && !self.has_live_background_children_locked(
                        &map,
                        payload.agent_id,
                        payload.agent_name.as_deref(),
                        payload.team_name.as_deref(),
                    )
            })
        })
    }

    /// Drain main-session notifications only.
    /// `Kan`/`Dlo`: consume byte deltas for running output watermarks without
    /// attaching their text. Completion rendering reads the full output later.
    async fn refresh_output_offsets(&self, recipient: Option<lingxi_core::types::AgentId>) {
        if recipient.is_some() {
            return;
        }
        let running: Vec<_> = self
            .tasks
            .read()
            .await
            .values()
            .filter(|state| state.base().status == TaskStatus::Running)
            .map(|state| {
                (
                    state.base().id.clone(),
                    state.base().output_file.clone(),
                    state.base().output_offset,
                )
            })
            .collect();
        for (id, file, offset) in running {
            if let Ok((content, next)) = self.output_manager.read_delta(&file, offset).await {
                if content.is_empty() {
                    continue;
                }
                if let Some(state) = self.tasks.write().await.get_mut(&id) {
                    if state.base().status == TaskStatus::Running
                        && state.base().output_offset == offset
                    {
                        state.base_mut().output_offset = next;
                    }
                }
            }
        }
    }

    pub async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        self.take_pending_task_notifications_for(None).await
    }

    /// Atomically select and consume notifications belonging to this recipient.
    pub async fn take_pending_task_notifications_for(
        &self,
        recipient: Option<lingxi_core::types::AgentId>,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        self.refresh_output_offsets(recipient).await;
        use crate::handle::{status_to_wire, task_type_to_wire};
        let mut map = self.tasks.write().await;
        let mut out = Vec::new();
        self.pending_monitor_events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|n| {
                if n.recipient_agent_id == recipient
                    && !self
                        .shell_transfer_fences
                        .lock()
                        .unwrap()
                        .contains(&n.task_id)
                {
                    out.push(n.clone());
                    false
                } else {
                    true
                }
            });
        let mut activated = self
            .activate_notification_recipient_locked(&mut map, recipient, !out.is_empty())
            .await;
        // TID-04: evict the rows a PREVIOUS pass already notified, BEFORE this
        // one marks any new ones.
        //
        // Order is the whole point. claude-code runs the sweep (`Kan`/`Dlo`) on
        // the attachment pass, where every candidate was notified by an earlier
        // pass; evicting after the drain below would delete a row in the same
        // breath as telling the model about it, and `/tasks` would never show a
        // finished task at all. Running it first reproduces the oracle's
        // one-pass grace without needing a timestamp to express it.
        let agent_handler = self.handler_for(TaskType::LocalAgent).await;
        let evicted_outputs = self.evict_notified_terminal_rows(&mut map, agent_handler.as_ref());
        // Collect ids first (terminal + not-notified) so the per-id remove below
        // doesn't fight the iteration borrow.
        let drain_ids: Vec<String> = map
            .iter()
            .filter(|(_, s)| {
                let b = s.base();
                b.status.is_terminal()
                    && !b.notified
                    && !self.shell_transfer_fences.lock().unwrap().contains(&b.id)
                    && Self::notification_recipient(s, &map) == recipient
                    && !self
                        .stopping_shells
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains_key(&b.id)
                    && !matches!(s, TaskState::LocalAgent(a) if a.is_parked || a.is_observer)
            })
            .map(|(id, _)| id.clone())
            .collect();
        out.reserve(drain_ids.len());
        for id in drain_ids {
            let Some(state) = map.get(&id) else {
                continue;
            };
            let b = state.base();
            // Per-type fields the renderer needs beyond the shared base.
            let exit_code = match state {
                TaskState::LocalBash(bash) => bash.exit_code,
                TaskState::Monitor(monitor) => monitor.exit_code,
                // (the stdout byte count is monitor-only; see below)
                _ => None,
            };
            let error = match state {
                TaskState::LocalAgent(agent) => agent.error.clone(),
                TaskState::LocalWorkflow(workflow) => workflow.outcome.error.clone(),
                TaskState::LocalFusion(fusion) => fusion
                    .error
                    .clone()
                    .or_else(|| fusion.publication_error.clone()),
                _ => None,
            };
            let workflow_outcome = match state {
                TaskState::LocalWorkflow(workflow) => Some(workflow.outcome.clone()),
                _ => None,
            };
            let workflow_metadata = match state {
                TaskState::LocalWorkflow(workflow) => Some((
                    workflow.script_path.clone(),
                    workflow.run_id.clone(),
                    workflow.args.clone(),
                    workflow
                        .transcript_dir
                        .as_ref()
                        .map(|path| path.to_string_lossy().into_owned()),
                )),
                _ => None,
            };
            let workflow_counts_available = workflow_outcome
                .as_ref()
                .is_some_and(|outcome| outcome.progress_counts_available);
            // `local_agent` optional sections — the payload the terminating run
            // reported via `set_agent_outcome` (and the stop initiator recorded
            // by `kill_with_reason`). Every other task type carries none of
            // them, which is the renderer's omit-the-clause case.
            let agent_outcome = match state {
                TaskState::LocalAgent(agent) => Some(agent.outcome.clone()),
                _ => None,
            };
            let agent_outcome = agent_outcome.unwrap_or_default();
            let fusion_state = match state {
                TaskState::LocalFusion(fusion) => Some(fusion.clone()),
                _ => None,
            };
            let fusion_final_text = fusion_state.as_ref().and_then(|f| f.final_text.clone());
            let output_path = self
                .output_manager
                .physical_output_is_authoritative(&b.output_file)
                .await
                .then(|| b.output_file.to_string_lossy().into_owned());
            out.push(lingxi_core::host::task_registry::TaskNotification {
                task_id: b.id.clone(),
                recipient_agent_id: Self::notification_recipient(state, &map),
                task_type: task_type_to_wire(b.task_type).to_string(),
                status: status_to_wire(b.status).to_string(),
                description: b.description.clone(),
                tool_use_id: b.tool_use_id.clone(),
                output_path,
                exit_code,
                stdout_bytes: match state {
                    TaskState::Monitor(monitor) => monitor.stdout_bytes,
                    _ => None,
                },
                error,
                // `local_agent` `<result>` / `<usage>`: the terminating run's
                // final text and usage rollup, reported through
                // `set_agent_outcome` before its terminal status. `None` stays
                // the byte-faithful "no result" case (a run that produced no
                // text, or a non-agent task). `local_fusion` reports its own
                // usage summary through `set_fusion_egress_and_usage`.
                usage: agent_outcome
                    .usage
                    .or_else(|| fusion_state.as_ref().and_then(|f| f.usage.clone())),
                // `killed_by` (the by-Claude/by-user split, from
                // `kill_with_reason`) + the isolation `<worktree>` section (the
                // KEPT worktree's path/branch, from the handler's terminal
                // `agent_worktree_result` judgment). `None` ⇒ the bare
                // `was stopped` verb / no worktree section.
                killed_by: agent_outcome.killed_by.or_else(|| match state {
                    TaskState::LocalBash(shell) => shell.stop_cause.clone(),
                    _ => None,
                }),
                worktree_path: agent_outcome.worktree_path,
                worktree_branch: agent_outcome.worktree_branch,
                // Set only when the run ended by exhausting its turn budget;
                // selects the turn-limit variant of the `completed` summary.
                max_turns_reached: agent_outcome.max_turns_reached,
                // Terminal notifications are never monitor events.
                monitor_housekeeping: false,
                // `mcp_task` only — what claude-code's `F` needs beyond the
                // registry id. `mcpStatus` (not the registry status) drives
                // both the summary's trailing word and the `<status>` tag.
                mcp: match state {
                    TaskState::McpTask(m) => {
                        Some(lingxi_core::host::task_registry::McpTaskNotificationMeta {
                            server_name: m.server_name.clone(),
                            tool_name: m.tool_name.clone(),
                            mcp_status: m.mcp_status.clone(),
                            status_message: m.status_message.clone(),
                            saved_hint: m.saved_hint.clone(),
                        })
                    }
                    _ => None,
                },
                result: workflow_outcome
                    .as_ref()
                    .and_then(|outcome| outcome.result.clone())
                    .or(agent_outcome.result)
                    .or(fusion_final_text)
                    .or_else(|| match state {
                        // The settled call's text, stamped by
                        // `settle_mcp_task`. Only a completed call has one.
                        TaskState::McpTask(m) => m.result_text.clone(),
                        _ => None,
                    }),
                workflow_failures: workflow_outcome
                    .as_ref()
                    .map(|outcome| outcome.failures.clone())
                    .unwrap_or_default(),
                workflow_agent_count: workflow_outcome.as_ref().map(|outcome| outcome.agent_count),
                workflow_total_tokens: workflow_outcome
                    .as_ref()
                    .map(|outcome| outcome.total_tokens),
                workflow_total_tool_calls: workflow_outcome
                    .as_ref()
                    .map(|outcome| outcome.total_tool_calls),
                workflow_duration_ms: workflow_outcome.as_ref().map(|outcome| outcome.duration_ms),
                workflow_script_path: workflow_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.0.clone()),
                workflow_run_id: workflow_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.1.clone()),
                workflow_args: workflow_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.2.clone()),
                workflow_transcript_dir: workflow_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.3.clone()),
                workflow_agents_done: workflow_counts_available
                    .then(|| workflow_outcome.as_ref().map(|outcome| outcome.agents_done))
                    .flatten(),
                workflow_agents_error: workflow_counts_available
                    .then(|| {
                        workflow_outcome
                            .as_ref()
                            .map(|outcome| outcome.agents_error)
                    })
                    .flatten(),
                workflow_agents_skipped: workflow_counts_available
                    .then(|| {
                        workflow_outcome
                            .as_ref()
                            .map(|outcome| outcome.agents_skipped)
                    })
                    .flatten(),
                workflow_agents_empty_result: workflow_counts_available
                    .then(|| {
                        workflow_outcome
                            .as_ref()
                            .map(|outcome| outcome.agents_empty_result)
                    })
                    .flatten(),
                egress_profiles: fusion_state
                    .as_ref()
                    .map(|f| f.egress_profiles.clone())
                    .unwrap_or_default(),
            });
            // Mark notified so the completion surfaces exactly once. The task
            // itself stays addressable until an explicit cleanup/delete removes
            // it.
            if let Some(state) = map.get_mut(&id) {
                state.base_mut().notified = true;
                if let TaskState::LocalAgent(agent) = state {
                    if matches!(
                        agent.agent_list_lifecycle.finalizing,
                        lingxi_core::host::task_registry::FieldPresence::Value(true)
                    ) {
                        agent.agent_list_lifecycle.finalizing =
                            lingxi_core::host::task_registry::FieldPresence::Value(false);
                    }
                }
            }
        }

        // A persistent completed+keepalive agent surfaces once per rest without
        // losing its live runner — claude-code's
        // "fires each time this agent comes to rest … the same task-id may notify
        // more than once." Drain + clear the armed set; the agent re-arms on its
        // next rest. A task that raced to terminal is skipped here (the terminal
        // drain above already owns it).
        let rest_payloads: Vec<(String, RestPayload)> = {
            let mut armed = self.pending_rest.write().await;
            let ids: Vec<_> = armed
                .keys()
                .filter(|id| {
                    map.get(*id)
                        .is_some_and(|state| Self::notification_recipient(state, &map) == recipient)
                })
                .cloned()
                .collect();
            ids.into_iter()
                .filter_map(|id| armed.remove(&id).map(|payload| (id, payload)))
                .collect::<Vec<_>>()
        };
        let mut deferred_rest = Vec::new();
        for (id, payload) in rest_payloads {
            let Some(state) = map.get(&id) else { continue };
            let b = state.base();
            if Self::notification_recipient(state, &map) != recipient {
                deferred_rest.push((id, payload));
                continue;
            }
            if b.status.is_terminal() && !matches!(state, TaskState::LocalAgent(a) if a.is_parked) {
                continue;
            }
            if self.has_live_background_children_locked(
                &map,
                payload.agent_id,
                payload.agent_name.as_deref(),
                payload.team_name.as_deref(),
            ) {
                deferred_rest.push((id, payload));
                continue;
            }
            out.push(lingxi_core::host::task_registry::TaskNotification {
                task_id: b.id.clone(),
                recipient_agent_id: Self::notification_recipient(state, &map),
                task_type: task_type_to_wire(b.task_type).to_string(),
                // A resting persistent agent completed this turn-set while its
                // keepalive retains the runner for future notifications/messages.
                status: status_to_wire(TaskStatus::Completed).to_string(),
                description: b.description.clone(),
                tool_use_id: b.tool_use_id.clone(),
                // The spool carries the just-produced turn-set result; the model
                // reads it (it was told it can Read/Bash-tail the output file).
                output_path: Some(b.output_file.to_string_lossy().into_owned()),
                exit_code: None,
                // A resting AGENT has no stdout stream to have measured.
                stdout_bytes: None,
                error: None,
                // The agent's final-text response + run usage, captured at rest
                // time. The binary `enqueueAgentNotification` always emits these
                // when a result exists; the renderer omits each clause when `None`.
                result: payload.result,
                usage: payload.usage,
                // A rest notification is not a kill and carries no worktree
                // metadata on the rest payload, so both stay `None` (see the
                // terminal-drain note above; renderer-ready when populated).
                killed_by: None,
                worktree_path: None,
                worktree_branch: None,
                // A persistent agent can come to rest by exhausting its turn
                // budget just as a one-shot can end that way, and the oracle
                // passes `maxTurnsReached` on every `enqueueAgentNotification`
                // — rest included. The budget does not ride on `RestPayload`
                // (that carries only what `notify_rest` is handed); the handler
                // reports it through `set_agent_outcome` before it arms the
                // rest, so it is read back off the stored outcome here.
                max_turns_reached: match state {
                    TaskState::LocalAgent(agent) => agent.outcome.max_turns_reached,
                    _ => None,
                },
                // A rest notification is never a monitor event.
                monitor_housekeeping: false,
                // A rest notification only ever fires for a `local_agent`.
                mcp: None,
                workflow_failures: Vec::new(),
                workflow_agent_count: None,
                workflow_total_tokens: None,
                workflow_total_tool_calls: None,
                workflow_duration_ms: None,
                workflow_script_path: None,
                workflow_run_id: None,
                workflow_args: None,
                workflow_transcript_dir: None,
                workflow_agents_done: None,
                workflow_agents_error: None,
                workflow_agents_skipped: None,
                workflow_agents_empty_result: None,
                // A rest notification only ever fires for a `local_agent`,
                // which has no egress-profile concept.
                egress_profiles: Vec::new(),
            });
            if let Some(state) = map.get_mut(&id) {
                state.base_mut().notified = true;
                if let TaskState::LocalAgent(agent) = state {
                    if matches!(
                        agent.agent_list_lifecycle.finalizing,
                        lingxi_core::host::task_registry::FieldPresence::Value(true)
                    ) {
                        agent.agent_list_lifecycle.finalizing =
                            lingxi_core::host::task_registry::FieldPresence::Value(false);
                    }
                }
            }
        }
        // Consuming an owner's message and reactivating that owner are one
        // transaction. Otherwise a grandparent can observe an idle owner with
        // all children notified and release its keepalive before the runner
        // gets to its separate activation await.
        activated |= self
            .activate_notification_recipient_locked(&mut map, recipient, !out.is_empty())
            .await;
        drop(map);
        if activated {
            self.bump_notification_revision();
        }
        for output in evicted_outputs {
            self.output_manager.release_output_state(&output).await;
        }
        self.requeue_deferred_rest(deferred_rest).await;
        out
    }

    async fn requeue_deferred_rest(&self, deferred_rest: Vec<(String, RestPayload)>) {
        if deferred_rest.is_empty() {
            return;
        }
        let mut armed = self.pending_rest.write().await;
        for (id, payload) in deferred_rest {
            armed.entry(id).or_insert(payload);
        }
    }

    /// Stop task admission and ask every published background owner to finish
    /// or cancel. The exclusive admission fence closes the spawn/snapshot gap:
    /// after it is released, no task can appear outside this drain set.
    /// Teardown attempts every task and returns the first error only afterward.
    pub async fn shutdown_background_tasks(&self) -> Result<(), TaskError> {
        let admission = self.lifecycle_gate.write().await;
        self.accepting_tasks.store(false, Ordering::Release);
        let mut task_ids = self
            .spawned
            .read()
            .await
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        task_ids.extend(self.handles.lock().await.keys().cloned());
        task_ids.extend(self.cleanups.lock().await.keys().cloned());
        task_ids.sort();
        task_ids.dedup();
        drop(admission);

        let mut first_error = None;
        for task_id in task_ids {
            if let Err(error) = self.kill(&task_id).await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        let handlers = self
            .handlers
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for handler in &handlers {
            if let Err(error) = handler.drain_shutdown().await {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }

        // A failed cancellation must not skip another handler's natural
        // finalization barrier. Attempt every drain before reporting failure,
        // but retain the graph for retry when any owner could still be live:
        // a handler's default no-op drain cannot prove a failed kill stopped it.
        if let Some(error) = first_error {
            return Err(error);
        }

        // Every handler-owned worker has now acknowledged teardown. Release
        // the handler graph before the registry itself is dropped so deferred
        // tool invokers cannot retain a path back to this registry (and thus
        // to the old session's budget/persistence authorities).
        self.handlers.write().await.clear();
        // Task lifecycle firers are allowed to share the production hook
        // executor. That executor can reach the agent spawner's late-bound tool
        // registry, whose builtin context points back to this registry. Keep the
        // firers live through every handler drain above (terminal hook ordering),
        // then sever both back edges before returning from shutdown.
        let task_created_firer = {
            self.task_created_firer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        };
        let task_completed_firer = {
            self.task_completed_firer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        };
        // Trait-object destructors may tear down an entire composition graph
        // and re-enter registry code. Never run them while either firer mutex
        // is held.
        drop((task_created_firer, task_completed_firer));
        Ok(())
    }

    /// Kill a task, cancelling its background handle if any.
    ///
    /// Two paths, decided by how the task was started:
    /// - A task started via [`spawn`](Self::spawn) is torn down through its
    ///   owning handler ([`Task::kill`]) — the handler manages its own runtime
    ///   task internally, so there is no `BackgroundTaskHandle` for the registry
    ///   to cancel.
    /// - Otherwise the legacy [`create`](Self::create) /
    ///   [`RuntimeSpawner`]-cancel path runs: cancel any recorded
    ///   [`BackgroundTaskHandle`] and mark the Bash/Agent variant `Killed`.
    ///
    /// Note: only Bash and Agent states currently carry a writable `status`
    /// field in the M1 surface; other variants are no-ops on cancel.
    pub async fn kill(&self, task_id: &str) -> Result<(), TaskError> {
        let canonical = self.canonical_or_raw(task_id).await;
        self.invalidate_handback_for_task(&canonical).await;
        // A source handoff and explicit shell teardown are exclusive ownership
        // decisions. Native completion may still win under the task-row lock.
        let is_shell = self
            .tasks
            .read()
            .await
            .get(&canonical)
            .is_some_and(|state| matches!(state, TaskState::LocalBash(_)));
        let _transfer_mutation = if is_shell {
            Some(self.shell_transfer_mutation.lock().await)
        } else {
            None
        };
        if is_shell && !self.tasks.read().await.contains_key(&canonical) {
            return Err(TaskError::NotFound(canonical));
        }
        let _stop_guard = if self
            .tasks
            .read()
            .await
            .get(&canonical)
            .is_some_and(|state| matches!(state, TaskState::LocalBash(_) | TaskState::Monitor(_)))
        {
            *self
                .stopping_shells
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entry(canonical.clone())
                .or_default() += 1;
            Some(ShellStopGuard {
                stopping: self.stopping_shells.clone(),
                task_id: canonical.clone(),
                revision: self.notification_revision.clone(),
            })
        } else {
            None
        };
        let stopped_agent = self.get(&canonical).await.and_then(|state| match state {
            TaskState::LocalAgent(agent) => Some(agent.agent_id),
            _ => None,
        });
        self.kill_backing_task(&canonical).await?;
        if let Some(agent_id) = stopped_agent {
            // nHn is runner-exit cleanup, including background/persistent exits.
            // Box the recursive shell-stop route; children here are shell tasks,
            // so this cannot recursively sweep another agent tree.
            Box::pin(self.kill_background_shells_for_agent(agent_id)).await;
        }
        let owner = self
            .teammate_departure_cleanup
            .read()
            .await
            .as_ref()
            .and_then(std::sync::Weak::upgrade);
        if let Some(owner) = owner {
            if owner.has_pending_departure(&canonical).await {
                owner
                    .complete_departure(&canonical)
                    .await
                    .map_err(TaskError::Internal)?;
            }
        }
        Ok(())
    }

    async fn kill_backing_task(&self, task_id: &str) -> Result<(), TaskError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let task_id_ref = task_id.as_str();
        // Read liveness BEFORE any kill dispatch. A handler's status sink flips
        // the row to `Killed` INSIDE `handler.kill(..)`, so by the time
        // `mark_killed` runs it can no longer tell "this kill is what ended it"
        // (stamp: the stop is redundant with the tool result) from "it had
        // already settled on its own" (do NOT stamp: a shell that hit its own
        // timeout also settles as `Killed`, `handlers/local_bash.rs:422`, and
        // still owes the model the notification it has not drained yet).
        let was_live = !self
            .tasks
            .read()
            .await
            .get(task_id_ref)
            .is_some_and(TaskState::is_terminated);
        if self
            .external_teammate_tasks
            .read()
            .await
            .contains(task_id_ref)
        {
            let controller = self
                .external_teammate_controller
                .read()
                .await
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .ok_or_else(|| {
                    TaskError::Internal("External teammate teardown owner is unavailable".into())
                })?;
            controller
                .kill(task_id_ref)
                .await
                .map_err(|error| TaskError::Internal(error.to_string()))?;
            self.unregister_external_teammate_task(task_id_ref).await;
            self.mark_killed(task_id_ref, was_live).await;
            return Ok(());
        }
        // Preserve the handler's explicit kill path as the primary cancellation
        // mechanism. The cleanup hook is a fallback/drop-owner hook; running it
        // first can remove the handler's worker record before `Task::kill` gets a
        // chance to cancel the live runtime work.
        // Handler-spawned path: dispatch teardown to the owning handler.
        // Read the routing entry without consuming it. A failed handler kill is
        // retryable; removing the route/cleanup before the awaited call would
        // strand a still-running task with no way to stop it.
        // The in-process handler consumes its worker entry before awaiting
        // exit/deallocation. Keep concurrent callers outside that window; the
        // routing entry is re-read only after the first stop has finished.
        // External pane controllers are deliberately outside this lock because
        // their fallback may re-enter the registry.
        let stop_lock =
            if self.spawned.read().await.get(task_id_ref) == Some(&TaskType::InProcessTeammate) {
                Some(
                    self.in_process_stop_locks
                        .lock()
                        .await
                        .entry(task_id_ref.to_owned())
                        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                        .clone(),
                )
            } else {
                None
            };
        let mut stop_guard = match stop_lock {
            Some(lock) => Some(lock.lock_owned().await),
            None => None,
        };
        let spawned_type = self.spawned.read().await.get(task_id_ref).copied();
        if let Some(task_type) = spawned_type {
            let handler = self
                .handlers
                .read()
                .await
                .get(&task_type)
                .ok_or(TaskError::UnknownType)?
                .clone();
            let ctx = TaskContext {
                fs: self.fs.clone(),
                runtime: self.runtime.clone(),
            };
            if task_type == TaskType::InProcessTeammate {
                let guard = stop_guard
                    .take()
                    .expect("in-process stop owns its task lock");
                let handler = handler.clone();
                let task_id = task_id_ref.to_owned();
                // Keep both exit/deallocation and exclusion alive if the caller
                // drops its Stop future after the handler consumes its worker.
                let (result, guard) = tokio::spawn(async move {
                    let result = handler.kill(&task_id, ctx).await;
                    (result, guard)
                })
                .await
                .map_err(|error| TaskError::Internal(error.to_string()))?;
                stop_guard = Some(guard);
                result?;
            } else {
                handler.kill(task_id_ref, ctx).await?;
            }
            // Commit the routing teardown only after the owner confirms the
            // process is stopped. A concurrent terminal transition is harmless:
            // removals are idempotent and the status guard below preserves it.
            self.spawned.write().await.remove(task_id_ref);
            let cleanup = self.cleanups.lock().await.remove(task_id_ref);
            // Reflect the kill in the tracked state for any variant the M1
            // surface can write; the handler's status sink drives the rest.
            // Reverse-race guard: a task that already reached a terminal status
            // (e.g. an MCP settle that won the race and fired `TaskCompleted`)
            // must NOT be demoted to `Killed` — that would leave the task
            // `Killed` after `TaskCompleted` already fired. Skip the status
            // flip once terminal (mirrors `settle_mcp_task`'s re-check).
            self.mark_killed(task_id_ref, was_live).await;
            if let Some(cleanup) = cleanup {
                cleanup();
            }
            drop(stop_guard);
            return Ok(());
        }

        let cleanup = self.cleanups.lock().await.remove(task_id_ref);
        let mut handles = self.handles.lock().await;
        if let Some(h) = handles.remove(task_id_ref) {
            self.runtime
                .cancel(&h)
                .await
                .map_err(|e| TaskError::Internal(e.to_string()))?;
        }
        self.mark_killed(task_id_ref, was_live).await;
        if let Some(cleanup) = cleanup {
            cleanup();
        }
        Ok(())
    }

    /// Flip a task to `Killed`, but only when THIS call performs the
    /// non-terminal transition. Returns the output file of a `local_bash` task
    /// that was actually killed here, so the caller can close its file.
    ///
    /// Reverse-race guard: a task that already reached a terminal status (e.g.
    /// an MCP settle that won the race and fired `TaskCompleted`) must NOT be
    /// demoted to `Killed`.
    /// `was_live` is the row's liveness as of BEFORE the kill dispatch — see
    /// [`Self::kill_backing_task`]. It is what separates a handler that flipped
    /// the row terminal during THIS kill from a task that had already settled.
    async fn mark_killed(&self, task_id: &str, was_live: bool) {
        let mut map = self.tasks.write().await;
        let Some(state) = map.get_mut(task_id) else {
            return;
        };
        // claude-code stamps `notified:!0` in EVERY per-type kill handler
        // (`JF` for local_bash, `THn` for monitors, and the remote-agent /
        // teammate / dream handlers) with ONE exception: `local_agent`, whose
        // kill uses `notified: D.notified || Yf(D)` and therefore lets its stop
        // notification through. [`stamp_kill_notified`] is that policy, in one
        // place so it cannot drift per variant.
        //
        // The stamp is what suppresses the redundant second telling. A model
        // that called TaskStop already has `Successfully stopped task: X` in
        // hand; without this it ALSO got a `<task-notification>` saying the
        // same command "was stopped", because this engine derives notifications
        // from `terminal && !notified`.
        if state.is_terminated() {
            // The status flip is skipped (the reverse-race guard above), but the
            // STAMP still has to land here: `kill_backing_task` awaits
            // `handler.kill(..)` BEFORE calling this, and every handler with a
            // bound `RegistryStatusSink` (Monitor, LocalBash, the desktop's
            // InProcessTeammate, Dream, LocalWorkflow, LocalFusion) flips the row
            // to `Killed` inside that call. Without this the stamp below is dead
            // code for every handler-spawned type and they keep double-telling.
            //
            // Only a row THIS kill ended is stamped. One that had already
            // settled on its own — a completion, a failure, or a shell that hit
            // its own timeout (also `Killed`) — still owes the model the
            // notification it has not drained yet.
            let stopped_event = if was_live && state.base().status == TaskStatus::Killed {
                let event = shell_stopped_sdk_event(state);
                stamp_terminal_clock(state, false);
                stamp_kill_notified(state);
                event
            } else {
                None
            };
            let stopped_note = stopped_event
                .as_ref()
                .and_then(|_| monitor_stopped_note(state));
            let output = stopped_event
                .as_ref()
                .filter(|_| state.base().task_type == TaskType::LocalBash)
                .map(|_| state.base().output_file.clone());
            if let Some(note) = stopped_note {
                self.pending_monitor_events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push_back(note);
            }
            drop(map);
            self.append_killed_trailer(output.clone()).await;
            if let Some(output) = output {
                self.output_manager.evict_writer(&output).await;
            }
            if let Some(event) = stopped_event {
                self.tasks.emit(event);
            }
            return;
        }
        let mut killed_bash_output = None;
        state.base_mut().status = TaskStatus::Killed;
        state.base_mut().end_time = Some(SystemTime::now());
        if let TaskState::LocalAgent(agent) = state {
            agent.is_parked = false;
        }
        stamp_terminal_clock(state, false);
        self.bump_notification_revision();
        let stopped_event = shell_stopped_sdk_event(state);
        let stopped_note = stopped_event
            .as_ref()
            .and_then(|_| monitor_stopped_note(state));
        stamp_kill_notified(state);
        match state {
            TaskState::LocalBash(bash) => {
                killed_bash_output = Some(bash.base.output_file.clone());
            }
            // A backgrounded MCP call: mark killed + `mcpStatus:"cancelled"`
            // (the poll loop's `status==="killed"` → `cancelTask` branch).
            TaskState::McpTask(mcp) => mcp.mcp_status = "cancelled".to_string(),
            _ => {}
        }
        if let Some(note) = stopped_note {
            self.pending_monitor_events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push_back(note);
        }
        drop(map);
        self.append_killed_trailer(killed_bash_output.clone()).await;
        if let Some(output) = killed_bash_output {
            self.output_manager.evict_writer(&output).await;
        }
        if let Some(event) = stopped_event {
            self.tasks.emit(event);
        }
    }

    /// Close a killed shell's output file with the trailer claude-code appends
    /// (`JF`: `s1e(e, "\n[killed]\n")`, 2.1.263 `src_160988549.js` @2039181),
    /// so a later `Read` of that file shows how the command ended.
    async fn append_killed_trailer(&self, output_file: Option<std::path::PathBuf>) {
        if let Some(output_file) = output_file {
            let _ = self
                .output_manager
                .append_shell_terminal(&output_file, "\n[killed]\n")
                .await;
        }
    }
}

/// THn's non-quiet WebSocket stop is a housekeeping monitor event, not the
/// shell completion reminder. Command monitors use JF and never take this arm.
fn monitor_stopped_note(
    state: &TaskState,
) -> Option<lingxi_core::host::task_registry::TaskNotification> {
    if !matches!(state, TaskState::Monitor(_)) || state.base().task_type != TaskType::Monitor {
        return None;
    }
    let base = state.base();
    Some(lingxi_core::host::task_registry::TaskNotification {
        task_id: base.id.clone(),
        recipient_agent_id: base.creator_agent_id,
        task_type: "monitor_ws".into(),
        status: "running".into(),
        description: base.description.clone(),
        result: Some("[Monitor stopped]".into()),
        monitor_housekeeping: true,
        ..Default::default()
    })
}

/// `JF` calls `pi`, not `ha`: this is an SDK system event, never a model
/// message (2.1.263 src_158095655.js, `pi` → `ju`). Claim before stamping
/// notified, so two stop calls cannot publish the same terminal event twice.
fn shell_stopped_sdk_event(state: &TaskState) -> Option<serde_json::Value> {
    if state.base().notified || !matches!(state, TaskState::LocalBash(_) | TaskState::Monitor(_)) {
        return None;
    }
    let base = state.base();
    let output_file = if base.task_type == TaskType::LocalBash {
        base.output_file.to_string_lossy().into_owned()
    } else {
        String::new()
    };
    let mut event = serde_json::json!({
        "type": "system", "subtype": "task_notification", "task_id": base.id,
        "status": "stopped", "output_file": output_file, "summary": base.description,
    });
    if let Some(tool_use_id) = &base.tool_use_id {
        event["tool_use_id"] = serde_json::json!(tool_use_id);
    }
    Some(event)
}

/// The `notified` half of a kill — claude-code's `notified:!0`, stamped by every
/// per-type kill handler with ONE exception.
///
/// `local_agent` is that exception: its kill uses `notified: D.notified || Yf(D)`,
/// so `Agent "X" was stopped by Claude` still reaches the model. That is the one
/// stop the model is NOT otherwise told about, which is why this is a policy
/// function rather than an unconditional write in [`TaskState::base_mut`].
fn stamp_kill_notified(state: &mut TaskState) {
    if !matches!(state, TaskState::LocalAgent(_)) {
        state.base_mut().notified = true;
    }
}

impl TaskRegistry {
    /// claude-code `Dlo`'s eviction half (`src_160988549.js` @2029385) — drop
    /// the terminal rows the model has already been told about.
    ///
    /// Without this the registry grows for the life of the session: every
    /// finished shell, agent and monitor stays in the map forever, so `/tasks`
    /// accumulates and every drain re-scans them.
    ///
    /// A candidate must be terminal AND notified. Four guards then hold a row
    /// back, transcribed from the oracle in its own order:
    ///
    /// ```js
    /// if("retain"in D&&(D.evictAfter??1/0)>Date.now())continue;
    /// if("retain"in D&&(D.keepaliveReasons?.size??0)>0)continue;
    /// if(D.type==="local_workflow"&&((D.evictAfter??0)>Date.now()||!eh(I)))continue;
    /// if(D.type==="mcp_task"&&(D.endTime??0)+fT>Date.now())continue;
    /// ```
    ///
    /// `"retain" in D` is a `local_agent` type test — `retain` is a field only
    /// that variant carries — so the first two guards are the agent's 30 s
    /// deadline and its live-children hold. Note the two default directions are
    /// OPPOSITE and both are load-bearing: an agent with no deadline (`?? 1/0`)
    /// is kept forever, a workflow with no deadline (`?? 0`) is evicted at once.
    ///
    /// ONE guard has no substrate here: `!eh(I)` keeps a `local_workflow` while
    /// a live `/loop` is keyed on its task id (`eh(e)=!Gb().loopsByTaskId.has(e)`).
    /// This port has no task-id-keyed loop registry — loops are cron-driven —
    /// so there is nothing to consult, and the workflow's own deadline is the
    /// only thing holding it. That is a KNOWN narrowing, not an oversight.
    fn evict_notified_terminal_rows(
        &self,
        map: &mut HashMap<String, TaskState>,
        // Resolved by the caller: the handler table is behind an async lock
        // and this body is synchronous.
        agent_handler: Option<&Arc<dyn Task>>,
    ) -> Vec<std::path::PathBuf> {
        let now = SystemTime::now();
        // `?? 1/0` — an agent with no deadline never expires.
        let agent_deadline_passed = |at: Option<SystemTime>| at.is_some_and(|at| at <= now);
        // `?? 0` — a workflow with no deadline is already expired.
        let workflow_deadline_passed = |at: Option<SystemTime>| at.is_none_or(|at| at <= now);
        let snapshot = &*map;
        let doomed: Vec<String> = snapshot
            .iter()
            .filter(|(_, state)| {
                let base = state.base();
                if !base.status.is_terminal()
                    || !base.notified
                    || self.has_human_messages(&base.id)
                    || self
                        .shell_transfer_fences
                        .lock()
                        .unwrap()
                        .contains(&base.id)
                {
                    return false;
                }
                match state {
                    TaskState::LocalAgent(agent) => {
                        !agent.is_parked
                            && agent_deadline_passed(base.evict_after)
                            && !self.has_live_background_children_locked(
                                snapshot,
                                Some(agent.agent_id),
                                None,
                                None,
                            )
                    }
                    TaskState::LocalWorkflow(_) => workflow_deadline_passed(base.evict_after),
                    // The `mcp_task` guard is the only one keyed on `endTime`
                    // rather than `evictAfter`; same 30 s, different clock.
                    TaskState::McpTask(_) => {
                        base.end_time.is_none_or(|end| end + EVICT_AFTER <= now)
                    }
                    // A Fusion run reaches a terminal STATUS before its
                    // `<fusion-result>` append resolves — `finish_fusion_terminal`
                    // flips the row, then the handler awaits the completion
                    // sink. Print mode's waiter is built on that gap
                    // (`apps/cli/src/run.rs::fusion_result_ready` polls while
                    // the receipt is `Pending`), so evicting here would delete
                    // the row it is watching and turn a successful run into a
                    // runtime error. Every other receipt state — published,
                    // durably queued, or a definite failure — is settled and
                    // has nothing left to protect.
                    TaskState::LocalFusion(fusion) => fusion.publication_status.is_terminal(),
                    _ => true,
                }
            })
            .map(|(id, _)| id.clone())
            .collect();
        doomed
            .into_iter()
            .filter_map(|id| {
                if let Some(handler) = agent_handler {
                    handler.forget_resume_recipe(&id);
                }
                self.human_messages.lock().unwrap().remove(&id);
                map.remove(&id)
                    .map(|state| state.base().output_file.clone())
            })
            .collect()
    }
}

/// claude-code `fT = 30000` — how long a terminal row lingers before the
/// attachment pass may evict it (`src_160988549.js` @2024365).
const EVICT_AFTER: Duration = Duration::from_secs(30);

/// The two clocks claude-code stamps on EVERY terminal transition, in one place
/// so no variant can be added without them.
///
/// * `endTime: Date.now()` — the oracle writes it on the killed
///   (@2047553), completed (@2050039) and failed (@2050942) transitions alike.
///   The port declared `end_time` and never wrote it outside the `mcp_task`
///   settle, so `elapsed` on an `mcp_task` and the 30 s `mcp_task` eviction
///   guard both read a `None` that could only mean "still running".
/// * `evictAfter` — claude-code `ret(e,t)`:
///
///   ```js
///   function ret(e,t){if(e.retain)return;
///     if(t.park&&(e.keepaliveReasons?.size??0)>0)return;
///     return Date.now()+fT}
///   ```
///
///   `retain` is a `local_agent` field, so the guard only ever applies to that
///   type; the park-with-keepalive arm is the resting agent that still owns live
///   children, which must not be evicted out from under them. The port has no
///   `retain` (nothing sets it) and spells "has keepalive reasons" as
///   [`TaskRegistry::has_live_background_children_locked`], which the caller
///   evaluates — so this function takes the answer rather than computing it.
fn stamp_terminal_clock(state: &mut TaskState, holds_live_children: bool) {
    let now = SystemTime::now();
    let base = state.base_mut();
    if base.end_time.is_none() {
        base.end_time = Some(now);
    }
    // Only a `local_agent` carries the oracle's `retain`/`keepaliveReasons`
    // pair; every other type is evicted on the pass after it is notified, with
    // no per-row deadline at all.
    let deferred = matches!(state, TaskState::LocalAgent(_)) && holds_live_children;
    if !deferred && state.base().evict_after.is_none() {
        state.base_mut().evict_after = Some(now + EVICT_AFTER);
    }
}

/// `TeamSpawnSeam` impl — the typed spawn/kill seam the coordinator's
/// implicit teammate service use to start and stop a real
/// `InProcessTeammate` task WITHOUT a `coordinator` → `lingxi-tasks` dependency
/// cycle (the abstract trait lives in `traits`; this concrete impl lives here).
///
/// Unlike [`TaskRegistryHandle::create`](lingxi_core::host::task_registry::TaskRegistryHandle::create)
/// — which builds a nil-`AgentId` / empty-name placeholder — `spawn_teammate`
/// threads the worker `agent_id` + `name` straight through
/// [`TaskRegistry::spawn`], so the started teammate is keyed on the real worker
/// identity, and returns the **handler-generated** `task_id` the coordinator
/// reconciles back onto its `WorkerAgent`.
#[async_trait]
impl TeamSpawnSeam for TaskRegistry {
    async fn send_peer(
        &self,
        task_id: &str,
        envelope: lingxi_core::host::handback::HandbackEnvelope,
    ) -> Result<(), TeamSpawnError> {
        let task_id = self.canonical_or_raw(task_id).await;
        self.admit_peer_to_task(&task_id, envelope)
            .await
            .map_err(task_err_to_team_spawn_err)
    }
    async fn spawn_teammate(
        &self,
        agent_id: lingxi_core::types::AgentId,
        name: String,
        team_name: String,
        description: String,
    ) -> Result<String, TeamSpawnError> {
        self.spawn(
            TaskType::InProcessTeammate,
            TaskSpawnInput::InProcessTeammate {
                spawn_request: None,
                inheritance: None,
                agent_id,
                name,
                team_name,
                // The Agent prompt IS the teammate's initial task
                // (seeded as its first user message); also reused as the task
                // subject below.
                description: description.clone(),
            },
            description,
        )
        .await
        .map_err(task_err_to_team_spawn_err)
    }

    async fn spawn_teammate_request(
        &self,
        agent_id: lingxi_core::types::AgentId,
        name: String,
        team_name: String,
        request: lingxi_core::host::subagent_spawn::SubagentSpawnRequest,
        inherit: lingxi_core::host::subagent_spawn::SubagentInheritance,
    ) -> Result<String, TeamSpawnError> {
        let description = request.description.clone().unwrap_or_default();
        self.spawn(
            TaskType::InProcessTeammate,
            TaskSpawnInput::InProcessTeammate {
                agent_id,
                name,
                team_name,
                description: request.prompt.clone(),
                spawn_request: Some(request),
                inheritance: Some(inherit),
            },
            description,
        )
        .await
        .map_err(|error| {
            tracing::warn!(%error, "in-process teammate startup failed");
            TeamSpawnError::Internal("Failed to spawn in-process teammate".into())
        })
    }

    async fn resolved_model_selection(
        &self,
        task_id: &str,
    ) -> Result<lingxi_core::host::team_spawn::TeammateModelSelection, TeamSpawnError> {
        match self.get(task_id).await {
            Some(TaskState::InProcessTeammate(teammate)) => {
                let model = teammate.child_model.filter(|model| !model.is_empty()).ok_or_else(|| {
                    TeamSpawnError::Internal("teammate model route was not resolved at admission".into())
                })?;
                Ok(lingxi_core::host::team_spawn::TeammateModelSelection {
                    model,
                    model_profile: teammate.child_model_profile,
                })
            }
            Some(_) => Err(TeamSpawnError::Unsupported("task is not an in-process teammate".into())),
            None => Err(TeamSpawnError::NotFound(task_id.into())),
        }
    }

    async fn apply_plan_approval(
        &self,
        task_id: &str,
        response: lingxi_core::host::teammate_plan::PlanApprovalResponse,
    ) -> Result<(), TeamSpawnError> {
        let task_id = self.canonical_or_raw(task_id).await;
        let kind = self
            .spawned
            .read()
            .await
            .get(&task_id)
            .copied()
            .ok_or(TeamSpawnError::Terminated)?;
        let handler = self
            .handler_for(kind)
            .await
            .ok_or_else(|| TeamSpawnError::Unsupported(format!("{kind:?}")))?;
        handler
            .apply_plan_approval(
                &task_id,
                response,
                TaskContext {
                    fs: self.fs.clone(),
                    runtime: self.runtime.clone(),
                },
            )
            .await
            .map_err(task_err_to_team_spawn_err)
    }

    async fn kill(&self, task_id: &str) -> Result<(), TeamSpawnError> {
        // Delegate to the inherent `TaskRegistry::kill`, which routes teardown
        // back to the owning handler for handler-spawned (teammate) tasks.
        TaskRegistry::kill(self, task_id)
            .await
            .map_err(task_err_to_team_spawn_err)
    }

    /// Inject a message into a running teammate's turn loop. This is the
    /// production override of the defaulted seam method — the load-bearing
    /// bridge that lets a coordinator `SendMessage` reach a teammate's runner.
    ///
    /// Resolves the task's owning handler the SAME way [`kill`](Self::kill)
    /// does — via the handler-spawned-id index (`spawned`), so it routes to the
    /// exact handler that started the worker — then dispatches to
    /// [`Task::send_message`] when the handler
    /// [`supports_messages`](Task::supports_messages). For the
    /// `InProcessTeammate` handler that lands the text on the running agent's
    /// persist-mode `recv()` (the `injectUserMessageToTeammate` analogue).
    ///
    /// Error mapping (so the mailbox→runner pump can act): a task whose handler
    /// reports `TerminatedTask` (its runner dropped the receiver) OR an
    /// unknown/gone task (`NotFound`) maps to [`TeamSpawnError::Terminated`] —
    /// the pump's "stop" signal. A handler that does not support messages maps
    /// to [`TeamSpawnError::Unsupported`]; anything else is `Internal`.
    async fn send_message(&self, task_id: &str, message: String) -> Result<(), TeamSpawnError> {
        // Addressing parity with the other task surfaces: an agent is
        // registered under its task id AND its aliases (the agent id, its name,
        // and `name@team`), and `get` / `kill` / `output` all resolve those
        // aliases first. This one did not, so an id the model was handed — the
        // `<task-id>` of a completion notification, or a name from `TaskList` —
        // resolved for `TaskStop` and `TaskOutput` but came back `Terminated`
        // here, which reads to the model as "that agent is gone".
        let task_id = &self.canonical_or_raw(task_id).await;
        // AGT-07. BEFORE the `spawned` lookup, and the order is load-bearing:
        // `kill` REMOVES the spawned entry, so a user-stopped agent no longer
        // has one — a gate placed after the lookup would compile, pass a test
        // that stubs the registry, and be unreachable in production for exactly
        // the case it exists to catch.
        //
        // A user stop means the work was deliberately cancelled. Falling through
        // to `Terminated` here told the model nothing at all (the pump logs that
        // at debug and stops), so it could relaunch what the user had just
        // stopped.
        if matches!(
            self.tasks.read().await.get(task_id),
            Some(TaskState::LocalAgent(agent))
                if agent.outcome.killed_by.as_deref() == Some("user")
        ) {
            return Err(TeamSpawnError::StoppedByUser(
                lingxi_core::host::task_registry::stopped_by_user_message(task_id),
            ));
        }
        let external = self
            .agent_message_receivers
            .lock()
            .await
            .get(task_id)
            .cloned();
        if let Some(receiver) = external {
            return receiver
                .send(message)
                .await
                .map_err(|error| TeamSpawnError::Internal(error.to_string()));
        }
        // Resolve the owning handler via the spawned-id index (mirrors `kill`'s
        // dispatch). An unknown id ⇒ the teammate is gone ⇒ `Terminated`.
        let task_type = self
            .spawned
            .read()
            .await
            .get(task_id)
            .copied()
            .ok_or(TeamSpawnError::Terminated)?;
        let handler = self
            .handlers
            .read()
            .await
            .get(&task_type)
            .ok_or_else(|| TeamSpawnError::Unsupported(format!("{task_type:?}")))?
            .clone();
        if !handler.supports_messages() {
            return Err(TeamSpawnError::Unsupported(format!("{task_type:?}")));
        }
        let ctx = TaskContext {
            fs: self.fs.clone(),
            runtime: self.runtime.clone(),
        };
        handler
            .send_message(task_id, message, ctx)
            .await
            .map_err(|e| match e {
                // The runner dropped its receiver / the task is gone ⇒ the pump
                // must stop: both collapse onto the seam's `Terminated`.
                TaskError::TerminatedTask | TaskError::NotFound(_) => TeamSpawnError::Terminated,
                TaskError::Unsupported => TeamSpawnError::Unsupported(format!("{task_type:?}")),
                other => TeamSpawnError::Internal(other.to_string()),
            })
    }

    /// A teammate task is ALIVE while its stored state is present and
    /// non-terminal. A terminal status (Completed / Failed / Killed) reads as
    /// "gone" even though the retained task remains inspectable — the
    /// mailbox→runner pump uses this on its park timeout to stop pumping a dead
    /// teammate (so its mailbox can be unregistered) even if no message ever
    /// arrived to surface `Terminated`. A completed persistent park still reads
    /// as alive because the runner can accept another message.
    async fn is_alive(&self, task_id: &str) -> bool {
        match self.get(task_id).await {
            Some(state) => !state.is_terminated(),
            None => false,
        }
    }
}

/// Map a [`TaskError`] from the inherent spawn/kill path onto the narrow
/// [`TeamSpawnError`] seam vocabulary: a missing handler is `Unsupported`, an
/// unknown id is `NotFound`, everything else is `Internal`.
fn task_err_to_team_spawn_err(e: TaskError) -> TeamSpawnError {
    match e {
        TaskError::UnknownType => TeamSpawnError::Unsupported("in_process_teammate".to_string()),
        TaskError::NotFound(id) => TeamSpawnError::NotFound(id),
        other => TeamSpawnError::Internal(other.to_string()),
    }
}

/// Build the typed [`TaskState`] for a [`spawn`](TaskRegistry::spawn) using the
/// REAL `input` fields (the spawn path, unlike `create`'s placeholder path,
/// has the agent ids / commands the variant carries).
fn state_for_spawn(mut base: TaskStateBase, input: &TaskSpawnInput) -> TaskState {
    // Stamp the originating `tool_use_id` onto the task so a background task's
    // `<task-notification>` carries the `<tool-use-id>` line (claude-code parity).
    match input {
        TaskSpawnInput::MonitorWs(source) => {
            let reg = &source.registration.task;
            base.tool_use_id.clone_from(&reg.tool_use_id);
            base.creator_teammate_name
                .clone_from(&reg.creator_teammate_name);
            base.creator_team_name.clone_from(&reg.creator_team_name);
            base.creator_agent_id = reg.creator_agent_id;
        }
        TaskSpawnInput::LocalAgent {
            tool_use_id,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            ..
        } => {
            if base.tool_use_id.is_none() {
                base.tool_use_id.clone_from(tool_use_id);
            }
            if base.creator_teammate_name.is_none() {
                base.creator_teammate_name.clone_from(creator_teammate_name);
            }
            if base.creator_team_name.is_none() {
                base.creator_team_name.clone_from(creator_team_name);
            }
            if base.creator_agent_id.is_none() {
                base.creator_agent_id = *creator_agent_id;
            }
        }
        TaskSpawnInput::LocalWorkflow {
            tool_use_id,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            ..
        }
        | TaskSpawnInput::Monitor {
            tool_use_id,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            ..
        }
        | TaskSpawnInput::McpTask {
            tool_use_id,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            ..
        } => {
            if base.tool_use_id.is_none() {
                base.tool_use_id.clone_from(tool_use_id);
            }
            if base.creator_teammate_name.is_none() {
                base.creator_teammate_name.clone_from(creator_teammate_name);
            }
            if base.creator_team_name.is_none() {
                base.creator_team_name.clone_from(creator_team_name);
            }
            if base.creator_agent_id.is_none() {
                base.creator_agent_id = *creator_agent_id;
            }
        }
        TaskSpawnInput::LocalBash { tool_use_id, .. } => {
            if base.tool_use_id.is_none() {
                base.tool_use_id.clone_from(tool_use_id);
            }
        }
        TaskSpawnInput::InProcessTeammate { spawn_request, .. } => {
            if base.creator_agent_id.is_none() {
                base.creator_agent_id = spawn_request
                    .as_ref()
                    .and_then(|request| request.creator_agent_id);
            }
        }
        _ => {}
    }
    match input {
        TaskSpawnInput::LocalBash { command, .. } => {
            TaskState::LocalBash(crate::state::LocalBashTaskState {
                is_adopted: false,
                caller: None,
                base,
                command: command.clone(),
                pid: None,
                exit_code: None,
                stop_cause: None,
                cwd: None,
                is_backgrounded: None,
            })
        }
        TaskSpawnInput::LocalAgent {
            agent_id,
            subagent_type,
            prompt,
            is_backgrounded,
            spawn_request,
            ..
        } => TaskState::LocalAgent(crate::state::LocalAgentTaskState {
            handback: None,
            handback_history: Vec::new(),
            agent_spawn_provenance: spawn_request
                .as_ref()
                .map(|request| request.agent_spawn_provenance.clone())
                .unwrap_or_default(),
            agent_list_lifecycle: lingxi_core::host::task_registry::AgentListLocalLifecycleFacts::initialized_local_agent(),
            spawned_description: spawn_request
                .as_ref()
                .and_then(|request| request.description.clone()),
            is_parked: false,
            is_observer: spawn_request.as_ref().is_some_and(|r| {
                r.query_source_label.as_deref()
                    == Some(lingxi_core::host::subagent_spawn::OBSERVER_QUERY_SOURCE)
            }),
            observed_agent_id: if spawn_request.as_ref().is_some_and(|r| {
                r.query_source_label.as_deref()
                    == Some(lingxi_core::host::subagent_spawn::OBSERVER_QUERY_SOURCE)
            }) {
                base.creator_agent_id.take()
            } else {
                None
            },
            // The fork identity rides the full spawn request, not the compact
            // task-index fields — a forked skill is dispatched through the same
            // `SubagentSpawnRequest` as any background agent.
            forked_skill_name: spawn_request
                .as_ref()
                .and_then(|r| r.forked_skill_name.clone()),
            base,
            agent_id: *agent_id,
            subagent_type: subagent_type.clone(),
            prompt: prompt.clone(),
            error: None,
            messages: vec![],
            pending_messages: vec![],
            is_backgrounded: *is_backgrounded,
            outcome: Default::default(),
        }),
        TaskSpawnInput::RemoteAgent { endpoint, .. } => {
            TaskState::RemoteAgent(crate::state::RemoteAgentTaskState {
                base,
                remote_session_id: String::new(),
                remote_endpoint: endpoint.clone(),
            })
        }
        TaskSpawnInput::InProcessTeammate {
            agent_id,
            name,
            team_name,
            spawn_request,
            ..
        } => {
            base.creator_teammate_name = Some(name.clone());
            base.creator_team_name = Some(team_name.clone());
            TaskState::InProcessTeammate(crate::state::InProcessTeammateTaskState {
                child_model: None,
                child_model_profile: None,
                handback: None,
                handback_history: Vec::new(),
                agent_spawn_provenance: spawn_request
                    .as_ref()
                    .map(|request| request.agent_spawn_provenance.clone())
                    .unwrap_or_default(),
                spawned_agent_type: spawn_request
                    .as_ref()
                    .map(|request| request.subagent_type.clone()),
                spawned_description: spawn_request
                    .as_ref()
                    .and_then(|request| request.description.clone()),
                is_idle: false,
                awaiting_plan_approval: false,
                base,
                agent_id: *agent_id,
                pending_messages: vec![],
            })
        }
        TaskSpawnInput::LocalWorkflow {
            session_uuid,
            workflow_id,
            script,
            resume_from_run_id,
            args,
            run_id,
            invocation_mode: _,
            workflow_source: _,
            script_is_verbatim_builtin: _,
            transcript_subdir,
            launched_from_subagent: _,
            tool_use_id: _,
            creator_teammate_name: _,
            creator_team_name: _,
            creator_agent_id: _,
            scope,
        } => TaskState::LocalWorkflow(crate::state::LocalWorkflowTaskState {
            base,
            session_uuid: session_uuid.clone(),
            workflow_id: workflow_id.clone(),
            script: script.clone(),
            resume_from_run_id: resume_from_run_id.clone(),
            args: args.clone(),
            // Effective run id: the launcher-minted id for a fresh run, else the
            // resumed id (so the resume gate can find a still-running workflow).
            run_id: run_id.clone().or_else(|| resume_from_run_id.clone()),
            script_path: None,
            transcript_dir: transcript_subdir.clone(),
            current_step: 0,
            outcome: Default::default(),
            // The Host's minted authority, carried through verbatim. There is
            // deliberately no derivation here from `workflow_id` or `args`:
            // both are caller-supplied, and reading either would re-open the
            // forgery vector `find_nonterminal_local_app_workflows`' doc
            // comment describes. See `LocalWorkflowTaskState::scope`.
            scope: scope.clone(),
        }),
        TaskSpawnInput::MonitorMcp { server_name, watch } => {
            TaskState::MonitorMcp(crate::state::MonitorMcpTaskState {
                base,
                server_name: server_name.clone(),
                watch_resources: watch.clone(),
            })
        }
        TaskSpawnInput::MonitorWs(source) => TaskState::Monitor(crate::state::MonitorTaskState {
            base,
            command: source.registration.url.clone(),
            exit_code: None,
            stdout_bytes: None,
        }),
        TaskSpawnInput::Monitor {
            command,
            spawn_command: _,
            timeout: _,
            cwd: _,
            tool_use_id,
            creator_teammate_name: _,
            creator_team_name: _,
            creator_agent_id: _,
        } => {
            if base.tool_use_id.is_none() {
                base.tool_use_id = tool_use_id.clone();
            }
            // Command monitoring is a shell task on the public wire. The
            // spawned-handler map separately retains Monitor for cancellation.
            base.task_type = TaskType::LocalBash;
            TaskState::Monitor(crate::state::MonitorTaskState {
                base,
                command: command.clone(),
                exit_code: None,
                stdout_bytes: None,
            })
        }
        TaskSpawnInput::McpTask {
            server_name,
            tool_name,
            tool_use_id,
            creator_teammate_name: _,
            creator_team_name: _,
            creator_agent_id: _,
        } => {
            if base.tool_use_id.is_none() {
                base.tool_use_id = tool_use_id.clone();
            }
            TaskState::McpTask(crate::state::McpTaskState {
                saved_hint: None,
                base,
                server_name: server_name.clone(),
                tool_name: tool_name.clone(),
                // `NZu` seeds `mcpStatus:"working"`.
                mcp_status: "working".to_string(),
                status_message: None,
                // A running call has produced nothing yet.
                result_text: None,
            })
        }
        TaskSpawnInput::AutoModeScan => {
            TaskState::AutoModeScan(crate::state::AutoModeScanTaskState { base })
        }
        TaskSpawnInput::Dream { max_iterations, .. } => {
            TaskState::Dream(crate::state::DreamTaskState {
                base,
                iteration_count: 0,
                max_iterations: *max_iterations,
            })
        }
        TaskSpawnInput::LocalFusion {
            request,
            conversation_id,
        } => TaskState::LocalFusion(crate::state::LocalFusionTaskState {
            base,
            conversation_id: conversation_id.clone(),
            prompt: request.prompt.clone(),
            run_id: None,
            preset: match request.preset {
                lingxi_core::host::FusionPreset::Quality => "quality".to_string(),
                lingxi_core::host::FusionPreset::Fast => "fast".to_string(),
            },
            cross_provider: request.cross_provider,
            final_text: None,
            error: None,
            egress_profiles: Vec::new(),
            usage: None,
            stage: None,
            effective_timeout_ms: None,
            planned_panels: None,
            fusion_activation_deadline: None,
            publication_status: lingxi_core::host::FusionPublicationStatus::Pending,
            publication_error: None,
        }),
    }
}

fn aliases_for_spawn(input: &TaskSpawnInput) -> Vec<String> {
    let mut aliases = Vec::new();
    match input {
        TaskSpawnInput::LocalAgent {
            agent_id,
            spawn_request,
            ..
        } => {
            // Agent publishes the bare UUID; typed host callers also address
            // this task through AgentId's prefixed Display form.
            aliases.push(agent_id.as_uuid().to_string());
            aliases.push(agent_id.to_string());
            if let Some(request) = spawn_request {
                if let Some(name) = request.name.as_deref().filter(|s| !s.is_empty()) {
                    aliases.push(name.to_string());
                    if let Some(team) = request.team_name.as_deref().filter(|s| !s.is_empty()) {
                        aliases.push(format!("{name}@{team}"));
                    }
                }
            }
        }
        TaskSpawnInput::InProcessTeammate {
            agent_id,
            name,
            team_name,
            ..
        } => {
            // Agent publishes the bare UUID; typed host callers also address
            // this task through AgentId's prefixed Display form.
            aliases.push(agent_id.as_uuid().to_string());
            aliases.push(agent_id.to_string());
            if !name.is_empty() {
                aliases.push(name.clone());
                if !team_name.is_empty() {
                    aliases.push(format!("{name}@{team_name}"));
                }
            }
        }
        _ => {}
    }
    aliases
}

/// Register the M2 *self-contained* per-type handlers — the ones whose only
/// dependencies are platform traits already available at boot (no agent /
/// subagent pool, mailbox, or budget enforcer). Today that is
/// [`TaskType::LocalBash`], [`TaskType::Monitor`], and
/// [`TaskType::MonitorMcp`].
///
/// `process` + `sandbox` are required because they are absent from
/// [`crate::task_trait::TaskContext`] yet [`LocalBashHandler`] cannot run a
/// command without them. The spool [`TaskOutputManager`] is shared with the
/// registry's own (`reg.output_manager`) so handler-allocated spool paths land
/// in the same sandboxed output dir the registry hands the TUI. `mcp` drives
/// [`MonitorMcpHandler`]'s catalog polls.
///
/// `bash_status_sink` (M8 cc2.1.198 "Task panels: no stuck Running") bridges
/// the bash worker's terminal status + exit code back into the registry.
/// Pre-fix the handler kept its default `NoopStatusSink`, so a finished
/// background bash task's stored `TaskStateBase.status` stayed `Running`
/// forever — the stuck panel. The sink is the DEFERRED
/// [`crate::registry_status_sink::RegistryStatusSink`] pattern: registered
/// here (before the registry `Arc` exists) and bound by the composition root
/// once it does — the same cycle-break the `LocalAgent` handler uses.
///
/// Call this *before* the registry is wrapped in an [`Arc`] — registration
/// takes `&mut self`. The remaining five task types (agent/teammate/workflow/
/// remote/dream) register once their production pools are wired (M9+).
pub fn register_self_contained_handlers(
    reg: &mut TaskRegistry,
    process: Arc<dyn ProcessRunner>,
    sandbox: Arc<dyn Sandbox>,
    mcp: Arc<mcp::McpRegistry>,
    bash_status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
) {
    reg.shell_adoption_runtime = Some((process.clone(), bash_status_sink.clone()));
    let output_manager = reg.output_manager.clone();
    reg.register_handler(
        TaskType::LocalBash,
        Arc::new(
            LocalBashHandler::new(process.clone(), sandbox.clone(), output_manager.clone())
                .with_status_sink(bash_status_sink.clone()),
        ),
    );
    reg.register_handler(
        TaskType::Monitor,
        Arc::new(
            MonitorHandler::new(process, sandbox, output_manager.clone())
                .with_status_sink(bash_status_sink),
        ),
    );
    reg.register_handler(
        TaskType::MonitorMcp,
        Arc::new(MonitorMcpHandler::with_default_interval(
            mcp,
            output_manager,
        )),
    );
}

/// Register the M2 *agent-backed* per-type handlers — the ones whose
/// dependencies are NOT available at platform boot but require the production
/// agent pipeline: [`TaskType::LocalAgent`] (a one-shot subagent driven through
/// a [`SubagentSpawner`]) and [`TaskType::InProcessTeammate`] (a persistent,
/// message-driven subagent driven through a [`StateMachinePool`]).
///
/// These are split out of [`register_self_contained_handlers`] because they need
/// seams the task-registry construction site does not have at boot today — a
/// concrete [`SubagentSpawner`] / [`StateMachinePool`], the parent's
/// [`ToolInvoker`] + [`BudgetEnforcerHandle`], and a [`SubagentApiClient`]. The
/// boot site (`apps/cli/src/init.rs`) constructs the task registry with
/// `subagent_spawner: None` / `budget_enforcer: None` and no teammate pool, so
/// wiring this helper there is a tracked follow-up (M9+). The helper exists now
/// so the wire step can call it once those pools land, and so the handlers are
/// reachable + tested in the interim.
///
/// `tool_invoker` + `budget` are passed through *unchanged* (cloning the `Arc`
/// preserves pointer identity, which the recursion-lock + budget-aggregation
/// invariants rely on). The spool [`TaskOutputManager`] is shared with the
/// registry's own (`reg.output_manager`). `status_sink` is the deferred
/// registry adapter used both for lifecycle writeback and for the LocalAgent
/// activation barrier. The subagent type arrives already resolved on the
/// [`crate::task_trait::TaskSpawnInput::LocalAgent`] variant, so no resolver
/// injection is needed here.
///
/// Call this *before* the registry is wrapped in an [`Arc`] — registration
/// takes `&mut self`.
pub fn register_agent_handlers(
    reg: &mut TaskRegistry,
    spawner: Arc<dyn SubagentSpawner>,
    pool: Arc<StateMachinePool>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    api_client: Arc<dyn SubagentApiClient>,
    status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
) {
    let output_manager = reg.output_manager.clone();
    reg.register_handler(
        TaskType::LocalAgent,
        Arc::new(
            LocalAgentHandler::new(
                spawner,
                tool_invoker.clone(),
                budget,
                output_manager.clone(),
            )
            .with_status_sink(status_sink),
        ),
    );
    reg.register_handler(
        TaskType::InProcessTeammate,
        Arc::new(
            InProcessTeammateHandler::new(pool, output_manager, api_client)
                .with_tool_invoker(tool_invoker),
        ),
    );
}

/// Register the M2 *agent-backed* dream handler — [`TaskType::Dream`] — a
/// one-shot forked subagent that runs the memory-consolidation prompt through a
/// [`SubagentSpawner`].
///
/// Split out for the same reason as [`register_agent_handlers`]: the dream
/// handler needs the production agent pipeline (a concrete [`SubagentSpawner`]
/// plus the parent's [`ToolInvoker`] + [`BudgetEnforcerHandle`]) that the
/// task-registry construction site does not have at boot today. The boot site
/// (`apps/cli/src/init.rs`) wires this once the subagent pool lands (M9+) — the
/// same deferred-wiring note the agent handlers carry. The helper exists now so
/// the wire step can call it once those pools land, and so the handler is
/// reachable + tested in the interim.
///
/// `tool_invoker` + `budget` are passed through *unchanged* (cloning the `Arc`
/// preserves pointer identity, which the recursion-lock + budget-aggregation
/// invariants rely on). The spool [`TaskOutputManager`] is shared with the
/// registry's own (`reg.output_manager`).
///
/// Call this *before* the registry is wrapped in an [`Arc`] — registration
/// takes `&mut self`.
pub fn register_dream_handler(
    reg: &mut TaskRegistry,
    spawner: Arc<dyn SubagentSpawner>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
) {
    let output_manager = reg.output_manager.clone();
    reg.register_handler(
        TaskType::Dream,
        Arc::new(
            DreamHandler::new(spawner, tool_invoker, budget, output_manager)
                .with_status_sink(status_sink),
        ),
    );
}

/// Register [`TaskType::LocalFusion`]. Call before wrapping the registry in an
/// [`Arc`].
pub fn register_fusion_handler(
    reg: &mut TaskRegistry,
    executor: Arc<dyn FusionExecutor>,
    sink: Arc<dyn FusionCompletionSink>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
) {
    register_fusion_handler_with_recorder(
        reg,
        executor,
        sink,
        tool_invoker,
        budget,
        status_sink,
        None,
    );
}

/// Register Fusion with the common host-owned terminal recorder. The legacy
/// helper above remains for mobile/tests that intentionally have no durable
/// session capability.
pub fn register_fusion_handler_with_recorder(
    reg: &mut TaskRegistry,
    executor: Arc<dyn FusionExecutor>,
    sink: Arc<dyn FusionCompletionSink>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
    terminal_recorder: Option<Arc<dyn lingxi_core::host::FusionRunRecorder>>,
) {
    register_fusion_handler_with_recorder_factory(
        reg,
        executor,
        sink,
        tool_invoker,
        budget,
        status_sink,
        terminal_recorder,
        None,
    );
}

/// Register Fusion with both a compatibility recorder and a pure per-session
/// factory. The factory wins for production preparation so hot session
/// switches cannot bind a new run to the boot session's coordinator.
pub fn register_fusion_handler_with_recorder_factory(
    reg: &mut TaskRegistry,
    executor: Arc<dyn FusionExecutor>,
    sink: Arc<dyn FusionCompletionSink>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    status_sink: Arc<dyn crate::handlers::TaskStatusSink>,
    terminal_recorder: Option<Arc<dyn lingxi_core::host::FusionRunRecorder>>,
    terminal_recorder_factory: Option<Arc<dyn lingxi_core::host::FusionRunRecorderFactory>>,
) {
    let output_manager = reg.output_manager.clone();
    let handler = LocalFusionHandler::new(executor, sink, tool_invoker, budget, output_manager)
        .with_status_sink(status_sink);
    let handler = if let Some(recorder) = terminal_recorder {
        handler.with_terminal_recorder(recorder)
    } else {
        handler
    };
    let handler = if let Some(factory) = terminal_recorder_factory {
        handler.with_terminal_recorder_factory(factory)
    } else {
        handler
    };
    reg.register_handler(TaskType::LocalFusion, Arc::new(handler));
}

/// Registry-API-level tests for the restart-adoption scope seam (P-1.12).
/// Kept as its own inline module rather than added to `registry_test.rs` so
/// this task's edits stay confined to files it owns; `registry_test.rs`
/// already exercises the unscoped `register_adopted_workflow` path
/// (`adopted_workflow_is_registered_as_paused_and_keeps_resume_metadata`,
/// `register_adopted_workflow_does_not_replace_existing_live_task_with_same_task_id`)
/// and is left untouched.
///
/// These two tests pin the registry API/plumbing in isolation (a caller
/// handing `register_adopted_workflow_with_scope` an already-built scope, or
/// going through the plain unscoped entry point). The two tests named
/// exactly `an_adopted_in_flight_build_still_blocks_its_apps_delete` and
/// `an_adopted_forged_workflow_still_gets_no_scope` live in
/// `engine-mobile`'s `workflow_support::run_id_tests` instead, where they
/// exercise the real `resolve_adopted_local_app_build_scope` re-derivation
/// end to end against real checkpoint/manifest fixtures -- that is where the
/// residual actually lived, so that is where the requested test names carry
/// the load-bearing coverage.
#[cfg(test)]
mod adopted_workflow_scope_test {
    use super::*;
    use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};

    // ---- Minimal in-memory FileSystem, just enough for `output_manager` ----
    struct InMemoryFs {
        files: tokio::sync::Mutex<HashMap<String, String>>,
        fail_writes: std::sync::atomic::AtomicBool,
        write_count: std::sync::atomic::AtomicUsize,
        fail_write_at: std::sync::atomic::AtomicUsize,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: tokio::sync::Mutex::new(HashMap::new()),
                fail_writes: std::sync::atomic::AtomicBool::new(false),
                write_count: std::sync::atomic::AtomicUsize::new(0),
                fail_write_at: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }
    #[async_trait]
    impl FileSystem for InMemoryFs {
        async fn read_file(
            &self,
            path: &str,
            _offset: Option<u64>,
            _limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content,
                truncated: false,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            let write_number = self
                .write_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if self.fail_writes.load(std::sync::atomic::Ordering::SeqCst)
                || self.fail_write_at.load(std::sync::atomic::Ordering::SeqCst) == write_number
            {
                return Err(FsError::Io("injected write failure".into()));
            }
            self.files
                .lock()
                .await
                .insert(path.to_string(), body.to_string());
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError>
        {
            Err(FsError::Io("not supported".into()))
        }
        async fn append_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            self.files
                .lock()
                .await
                .entry(path.to_string())
                .or_default()
                .push_str(body);
            Ok(())
        }
        async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
            Ok(())
        }
        async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, path: &str) -> Result<u64, FsError> {
            let map = self.files.lock().await;
            Ok(map.get(path).map_or(0, |s| s.len() as u64))
        }
        async fn delete_file(&self, path: &str) -> Result<(), FsError> {
            self.files.lock().await.remove(path);
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
            Ok(())
        }
        async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
            Err(FsError::Io("not supported".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), FsError> {
            Ok(())
        }
    }

    fn make_registry() -> (tempfile::TempDir, Arc<InMemoryFs>, TaskRegistry) {
        let dir = tempfile::tempdir().unwrap();
        let in_memory_fs = Arc::new(InMemoryFs::new());
        let fs: Arc<dyn FileSystem> = in_memory_fs.clone();
        let runtime = Arc::new(test_harness::mocks::MockRuntimeSpawner::default());
        let out_mgr = Arc::new(crate::output_manager::TaskOutputManager::new(
            std::path::PathBuf::from(dir.path()),
            fs.clone(),
        ));
        (dir, in_memory_fs, TaskRegistry::new(runtime, fs, out_mgr))
    }

    #[tokio::test]
    async fn foreground_agent_output_is_readable_before_transcript_link() {
        let (_dir, fs, registry) = make_registry();
        let state = registry
            .register_foreground_agent(
                lingxi_core::host::task_registry::ForegroundAgentRegistration {
                    agent_id: lingxi_core::types::AgentId::new(),
                    agent_type: "Plan".into(),
                    description: "plan the change".into(),
                    prompt: "design".into(),
                    tool_use_id: None,
                    creator_agent_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    agent_spawn_provenance: Default::default(),
                },
            )
            .await
            .unwrap();
        assert!(
            fs.files
                .lock()
                .await
                .contains_key(state.base().output_file.to_str().unwrap()),
            "foreground task must allocate its spool before the row is visible"
        );
        let output = lingxi_core::host::task_registry::TaskRegistryHandle::output(
            &registry,
            &state.base().id,
            Some(0),
        )
        .await
        .unwrap();
        assert!(output.content.is_empty());
        assert!(!output.done);
    }

    #[tokio::test]
    async fn completed_agent_dialog_visibility_requires_background_nonobserver_without_children() {
        let (_dir, _fs, registry) = make_registry();
        let owner = lingxi_core::types::AgentId::new();
        let state = registry
            .register_foreground_agent(
                lingxi_core::host::task_registry::ForegroundAgentRegistration {
                    agent_id: owner,
                    agent_type: "general-purpose".into(),
                    description: "ui".into(),
                    prompt: "".into(),
                    tool_use_id: None,
                    creator_agent_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    agent_spawn_provenance: Default::default(),
                },
            )
            .await
            .unwrap();
        let id = state.base().id.clone();
        registry
            .set_agent_display(&id, "real-model".into(), Some("high".into()))
            .await;
        {
            let mut rows = registry.tasks.write().await;
            let TaskState::LocalAgent(agent) = rows.get_mut(&id).unwrap() else {
                unreachable!()
            };
            agent.base.status = TaskStatus::Completed;
            agent.is_parked = true;
        }
        assert!(
            !registry.completed_agent_visible(&id).await,
            "foreground rows stay hidden"
        );
        {
            let mut rows = registry.tasks.write().await;
            let TaskState::LocalAgent(agent) = rows.get_mut(&id).unwrap() else {
                unreachable!()
            };
            agent.is_backgrounded = true;
        }
        assert!(registry.completed_agent_visible(&id).await);
        let record = <TaskRegistry as lingxi_core::host::task_registry::TaskRegistryHandle>::list(
            &registry,
            Default::default(),
        )
        .await
        .unwrap()
        .remove(0);
        assert!(record.completed_agent_visible);
        assert_eq!(record.model.as_deref(), Some("real-model"));
        registry
            .register_background_bash(
                "bchild001".into(),
                "sleep 1".into(),
                "child".into(),
                None,
                None,
                Some(owner),
            )
            .await
            .unwrap();
        assert!(
            !registry.completed_agent_visible(&id).await,
            "live dependency holds hide the completed parent"
        );
        {
            let mut rows = registry.tasks.write().await;
            let TaskState::LocalAgent(agent) = rows.get_mut(&id).unwrap() else {
                unreachable!()
            };
            agent.is_observer = true;
        }
        assert!(!registry.completed_agent_visible(&id).await);
    }

    #[tokio::test]
    async fn foreground_agent_killer_binding_reaches_real_registry_and_stops_runner() {
        struct Killer(Arc<tokio::sync::Notify>);
        #[async_trait::async_trait]
        impl lingxi_core::host::task_registry::TaskKiller for Killer {
            async fn kill(&self) {
                self.0.notify_one();
            }
        }
        let (_dir, _fs, registry) = make_registry();
        let agent_id = lingxi_core::types::AgentId::new();
        let registration = lingxi_core::host::task_registry::ForegroundAgentRegistration {
            agent_id,
            agent_type: "Plan".into(),
            description: "plan the change".into(),
            prompt: "design".into(),
            tool_use_id: None,
            creator_agent_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            agent_spawn_provenance: Default::default(),
        };
        let state = registry
            .register_foreground_agent(registration.clone())
            .await
            .unwrap();
        let id = state.base().id.clone();
        let killed = Arc::new(tokio::sync::Notify::new());
        // Exercise the public adapter used by Agent's before_start observer,
        // including alias resolution; a mock registry hid the shell-only bug.
        lingxi_core::host::task_registry::TaskRegistryHandle::bind_background_killer(
            &registry,
            &agent_id.to_string(),
            Arc::new(Killer(killed.clone())),
        )
        .await
        .unwrap();
        registry.kill(&id).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), killed.notified())
            .await
            .unwrap();
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Killed
        );

        // The binding must not install stale cleanup after terminal publication.
        let terminal = registry
            .register_foreground_agent(
                lingxi_core::host::task_registry::ForegroundAgentRegistration {
                    agent_id: lingxi_core::types::AgentId::new(),
                    ..registration
                },
            )
            .await
            .unwrap();
        let terminal_id = terminal.base().id.clone();
        registry
            .set_status(&terminal_id, TaskStatus::Completed)
            .await
            .unwrap();
        lingxi_core::host::task_registry::TaskRegistryHandle::bind_background_killer(
            &registry,
            &terminal_id,
            Arc::new(Killer(killed)),
        )
        .await
        .unwrap();
        assert!(!registry.cleanups.lock().await.contains_key(&terminal_id));

        // PID binding still rejects agents: OS identity remains shell-specific.
        let result = registry
            .bind_background_bash_process(
                &terminal_id,
                Some(123),
                Arc::new(Killer(Arc::new(tokio::sync::Notify::new()))),
            )
            .await;
        assert!(matches!(result, Err(TaskError::Unsupported)));
    }

    #[tokio::test]
    async fn foreground_agent_controls_register_background_and_preserve_completed_row() {
        struct BackgroundSignal(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl lingxi_core::host::task_registry::TaskBackgrounder for BackgroundSignal {
            async fn background(&self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let (_dir, _fs, registry) = make_registry();
        let agent_id = lingxi_core::types::AgentId::new();
        let registration = lingxi_core::host::task_registry::ForegroundAgentRegistration {
            agent_id,
            agent_type: "general-purpose".into(),
            description: "foreground".into(),
            prompt: "work".into(),
            tool_use_id: Some("agent-tool-use".into()),
            creator_agent_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            agent_spawn_provenance: Default::default(),
        };
        let state = registry
            .register_foreground_agent(registration.clone())
            .await
            .unwrap();
        let id = state.base().id.clone();
        assert_eq!(
            registry.get(&agent_id.to_string()).await.unwrap().base().id,
            id
        );
        let signal = Arc::new(BackgroundSignal(std::sync::atomic::AtomicUsize::new(0)));
        registry
            .bind_background_requester(&id, signal.clone())
            .await
            .unwrap();
        assert!(registry.has_backgroundable_tasks().await);
        assert!(
            registry
                .background_task_for_tool_use("agent-tool-use")
                .await
        );
        assert_eq!(signal.0.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(!registry.background_task(&id).await);
        registry
            .set_status(&id, TaskStatus::Completed)
            .await
            .unwrap();
        registry.unregister_foreground_agent(&id).await;
        assert!(
            matches!(registry.get(&id).await, Some(TaskState::LocalAgent(agent)) if agent.is_backgrounded)
        );
        let temporary = registry
            .register_foreground_agent(
                lingxi_core::host::task_registry::ForegroundAgentRegistration {
                    agent_id: lingxi_core::types::AgentId::new(),
                    ..registration
                },
            )
            .await
            .unwrap();
        registry
            .unregister_foreground_agent(&temporary.base().id)
            .await;
        assert!(registry.get(&temporary.base().id).await.is_none());
    }

    #[tokio::test]
    async fn sdk_lifecycle_covers_direct_writes_and_suppresses_noop_projection() {
        let (_dir, _fs, registry) = make_registry();
        let mut events = registry.subscribe_task_lifecycle();
        registry
            .register_background_bash(
                "bevent001".into(),
                "sleep 1".into(),
                "job".into(),
                Some("tool1".into()),
                None,
                None,
            )
            .await
            .unwrap();
        let started = events.try_recv().unwrap();
        assert_eq!(started["subtype"], "task_started");
        assert_eq!(started["task_type"], "local_bash");
        assert_eq!(started["tool_use_id"], "tool1");
        assert_eq!(started["is_backgrounded"], true);
        assert!(started.get("command").is_none());
        {
            let mut rows = registry.tasks.write().await;
            let base = rows.get_mut("bevent001").unwrap().base_mut();
            base.description = "renamed".into();
            base.total_paused_ms = 12;
        }
        let update = events.try_recv().unwrap();
        assert_eq!(update["subtype"], "task_updated");
        assert_eq!(
            update["patch"],
            serde_json::json!({"description":"renamed", "total_paused_ms":12})
        );
        registry.mark_notified("bevent001").await.unwrap();
        assert!(
            events.try_recv().is_err(),
            "notification bookkeeping is not SDK progress"
        );
        registry
            .set_status("bevent001", TaskStatus::Completed)
            .await
            .unwrap();
        assert_eq!(events.try_recv().unwrap()["patch"]["status"], "completed");
        let receipt = events.try_recv().unwrap();
        assert_eq!(receipt["subtype"], "task_notification");
        assert_eq!(receipt["status"], "completed");
        assert_eq!(receipt["summary"], "renamed");
        assert_eq!(receipt["tool_use_id"], "tool1");
        assert_eq!(
            events.try_recv().unwrap(),
            serde_json::json!({"type":"task_feature", "feature_name":"task_local_shell", "error_code":null})
        );
    }

    #[tokio::test]
    async fn sdk_lifecycle_is_registry_scoped_and_hides_observers() {
        let (_dir, _fs, registry) = make_registry();
        let (_other_dir, _other_fs, other) = make_registry();
        let mut events = registry.subscribe_task_lifecycle();
        let mut other_events = other.subscribe_task_lifecycle();
        registry
            .register_background_bash(
                "bevent002".into(),
                "sleep 1".into(),
                "job".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let shell = registry.get("bevent002").await.unwrap();
        events.try_recv().unwrap();
        let mut value = serde_json::to_value(shell.base()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.insert("id".into(), serde_json::json!("aobserver"));
        object.insert("task_type".into(), serde_json::json!("LocalAgent"));
        object.insert(
            "agent_id".into(),
            serde_json::json!(lingxi_core::types::AgentId::new()),
        );
        object.insert("is_observer".into(), serde_json::json!(true));
        object.insert(
            "prompt".into(),
            serde_json::json!("private observer prompt"),
        );
        object.insert("messages".into(), serde_json::json!([]));
        object.insert("pending_messages".into(), serde_json::json!([]));
        object.insert("is_backgrounded".into(), serde_json::json!(true));
        let observer: crate::state::LocalAgentTaskState = serde_json::from_value(value).unwrap();
        registry
            .tasks
            .write()
            .await
            .insert("aobserver", TaskState::LocalAgent(observer));
        registry
            .set_status("aobserver", TaskStatus::Completed)
            .await
            .unwrap();
        assert!(events.try_recv().is_err());
        assert!(other_events.try_recv().is_err());
    }

    #[tokio::test]
    async fn shell_health_stall_is_advisory_and_completion_still_delivers() {
        let (_dir, _fs, registry) = make_registry();
        let id = "bhealth01";
        registry
            .register_background_bash(
                id.into(),
                "read answer".into(),
                "prompt".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        registry.notify_bash_stall(id, "Continue?").await;
        let notices = registry.take_pending_task_notifications().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, "running");
        assert_eq!(notices[0].result.as_deref(), Some("Continue?"));
        let row = registry.get(id).await.unwrap();
        assert_eq!(row.base().status, TaskStatus::Running);
        assert!(!row.base().notified);
        registry
            .settle_background_bash(id, Some(0), false)
            .await
            .unwrap();
        let notices = registry.take_pending_task_notifications().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, "completed");
    }

    #[tokio::test]
    async fn shell_health_pressure_requires_idle_session_and_claims_once() {
        let (_dir, fs, registry) = make_registry();
        let id = "bhealth02";
        let (_, path) = registry
            .register_background_bash(
                id.into(),
                "sleep 100".into(),
                "sleep".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "unwired session"
        );
        registry.update_shell_session_activity(true, false, true);
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "recent interaction"
        );
        registry
            .shell_session_activity
            .lock()
            .unwrap()
            .last_interaction = Some(std::time::Instant::now() - Duration::from_secs(1801));
        registry.update_shell_session_activity(true, true, false);
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "busy model request"
        );
        registry.update_shell_session_activity(false, false, false);
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "print/SDK session"
        );
        registry.update_shell_session_activity(true, false, false);
        let mut agent_base = registry.get(id).await.unwrap().base().clone();
        agent_base.id = "rhealth01".into();
        agent_base.task_type = TaskType::RemoteAgent;
        registry.tasks.write().await.insert(
            agent_base.id.clone(),
            TaskState::RemoteAgent(crate::state::RemoteAgentTaskState {
                base: agent_base,
                remote_session_id: "remote".into(),
                remote_endpoint: "test".into(),
            }),
        );
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "active delegated task"
        );
        registry.tasks.write().await.remove("rhealth01");
        assert!(
            !registry.claim_bash_memory_pressure_stop(id).await,
            "killer not bound yet"
        );
        registry
            .cleanups
            .lock()
            .await
            .insert(id.into(), Arc::new(|| {}));
        assert!(registry.claim_bash_memory_pressure_stop(id).await);
        assert!(!registry.claim_bash_memory_pressure_stop(id).await);
        registry
            .settle_background_bash(id, None, false)
            .await
            .unwrap();
        let notices = registry.take_pending_task_notifications().await;
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].status, "killed");
        assert_eq!(notices[0].killed_by.as_deref(), Some("memory_pressure"));
        assert!(registry.take_pending_task_notifications().await.is_empty());
        assert!(fs
            .files
            .lock()
            .await
            .get(path.to_str().unwrap())
            .unwrap()
            .ends_with("\n[killed]\n"));
    }

    #[tokio::test]
    async fn pressure_stop_survives_cancellation_during_output_io() {
        struct Killer(Arc<tokio::sync::Notify>);
        #[async_trait]
        impl lingxi_core::host::task_registry::TaskKiller for Killer {
            async fn kill(&self) {
                self.0.notify_one();
            }
        }
        let (_dir, fs, registry) = make_registry();
        let registry = Arc::new(registry);
        let id = "bhealth03";
        let (_, path) = registry
            .register_background_bash(
                id.into(),
                "sleep 100".into(),
                "sleep".into(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let killed = Arc::new(tokio::sync::Notify::new());
        registry
            .bind_background_bash_process(id, Some(123), Arc::new(Killer(killed.clone())))
            .await
            .unwrap();
        registry.update_shell_session_activity(true, false, true);
        registry
            .shell_session_activity
            .lock()
            .unwrap()
            .last_interaction = Some(std::time::Instant::now() - Duration::from_secs(1801));
        // Hold the actual output backend while the registry publishes its stop.
        let files = fs.files.lock().await;
        let claimant = {
            let registry = registry.clone();
            tokio::spawn(async move { registry.claim_bash_memory_pressure_stop(id).await })
        };
        tokio::time::timeout(Duration::from_secs(2), async {
            while registry.get(id).await.unwrap().base().status != TaskStatus::Killed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        claimant.abort();
        assert!(claimant.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(2), killed.notified())
            .await
            .expect("cancelling output must not discard the native kill");
        drop(files);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if fs
                    .files
                    .lock()
                    .await
                    .get(path.to_str().unwrap())
                    .is_some_and(|text| text.ends_with("\n[killed]\n"))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("terminal output finalization survives its caller");
        assert!(!registry.claim_bash_memory_pressure_stop(id).await);
        assert_eq!(registry.take_pending_task_notifications().await.len(), 1);
    }

    fn adopted(task_id: &str, run_id: &str, args_app_id: &str) -> AdoptedWorkflow {
        AdoptedWorkflow {
            task_id: task_id.to_string(),
            session_uuid: Some("session-1".to_string()),
            // A real build workflow's name -- exactly what a forged
            // checkpoint would also carry (see `AdoptedWorkflow`'s doc
            // comment). Both tests below prove the guard no longer cares.
            workflow_id: "lingxi-local-app:local-app-build".to_string(),
            run_id: run_id.to_string(),
            script_path: "/workspace/.lingxi/workflows/build.js".to_string(),
            args: Some(format!(r#"{{"app_id":"{args_app_id}"}}"#)),
            transcript_dir: format!("/sessions/s1/subagents/workflows/{run_id}"),
            description: "Build local app".to_string(),
            start_time: SystemTime::now(),
        }
    }

    /// (1) Registry-level half of the P-1.12 fix: once a caller HAS
    /// re-derived a scope for an in-flight build (the job
    /// `engine-mobile`'s `adopt_session` does end to end --
    /// see `workflow_support::run_id_tests::an_adopted_in_flight_build_still_blocks_its_apps_delete`,
    /// which is the test carrying this exact name against the real
    /// resolution logic), `register_adopted_workflow_with_scope` stamps it on
    /// the adopted (`Paused`, non-terminal) row and the delete guard honors
    /// it -- independent of how that scope was derived.
    #[tokio::test]
    async fn an_adopted_in_flight_build_still_blocks_its_apps_delete_at_the_registry_api() {
        let (_dir, _fs, registry) = make_registry();
        let scope = crate::scope::LocalAppWorkflowTaskScope::for_build("resumed-app")
            .expect("well-formed app id");

        registry
            .register_adopted_workflow_with_scope(
                adopted("wadopted1", "wf_adopted1", "resumed-app"),
                Some(scope),
            )
            .await
            .expect("adopt with re-derived scope");

        let state = registry.get("wadopted1").await.expect("adopted state");
        assert!(
            !state.base().status.is_terminal(),
            "an adopted row must be non-terminal (Paused)"
        );
        assert_eq!(
            registry
                .find_nonterminal_local_app_workflows("resumed-app")
                .await,
            vec!["wadopted1".to_string()],
            "a build re-validated on adoption must still block its app's delete"
        );
    }

    /// (2) Registry-level half: a checkpoint adopted through the plain
    /// (unscoped) `register_adopted_workflow` entry point -- what any caller
    /// gets by default, and exactly what a forged checkpoint (`workflow_id`
    /// naming a real build workflow, `args.app_id` naming a victim app) must
    /// still resolve to once re-validated -- must never block another app's
    /// delete. See
    /// `workflow_support::run_id_tests::an_adopted_forged_workflow_still_gets_no_scope`
    /// for the test carrying this exact name that exercises the real
    /// re-validation logic end to end.
    #[tokio::test]
    async fn an_adopted_forged_workflow_still_gets_no_scope_at_the_registry_api() {
        let (_dir, _fs, registry) = make_registry();

        registry
            .register_adopted_workflow(adopted("wforged12", "wf_forged1", "victim-app"))
            .await
            .expect("adopt without scope");

        let state = registry.get("wforged12").await.expect("adopted state");
        assert!(
            !state.base().status.is_terminal(),
            "an adopted row must be non-terminal (Paused)"
        );
        assert!(
            registry
                .find_nonterminal_local_app_workflows("victim-app")
                .await
                .is_empty(),
            "a forged workflow_id/args.app_id pair must never block another \
            app's delete just because it was adopted"
        );
    }

    async fn running_scoped_workflow(
        registry: &TaskRegistry,
        task_id: &str,
        purpose: crate::scope::LocalAppWorkflowPurpose,
    ) {
        let scope = match purpose {
            crate::scope::LocalAppWorkflowPurpose::Build => {
                crate::scope::LocalAppWorkflowTaskScope::for_build("qa-app")
            }
            crate::scope::LocalAppWorkflowPurpose::UseTest => {
                crate::scope::LocalAppWorkflowTaskScope::for_use_test("qa-app")
            }
            crate::scope::LocalAppWorkflowPurpose::McpAuthoring => {
                crate::scope::LocalAppWorkflowTaskScope::for_mcp_authoring("qa-app")
            }
        }
        .expect("well-formed app id");
        registry
            .register_adopted_workflow_with_scope(
                adopted(task_id, "wf_terminal1", "qa-app"),
                Some(scope),
            )
            .await
            .expect("register workflow");
        registry
            .set_status(task_id, TaskStatus::Running)
            .await
            .expect("activate workflow");
    }

    fn checked_outcome() -> lingxi_core::host::task_registry::WorkflowTerminalOutcome {
        lingxi_core::host::task_registry::WorkflowTerminalOutcome {
            result: Some(r#"{"ok":true,"host_checked":true}"#.into()),
            agent_count: 3,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn local_app_terminal_commit_publishes_spool_metadata_and_state_together() {
        let (_dir, _fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit01",
            crate::scope::LocalAppWorkflowPurpose::UseTest,
        )
        .await;
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal(
                "wcommit01",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("commit success");

        assert_eq!(actual.base().status, TaskStatus::Completed);
        assert!(committed);
        assert!(published.load(std::sync::atomic::Ordering::SeqCst));
        let output = registry
            .output_manager
            .read(
                &registry.output_manager.path_for("wcommit01").unwrap(),
                crate::output_manager::OutputOptions::default(),
            )
            .await
            .expect("canonical spool");
        assert_eq!(output.content, r#"{"ok":true,"host_checked":true}"#);
    }

    #[tokio::test]
    async fn published_local_app_stays_completed_when_final_spool_write_fails() {
        let (_dir, fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit08",
            crate::scope::LocalAppWorkflowPurpose::UseTest,
        )
        .await;
        registry
            .tasks
            .write()
            .await
            .get_mut("wcommit08")
            .expect("registered workflow")
            .base_mut()
            .notified = false;

        // The pending placeholder succeeds, Host publication succeeds, and
        // only the subsequent canonical spool write fails.
        let placeholder_write = fs.write_count.load(std::sync::atomic::Ordering::SeqCst) + 1;
        fs.fail_write_at
            .store(placeholder_write + 1, std::sync::atomic::Ordering::SeqCst);
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal(
                "wcommit08",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("Host publication is the commit point");

        assert!(published.load(std::sync::atomic::Ordering::SeqCst));
        assert!(committed);
        assert_eq!(actual.base().status, TaskStatus::Completed);
        let TaskState::LocalWorkflow(workflow) = actual else {
            panic!("expected workflow")
        };
        assert_eq!(workflow.outcome, checked_outcome());

        let handle: &dyn lingxi_core::host::task_registry::TaskRegistryHandle = &registry;
        let chunk = handle
            .output("wcommit08", None)
            .await
            .expect("public TaskOutput projection");
        assert_eq!(chunk.status.as_deref(), Some("completed"));
        assert!(chunk.done);
        assert_eq!(chunk.content, r#"{"ok":true,"host_checked":true}"#);
        assert_eq!(
            chunk.output_path, None,
            "the stale pending spool is not an authoritative output path"
        );

        let notifications = registry.take_pending_task_notifications().await;
        let notification = notifications
            .iter()
            .find(|notification| notification.task_id == "wcommit08")
            .expect("completed terminal notification");
        assert_eq!(notification.status, "completed");
        assert_eq!(
            notification.result.as_deref(),
            Some(r#"{"ok":true,"host_checked":true}"#)
        );
        assert!(notification.error.is_none());
        assert_eq!(
            notification.output_path, None,
            "the notification must not advertise stale physical bytes"
        );

        let physical_path = registry.output_manager.path_for("wcommit08").unwrap();
        let physical = fs
            .files
            .lock()
            .await
            .get(physical_path.to_str().unwrap())
            .cloned()
            .expect("pending spool remains on disk");
        let physical: serde_json::Value = serde_json::from_str(&physical).unwrap();
        assert_eq!(physical["verified"], false);
        assert!(physical["error"]
            .as_str()
            .is_some_and(|error| error.contains("publication pending")));
    }

    #[tokio::test]
    async fn killed_local_app_never_runs_the_prepared_publication_commit() {
        let (_dir, _fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit02",
            crate::scope::LocalAppWorkflowPurpose::Build,
        )
        .await;
        registry
            .set_status("wcommit02", TaskStatus::Killed)
            .await
            .expect("kill wins");
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal(
                "wcommit02",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("return authoritative terminal row");

        assert_eq!(actual.base().status, TaskStatus::Killed);
        assert!(!committed);
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn prior_completed_local_app_does_not_claim_a_skipped_publication_commit() {
        let (_dir, _fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit06",
            crate::scope::LocalAppWorkflowPurpose::Build,
        )
        .await;
        registry
            .finish_workflow_terminal("wcommit06", checked_outcome(), TaskStatus::Completed)
            .await
            .expect("competing completion wins");
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal(
                "wcommit06",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("return authoritative completed row");

        assert_eq!(actual.base().status, TaskStatus::Completed);
        assert!(!committed);
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn local_app_publication_failure_is_one_canonical_failed_terminal() {
        let (_dir, _fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit03",
            crate::scope::LocalAppWorkflowPurpose::Build,
        )
        .await;

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal("wcommit03", checked_outcome(), || async {
                Err("injected pointer commit failure".into())
            })
            .await
            .expect("commit returns failed row");

        assert_eq!(actual.base().status, TaskStatus::Failed);
        assert!(!committed);
        let TaskState::LocalWorkflow(workflow) = actual else {
            panic!("expected workflow")
        };
        assert!(workflow.outcome.result.is_none());
        assert!(workflow
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("pointer commit failure")));
        let output = registry
            .output_manager
            .read(
                &registry.output_manager.path_for("wcommit03").unwrap(),
                crate::output_manager::OutputOptions::default(),
            )
            .await
            .expect("failure spool");
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["verified"], false);
        assert!(payload["error"]
            .as_str()
            .is_some_and(|error| error.contains("pointer commit failure")));
    }

    #[tokio::test]
    async fn local_app_publication_failure_fails_closed_when_failure_spool_rewrite_fails() {
        let (_dir, fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit07",
            crate::scope::LocalAppWorkflowPurpose::Build,
        )
        .await;
        registry
            .tasks
            .write()
            .await
            .get_mut("wcommit07")
            .expect("registered workflow")
            .base_mut()
            .notified = false;

        // The unverified publication placeholder is the next write. The
        // following failure-payload rewrite fails, leaving the placeholder on
        // disk while the drained notification suppresses that stale path.
        let placeholder_write = fs.write_count.load(std::sync::atomic::Ordering::SeqCst) + 1;
        fs.fail_write_at
            .store(placeholder_write + 1, std::sync::atomic::Ordering::SeqCst);

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal("wcommit07", checked_outcome(), || async {
                Err("injected pointer commit failure".into())
            })
            .await
            .expect("commit returns failed row");

        assert_eq!(actual.base().status, TaskStatus::Failed);
        assert!(!committed);
        let TaskState::LocalWorkflow(workflow) = actual else {
            panic!("expected workflow")
        };
        assert!(workflow.outcome.result.is_none());
        assert!(workflow
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("pointer commit failure")));

        let notifications = registry.take_pending_task_notifications().await;
        let notification = notifications
            .iter()
            .find(|notification| notification.task_id == "wcommit07")
            .expect("failed terminal notification");
        assert_eq!(notification.status, "failed");
        assert!(notification
            .error
            .as_deref()
            .is_some_and(|error| error.contains("pointer commit failure")));
        assert!(notification.result.is_none());
        assert_eq!(
            notification.output_path, None,
            "the failed rewrite leaves no authoritative physical spool"
        );
        let physical_path = registry.output_manager.path_for("wcommit07").unwrap();
        let physical = fs
            .files
            .lock()
            .await
            .get(physical_path.to_str().unwrap())
            .cloned()
            .expect("pending physical spool must still exist");
        let physical_payload: serde_json::Value = serde_json::from_str(&physical).unwrap();
        assert_eq!(physical_payload["ok"], false);
        assert_eq!(physical_payload["verified"], false);
        assert!(physical_payload["error"]
            .as_str()
            .is_some_and(|error| error.contains("publication")));

        let output = registry
            .output_manager
            .read(
                &registry.output_manager.path_for("wcommit07").unwrap(),
                crate::output_manager::OutputOptions::default(),
            )
            .await
            .expect("fail-closed failure spool");
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["verified"], false);
        assert!(payload["error"]
            .as_str()
            .is_some_and(|error| error.contains("pointer commit failure")));

        let path = registry.output_manager.path_for("wcommit07").unwrap();
        registry
            .output_manager
            .append(&path, "late raw handler output")
            .await
            .expect("late terminal appends are absorbed");
        let offset = registry
            .output_manager
            .read(
                &path,
                crate::output_manager::OutputOptions {
                    offset: Some(1),
                    limit: None,
                },
            )
            .await
            .expect("post-failure offset read");
        assert!(offset.content.is_empty());
        assert_eq!(offset.total_lines, 1);

        // Exercise the public TaskOutput projection as well as the backing
        // manager: the failed registry row and the fail-closed payload must
        // agree even when the rewrite itself was rejected.
        let handle: &dyn lingxi_core::host::task_registry::TaskRegistryHandle = &registry;
        let chunk = handle
            .output("wcommit07", None)
            .await
            .expect("TaskOutput projection");
        assert_eq!(chunk.status.as_deref(), Some("failed"));
        assert!(chunk.done);
        assert_eq!(chunk.output_path, None);
        let chunk_payload: serde_json::Value = serde_json::from_str(&chunk.content).unwrap();
        assert_eq!(chunk_payload["ok"], false);
        assert_eq!(chunk_payload["verified"], false);
    }

    #[tokio::test]
    async fn local_app_spool_failure_never_publishes_or_completes() {
        let (_dir, fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit04",
            crate::scope::LocalAppWorkflowPurpose::UseTest,
        )
        .await;
        fs.fail_writes
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let (actual, committed) = registry
            .commit_local_app_workflow_terminal(
                "wcommit04",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect("spool failure becomes task failure");

        assert_eq!(actual.base().status, TaskStatus::Failed);
        assert!(!committed);
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
        let TaskState::LocalWorkflow(workflow) = actual else {
            panic!("expected workflow")
        };
        assert!(workflow
            .outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("terminal spool replacement failed")));
        let output = registry
            .output_manager
            .read(
                &registry.output_manager.path_for("wcommit04").unwrap(),
                crate::output_manager::OutputOptions::default(),
            )
            .await
            .expect("fail-closed initial spool failure");
        let payload: serde_json::Value = serde_json::from_str(&output.content).unwrap();
        assert_eq!(payload["ok"], false);
        assert_eq!(payload["verified"], false);
    }

    #[tokio::test]
    async fn mcp_authoring_scope_cannot_use_the_qa_terminal_commit() {
        let (_dir, _fs, registry) = make_registry();
        running_scoped_workflow(
            &registry,
            "wcommit05",
            crate::scope::LocalAppWorkflowPurpose::McpAuthoring,
        )
        .await;
        let published = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = published.clone();

        let error = registry
            .commit_local_app_workflow_terminal(
                "wcommit05",
                checked_outcome(),
                move || async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                },
            )
            .await
            .expect_err("only Build/UseTest may commit QA publication");

        assert!(matches!(error, TaskError::Unsupported));
        assert!(!published.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            registry.get("wcommit05").await.unwrap().base().status,
            TaskStatus::Running
        );
    }
}

#[cfg(test)]
#[path = "registry_test.rs"]
mod registry_test;
