//! Local-agent (one-shot subagent) task handler — M2 implementation.
//!
//! Spawns a child subagent through the [`SubagentSpawner`] seam, drives it to
//! completion on the engine's [`RuntimeSpawner`] (never `tokio::spawn` directly
//! — D17), spools the terminal [`SubagentResult`] payload into the task's spool
//! file, and reports the terminal [`TaskStatus`] through a narrow status sink.
//!
//! This is the Rust analogue of the claude-code `LocalAgentTask` /
//! `registerAsyncAgent` lifecycle: the agent runs as a backgrounded one-shot,
//! the transcript/result is spooled to a per-agent disk file, and `kill`
//! aborts the in-flight worker (the analogue of TS `abortController.abort()` +
//! `unregisterCleanup()`).
//!
//! ## Why kill rides on the runtime handle, not a process handle
//!
//! Unlike [`crate::handlers::local_bash::LocalBashHandler`] (which can recover
//! a [`platform_api::ProcessHandle`] for a true OS kill), [`SubagentSpawner::spawn`]
//! is *await-to-completion*: it returns the terminal [`SubagentResult`] and
//! hands back no live handle while the subagent runs. The only cancellation
//! primitive available is therefore cooperative cancellation of the worker
//! future via [`RuntimeSpawner::cancel`] on the [`platform_api::BackgroundTaskHandle`]
//! returned by [`RuntimeSpawner::spawn`]. The handler records that handle (plus
//! the `runtime` Arc that minted it, since [`TaskContext`] is per-call and the
//! synchronous cleanup path has no `ctx`) in a map keyed by `task_id`, so both
//! [`Task::kill`] and [`LocalAgentHandler::drain_pending_kills`] can cancel the
//! worker. Dropping the worker future drops the pending `spawner.spawn(..)`
//! await, which is the cooperative-cancel contract the runtime documents.
//!
//! ## `agent_id` and `subagent_type` are independent sibling fields
//!
//! [`TaskSpawnInput::LocalAgent`] carries BOTH an [`protocol::AgentId`] (a
//! per-instance identity UUID) AND a resolved `subagent_type: String` (one of
//! the built-in subagent-type *names*), exactly mirroring the TS
//! `LocalAgentTaskState` shape (`agentId` + `agentType`). The caller that
//! already resolved the `AgentDefinition` stamps `subagent_type` (applying the
//! `'general-purpose'` fallback when the definition has none, matching the TS
//! `selectedAgent.agentType ?? 'general-purpose'`). Modern background Agent
//! launches also carry the complete already-resolved request and inheritance
//! handles; legacy direct-task callers retain the compact fallback path.

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use agent::{StreamingSubagentSpawner, SubagentEvent};
use async_trait::async_trait;
use platform_api::{
    BackgroundTaskHandle, BudgetEnforcerHandle, RuntimeSpawner, SubagentInheritance,
    SubagentResult, SubagentSpawnRequest, SubagentSpawner,
};
use protocol::AgentId;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

// Re-export the status-sink seam from the bash handler so callers wire a single
// implementation across handlers (the wire step plugs in one adapter over the
// registry). Defined once in `local_bash` to avoid divergence.
pub use crate::handlers::local_bash::{NoopStatusSink, TaskStatusSink};

/// Handler name reported by [`Task::name`] / used as the runtime task-name
/// prefix. Byte-aligned with the `"local_agent"` wire string the registration
/// step uses.
const HANDLER_NAME: &str = "local_agent";

/// A worker-cancel record: the [`BackgroundTaskHandle`] returned by
/// [`RuntimeSpawner::spawn`] plus the `runtime` Arc that minted it.
///
/// [`TaskContext`] (which carries `runtime`) is per-call and the synchronous
/// [`TaskHandle::cleanup`] closure receives no `ctx`, so the handler captures
/// the `runtime` alongside the handle at spawn time. That lets both
/// [`Task::kill`] and [`LocalAgentHandler::drain_pending_kills`] issue
/// [`RuntimeSpawner::cancel`] without a fresh `ctx`.
///
/// Public because it appears in the signature of the public
/// [`LocalAgentHandler::workers_map`] accessor (parity with
/// `LocalBashHandler::children_map`); fields stay private.
/// claude-code `maxTurnsReached` — the turn budget a run exhausted, or `None`
/// when it stopped for any other reason.
///
/// The runner's max-turns fall-through is the ONLY completion whose payload is
/// `{"reason":"max_turns_exhausted","max_turns":N}` (`agent/src/runner.rs`);
/// every other completion carries the agent's answer under `text`. Reading the
/// budget from the payload is what lets the terminal notification say the
/// result is partial instead of reporting the run as simply "finished".
fn max_turns_reached_from(result: &serde_json::Value) -> Option<u64> {
    if result.get("reason").and_then(serde_json::Value::as_str) != Some("max_turns_exhausted") {
        return None;
    }
    result.get("max_turns").and_then(serde_json::Value::as_u64)
}

pub struct WorkerCancel {
    handle: BackgroundTaskHandle,
    runtime: Arc<dyn RuntimeSpawner>,
    persistent_teardown: Option<PersistentTeardown>,
}

struct PersistentTeardown {
    agent_id: Arc<StdMutex<Option<AgentId>>>,
}

struct AgentIdentityObserver {
    effort: Option<String>,
    task_id: String,
    sink: Arc<dyn TaskStatusSink>,
    human_epoch: Option<u64>,
}

#[async_trait]
impl platform_api::subagent_spawn::SubagentSpawnObserver for AgentIdentityObserver {
    async fn on_model_selected(
        &self,
        event: &platform_api::subagent_spawn::SubagentObservation,
        effort: Option<&str>,
    ) {
        if let platform_api::subagent_spawn::SubagentObservation::Allocated { model, .. } = event {
            self.sink
                .set_agent_display(&self.task_id, model.clone(), effort.map(str::to_string))
                .await;
        }
    }

    async fn before_start(
        &self,
        event: &platform_api::subagent_spawn::SubagentObservation,
    ) -> Result<(), platform_api::SubagentSpawnError> {
        if let platform_api::subagent_spawn::SubagentObservation::Allocated {
            agent_id,
            model,
            ..
        } = event
        {
            if let Some(epoch) = self.human_epoch {
                self.sink
                    .task_registry()
                    .ok_or_else(|| {
                        platform_api::SubagentSpawnError::Runtime(
                            "human resume registry unavailable".into(),
                        )
                    })?
                    .begin_human_task_resume(&self.task_id, epoch)
                    .await
                    .map_err(|error| {
                        platform_api::SubagentSpawnError::Runtime(error.to_string())
                    })?;
            }
            self.sink
                .bind_agent_id(&self.task_id, *agent_id)
                .await
                .map_err(platform_api::SubagentSpawnError::Runtime)?;
            self.sink
                .set_agent_display(&self.task_id, model.clone(), self.effort.clone())
                .await;
        }
        Ok(())
    }
    async fn on_event(&self, _: platform_api::subagent_spawn::SubagentObservation) {}
}

impl WorkerCancel {
    fn take_persistent_agent_id(&self) -> Option<AgentId> {
        self.persistent_teardown
            .as_ref()
            .and_then(|teardown| teardown.agent_id.lock().unwrap().take())
    }
}

/// Local-agent task handler.
///
/// Holds the constructor-injected dependencies that are *not* on
/// [`TaskContext`] — the subagent spawner, the parent's `tool_invoker` +
/// `budget` (bundled into a per-spawn [`SubagentInheritance`]), the spool
/// manager, and the terminal-status sink — plus the shared worker-handle map
/// keyed by `task_id` so [`Task::kill`] can cancel the in-flight worker future.
/// The `subagent_type` is no longer derived here: it arrives already resolved
/// on the [`TaskSpawnInput::LocalAgent`] variant and is forwarded verbatim.
pub struct LocalAgentHandler {
    resume_recipes: Arc<StdMutex<HashMap<String, (SubagentSpawnRequest, SubagentInheritance)>>>,
    /// Allocates a subagent slot and pumps it to a terminal [`SubagentResult`].
    spawner: Arc<dyn SubagentSpawner>,
    /// The persistent/resumable spawn seam (local_agent resume). When wired AND
    /// the spawn is backgrounded, the agent runs PERSISTENT — it "comes to rest"
    /// after each turn-set and can be resumed via [`Task::send_message`] —
    /// instead of the one-shot `spawner`. Production passes the same
    /// `PoolSubagentSpawner` (it impls both `SubagentSpawner` and this).
    streaming_spawner: Option<Arc<dyn StreamingSubagentSpawner>>,
    /// `task_id` → the resting agent's id, for `send_message` resume routing.
    /// Populated for a live persistent agent; removed when it terminates.
    agent_ids: Arc<Mutex<HashMap<String, AgentId>>>,
    /// `task_id` → the carried isolation worktree for a live PERSISTENT agent.
    /// A kill/cleanup that cancels the outer event-pump still must run the
    /// terminal keep/cleanup judgment before publishing `Killed`.
    persistent_worktrees: Arc<Mutex<HashMap<String, platform_api::worktree::WorktreeHandle>>>,
    /// `task_id` → the SKILL this agent is, when a `context: fork` skill
    /// launched it. The fork identity the resume gate corroborates against the
    /// on-disk scoping record. Kept beside [`Self::agent_ids`] and torn down
    /// with it.
    fork_names: Arc<Mutex<HashMap<String, String>>>,
    /// `task_id` → the LAST rest payload for a persistent agent. If the agent
    /// is killed after coming to rest, cancelling the outer worker still leaves
    /// enough terminal payload to match the normal completion path.
    persistent_outcomes:
        Arc<Mutex<HashMap<String, platform_api::task_registry::AgentTerminalOutcome>>>,
    /// Consulted before a parked agent is resumed: a forked skill whose
    /// permission scoping cannot be re-established must NOT resume under the
    /// parent's (wider) permissions. `None` ⇒ no gate, which is correct for a
    /// host that also cannot launch a forked skill.
    fork_resume_gate: Option<Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>>,
    /// Records a parked agent so a LATER process can restore it. Written each
    /// time the agent comes to rest, erased when it terminates. `None` ⇒ no
    /// durable record, which is correct for a host that also cannot restore.
    parked_store: Option<Arc<dyn platform_api::parked_agent_store::ParkedAgentStore>>,
    /// Parent's tool invoker — passed through *unchanged* in
    /// [`SubagentInheritance`] (the recursion lock relies on `Arc::ptr_eq`).
    tool_invoker: Arc<dyn platform_api::ToolInvoker>,
    /// Parent's budget enforcer — passed through *unchanged* so budget charges
    /// aggregate across the whole agent tree (`Arc::ptr_eq` invariant).
    budget: Arc<dyn BudgetEnforcerHandle>,
    /// Owns the spool directory + path allocation for the result payload.
    output_manager: Arc<TaskOutputManager>,
    /// Where terminal status transitions / token usage are reported.
    status_sink: Arc<dyn TaskStatusSink>,
    /// `task_id` → live worker-cancel record. Populated for the duration of the
    /// spawn; the worker removes its own entry on exit, and [`Task::kill`]
    /// removes + cancels it if still present.
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    /// Task ids queued for teardown by the synchronous [`TaskHandle::cleanup`]
    /// closure (which cannot await). The closure records the request here so a
    /// contended async mutex cannot silently drop cancellation.
    pending_kill: Arc<StdMutex<Vec<(String, u64)>>>,
    /// Runs the terminal keep/cleanup judgment on a background agent's
    /// isolation worktree (`SubagentSpawnRequest::worktree`) — claude-code
    /// hands its `getWorktreeResult` closure to the detached async lifecycle,
    /// so the worker here judges via
    /// [`platform_api::worktree::agent_worktree_result`] when the agent reaches a
    /// terminal state (keep when dirty/ahead, else auto-remove). `None`
    /// (default) ⇒ no worktree handling: a carried worktree is left in place,
    /// the conservative direction.
    worktree_manager: Option<Arc<dyn platform_api::worktree::WorktreeManager>>,
}

impl LocalAgentHandler {
    /// Construct a handler with the injected dependencies.
    ///
    /// `fs` and `runtime` arrive per-call via [`TaskContext`] and are
    /// deliberately *not* injected here. `tool_invoker` + `budget` are stored
    /// so each spawn can bundle them into a [`SubagentInheritance`] (cloning the
    /// `Arc` preserves pointer identity — required by the recursion-lock +
    /// budget-aggregation invariants). The `subagent_type` is supplied per-spawn
    /// on the [`TaskSpawnInput::LocalAgent`] variant, so there is nothing to
    /// inject here.
    #[must_use]
    pub fn new(
        spawner: Arc<dyn SubagentSpawner>,
        tool_invoker: Arc<dyn platform_api::ToolInvoker>,
        budget: Arc<dyn BudgetEnforcerHandle>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            resume_recipes: Arc::new(StdMutex::new(HashMap::new())),
            spawner,
            streaming_spawner: None,
            agent_ids: Arc::new(Mutex::new(HashMap::new())),
            persistent_worktrees: Arc::new(Mutex::new(HashMap::new())),
            fork_names: Arc::new(Mutex::new(HashMap::new())),
            persistent_outcomes: Arc::new(Mutex::new(HashMap::new())),
            fork_resume_gate: None,
            parked_store: None,
            tool_invoker,
            budget,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workers: Arc::new(Mutex::new(HashMap::new())),
            pending_kill: Arc::new(StdMutex::new(Vec::new())),
            worktree_manager: None,
        }
    }

    /// Wire the worktree manager so a BACKGROUND agent's `isolation:"worktree"`
    /// worktree (resolved by `AgentTool` before dispatch and carried on
    /// [`SubagentSpawnRequest::worktree`]) is auto-cleaned — kept when
    /// dirty/ahead — when the agent reaches a terminal state. This is the
    /// async-path owner of the same keep/cleanup judgment the sync path runs
    /// inside `AgentTool` (claude-code's `getWorktreeResult` closure).
    #[must_use]
    pub fn with_worktree_manager(
        mut self,
        manager: Arc<dyn platform_api::worktree::WorktreeManager>,
    ) -> Self {
        self.worktree_manager = Some(manager);
        self
    }

    /// Wire the persistent/resumable spawn seam (local_agent resume). When set,
    /// a backgrounded spawn runs PERSISTENT (comes to rest + resumable) instead
    /// of one-shot. Production passes the same `PoolSubagentSpawner`.
    #[must_use]
    pub fn with_streaming_spawner(mut self, streaming: Arc<dyn StreamingSubagentSpawner>) -> Self {
        self.streaming_spawner = Some(streaming);
        self
    }

    /// Attach a [`TaskStatusSink`] so terminal transitions / token usage are
    /// reported (e.g. an adapter over [`crate::registry::TaskRegistry`]).
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Wire the durable parked-agent store, so a backgrounded agent survives
    /// the process that ran it.
    #[must_use]
    pub fn with_parked_agent_store(
        mut self,
        store: Arc<dyn platform_api::parked_agent_store::ParkedAgentStore>,
    ) -> Self {
        self.parked_store = Some(store);
        self
    }

    /// Wire the forked-skill resume gate. Without it, resuming a forked skill
    /// would silently run it under the parent's (wider) permissions rather than
    /// the scoping it was launched with.
    #[must_use]
    pub fn with_fork_resume_gate(
        mut self,
        gate: Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>,
    ) -> Self {
        self.fork_resume_gate = Some(gate);
        self
    }

    /// Share the same `workers` map with an external owner (e.g. the registry
    /// wiring) so a [`TaskHandle::cleanup`] closure and [`Task::kill`] observe
    /// the same handles.
    #[must_use]
    pub fn workers_map(&self) -> Arc<Mutex<HashMap<String, WorkerCancel>>> {
        self.workers.clone()
    }

    /// Drain records queued by [`TaskHandle::cleanup`] and cancel each worker
    /// future for real. This is the async counterpart of the synchronous
    /// cleanup closure: the registry/cleanup-registry calls it on agent
    /// teardown so a subagent outliving its parent is aborted (claude-code
    /// `killTask`-on-cleanup parity). Status is flipped to `Killed` UNLESS the
    /// task already reached a terminal status (a raced pending-kill record must
    /// not clobber a real Completed/Failed with `Killed`).
    pub async fn drain_pending_kills(&self) {
        let pending = {
            let mut pending = self.pending_kill.lock().unwrap();
            std::mem::take(&mut *pending)
        };
        for (task_id, generation) in pending {
            let rec = {
                let mut workers = self.workers.lock().await;
                if !workers
                    .get(&task_id)
                    .is_some_and(|worker| worker.handle.task_id == generation)
                {
                    continue;
                }
                workers.remove(&task_id).expect("generation checked")
            };
            let _ = rec.runtime.cancel(&rec.handle).await;
            let fallback_agent_id = rec.take_persistent_agent_id();
            // Don't overwrite an already-reported terminal status: a subagent
            // that finished on its own before this (possibly raced) record was
            // drained keeps its real terminal status rather than being flipped
            // to Killed.
            let already_terminal = self.status_sink.is_terminal(&task_id).await;
            if !self
                .finish_persistent_terminal(
                    &task_id,
                    TaskStatus::Killed,
                    fallback_agent_id,
                    !already_terminal,
                )
                .await
                && !already_terminal
            {
                self.status_sink
                    .set_status(&task_id, TaskStatus::Killed)
                    .await;
            }
        }
    }

    async fn finish_persistent_terminal(
        &self,
        task_id: &str,
        status: TaskStatus,
        fallback_agent_id: Option<AgentId>,
        publish_terminal: bool,
    ) -> bool {
        // `fallback_agent_id` closes the post-spawn/pre-map-registration kill
        // window. Prefer the routing-map entry once present, but consume only
        // one id so the same inner runner is never stopped twice.
        // When spawn_persistent has returned but the worker is still waiting
        // to publish into `agent_ids`, kill already owns the same id through
        // the cancel record. Do not wait on the routing-map lock before
        // stopping that inner runner: doing so can leave its pool slot live
        // for the whole registration window. The cancelled worker cannot
        // complete a future insert; after stop, take the lock and erase any
        // registration that raced cancellation.
        let used_fallback = fallback_agent_id.is_some();
        let agent_id = match fallback_agent_id {
            Some(agent_id) => Some(agent_id),
            None => self.agent_ids.lock().await.remove(task_id),
        };
        let worktree = self.persistent_worktrees.lock().await.remove(task_id);
        let had_fork = self.fork_names.lock().await.remove(task_id).is_some();
        let mut outcome = self
            .persistent_outcomes
            .lock()
            .await
            .remove(task_id)
            .unwrap_or_default();

        if agent_id.is_none() && worktree.is_none() && !had_fork && outcome == Default::default() {
            return false;
        }

        if let Some(streaming) = &self.streaming_spawner {
            if let Some(agent_id) = agent_id {
                let _ = streaming.stop(&agent_id).await;
                if let Some(store) = &self.parked_store {
                    store.unpark(agent_id).await;
                }
            }
        }
        if used_fallback {
            self.agent_ids.lock().await.remove(task_id);
        }

        if let (Some(mgr), Some(handle)) = (&self.worktree_manager, worktree.as_ref()) {
            if let Some((path, branch)) =
                platform_api::worktree::agent_worktree_result(mgr.as_ref(), handle).await
            {
                outcome.worktree_path = Some(path);
                outcome.worktree_branch = Some(branch);
            }
        }

        if publish_terminal {
            self.status_sink.set_agent_outcome(task_id, outcome).await;
            self.status_sink.set_status(task_id, status).await;
        }
        true
    }
}

#[async_trait]
impl Task for LocalAgentHandler {
    async fn has_live_worker(&self, task_id: &str) -> bool {
        self.workers.lock().await.contains_key(task_id)
    }

    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::LocalAgent
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        self.spawn_inner(input, ctx, None).await
    }

    fn forget_resume_recipe(&self, task_id: &str) {
        self.resume_recipes.lock().unwrap().remove(task_id);
    }

    async fn register_resume_recipe(
        &self,
        task_id: &str,
        request: SubagentSpawnRequest,
        inheritance: SubagentInheritance,
    ) -> Result<(), TaskError> {
        self.resume_recipes
            .lock()
            .unwrap()
            .insert(task_id.to_owned(), (request, inheritance));
        Ok(())
    }

    async fn prepare_human_resume(
        &self,
        task_id: &str,
        agent_id: AgentId,
        epoch: u64,
        ctx: TaskContext,
    ) -> Result<crate::task_trait::HumanResumePrepared, TaskError> {
        self.prepare_human_restore(task_id, agent_id, epoch, ctx)
            .await
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        // Recover the live worker-cancel record (if any) and cancel the
        // in-flight worker future — the analogue of TS `abortController.abort()`
        // + `unregisterCleanup()`. An absent record ⇒ the subagent already
        // terminated ⇒ graceful no-op (claude-code `status !== 'running'`).
        let rec = self.workers.lock().await.remove(task_id);
        let mut cancel_error = None;
        let fallback_agent_id = if let Some(rec) = rec {
            if let Err(error) = rec.runtime.cancel(&rec.handle).await {
                cancel_error = Some(TaskError::Io(error.to_string()));
            }
            rec.take_persistent_agent_id()
        } else {
            None
        };
        // A raced kill must not clobber a real terminal outcome that the worker
        // has already reported through the sink.
        let already_terminal = self.status_sink.is_terminal(task_id).await;
        if !self
            .finish_persistent_terminal(
                task_id,
                TaskStatus::Killed,
                fallback_agent_id,
                !already_terminal,
            )
            .await
            && !already_terminal
        {
            self.status_sink
                .set_status(task_id, TaskStatus::Killed)
                .await;
        }
        if let Some(error) = cancel_error {
            return Err(error);
        }
        Ok(())
    }

    fn supports_messages(&self) -> bool {
        // A PERSISTENT (backgrounded + resumable) local_agent accepts messages:
        // `send_message` resumes a resting agent (claude-code `resumeAgentBackground`
        // / `injectUserMessageToTeammate`). Without the streaming seam wired, the
        // one-shot agent has no inbound channel → not supported.
        self.streaming_spawner.is_some()
    }

    /// Resume a resting persistent local_agent: deliver `message` to its parked
    /// runner (via [`StreamingSubagentSpawner::resume`] → `pool.send_event`),
    /// which appends it to history and runs the next turn-set — the
    /// `injectUserMessageToTeammate` / `resumeAgentBackground` analogue. Errors
    /// when the streaming seam is unwired or the agent has terminated/evicted.
    async fn send_message(
        &self,
        task_id: &str,
        message: String,
        _ctx: TaskContext,
    ) -> Result<(), TaskError> {
        let Some(streaming) = &self.streaming_spawner else {
            return Err(TaskError::Unsupported);
        };
        let agent_id = self
            .agent_ids
            .lock()
            .await
            .get(task_id)
            .copied()
            .ok_or(TaskError::TerminatedTask)?;
        // 2.1.266 `a0`, the TARGET half of the resume gate: refuse to re-enter an
        // agent whose own stop has not finished. Upstream's condition is
        // `a0(e) || !xh(e) && Ps(status)` — either the target is stop-pending, or
        // it still holds a loop entry while its row already reads terminal, which
        // is the same "stopped but not exited" state seen from the other side.
        // The port's loop entry is the live worker record.
        let target_still_stopping =
            platform_api::agent_processes::is_stop_pending(&agent_id.to_string())
                || (self.workers.lock().await.contains_key(task_id)
                    && self.status_sink.is_terminal(task_id).await);
        if target_still_stopping {
            return Err(TaskError::Internal(format!(
                "Agent {agent_id} is still stopping \u{2014} its previous run was stopped \
                 but has not exited. Re-run TaskStop on it or wait for it to exit before \
                 resuming."
            )));
        }
        // Resuming a parked agent re-enters it with the permissions of whatever
        // is wired NOW. For a forked skill that is the parent's scoping, which
        // is strictly wider than the skill's — so the gate re-establishes (or
        // refuses) before the message can reach the runner. Every ambiguous
        // on-disk state refuses.
        if let Some(gate) = &self.fork_resume_gate {
            let fork_name = self.fork_names.lock().await.get(task_id).cloned();
            gate.check_resume(agent_id, fork_name.as_deref())
                .await
                .map_err(TaskError::Internal)?;
        }
        // Publish Running before dispatch: the resumed runner can finish before
        // resume returns, and a late Running write would erase that new rest.
        self.status_sink
            .set_status(task_id, TaskStatus::Running)
            .await;
        if let Err(error) = streaming.resume(&agent_id, message).await {
            // A failed dispatch cannot leave the task advertising active work.
            // The runner channel is unavailable; retire its retained state.
            self.status_sink
                .set_status(task_id, TaskStatus::Failed)
                .await;
            return Err(TaskError::Internal(error.to_string()));
        }
        Ok(())
    }
}

#[path = "human_resume.rs"]
mod human_resume;

struct HumanRestore {
    task_id: String,
    epoch: u64,
    ready: tokio::sync::oneshot::Sender<Result<(), TaskError>>,
}
impl LocalAgentHandler {
    async fn spawn_inner(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
        mut restoring: Option<HumanRestore>,
    ) -> Result<TaskHandle, TaskError> {
        // 1. Only the LocalAgent variant is accepted; reject the other six.
        let TaskSpawnInput::LocalAgent {
            agent_id: requested_agent_id,
            subagent_type,
            prompt,
            is_backgrounded,
            // The registry's `state_for_spawn` stamps this onto `TaskStateBase`;
            // the handler itself doesn't consume it.
            tool_use_id: _,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            spawn_request,
            inheritance,
        } = input
        else {
            return Err(TaskError::Internal(
                "local_agent handler received a non-LocalAgent spawn input".into(),
            ));
        };
        // `is_backgrounded` drives surfacing/eviction (the registry layer
        // records it on `LocalAgentTaskState`) and is also threaded onto the
        // spawn request's `run_in_background` below for spawn-surface parity; the
        // one-shot subagent's local run mechanics are otherwise identical.

        // 2. Generate the task id (prefix 'a') and allocate its spool file.
        let task_id = restoring
            .as_ref()
            .map(|restore| restore.task_id.clone())
            .unwrap_or_else(|| crate::id::generate_task_id(TaskType::LocalAgent));
        let spool_path = if restoring.is_some() {
            self.output_manager.path_for(&task_id)
        } else {
            self.output_manager.allocate(&task_id).await
        }
        .map_err(|e| TaskError::Io(e.to_string()))?;
        // Validate UTF-8 once up front (the output manager's `append` assumes a
        // UTF-8 spool path); spool paths under the manager's dir always are.
        if spool_path.to_str().is_none() {
            return Err(TaskError::Internal("spool path is not valid UTF-8".into()));
        }

        // 3. Preserve the complete background Agent request (model/cwd/context/
        //    isolation/schema/depth and more). Direct TaskCreate-style callers
        //    lack that payload, so only they use the compact legacy fallback.
        // The fork identity travels on the full spawn request, not the compact
        // task-index fields — a forked skill is dispatched through the same
        // `SubagentSpawnRequest` as any background agent. Read BEFORE the
        // request is consumed below.
        let fork_name = spawn_request
            .as_ref()
            .and_then(|r| r.forked_skill_name.clone());
        let request = spawn_request.unwrap_or_else(|| SubagentSpawnRequest {
            teammate_color: None,
            subagent_type,
            prompt,
            observer: None,
            context_paths: Vec::new(),
            description: None,
            model: None,
            model_profile: None,
            run_in_background: is_backgrounded,
            name: None,
            team_name: None,
            creator_teammate_name,
            creator_team_name,
            creator_agent_id,
            mode: None,
            isolation: None,
            cwd: None,
            worktree: None,
            fork_context_messages: None,
            fork_parent_system_prompt: None,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            depth: 0,
            origin_session_id: None,
            parent_model_override: None,
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
        });
        // What `park` needs, cloned BEFORE `request` moves into the spawn:
        // the launch configuration is what a rebuilt runner is configured from.
        let parked_request = request.clone();
        let parked_description = request.description.clone().unwrap_or_default();

        // 4. Preserve the immediate parent's registry/budget handles. Root
        //    handles are only the correct fallback for legacy direct tasks.
        let inherit = inheritance.unwrap_or_else(|| SubagentInheritance {
            tool_invoker: self.tool_invoker.clone(),
            budget: self.budget.clone(),
        });

        if restoring.is_none() {
            self.resume_recipes
                .lock()
                .unwrap()
                .insert(task_id.clone(), (request.clone(), inherit.clone()));
        }
        let human_epoch = restoring.as_ref().map(|restore| restore.epoch);
        let mut human_ready = restoring.take().map(|restore| restore.ready);

        // The isolation worktree `AgentTool` resolved for this agent (if any).
        // The BACKGROUND lifecycle owns the terminal keep/cleanup judgment —
        // claude-code hands `getWorktreeResult` to the detached task
        // (`AgentTool` returns `async_launched` immediately and must NOT clean
        // up at launch) — so the worker below runs it once the agent reaches a
        // terminal state. Requires the injected manager; without it the
        // worktree is left in place (conservative).
        let agent_worktree = request.worktree.clone();
        let worktree_manager = self.worktree_manager.clone();

        // 5. Drive the subagent to completion inside a runtime-spawned worker
        //    (engine code must not call tokio::spawn directly — D17). The worker
        //    awaits `spawner.spawn`, spools the terminal payload, reports the
        //    terminal status, and removes its own cancel record on exit.
        // Stamp the LAUNCH metadata now, not at termination: a kill lands
        // mid-run, and `tengu_agent_tool_terminated` is emitted by the registry,
        // which knows `killed_by` but nothing about how this agent was spawned.
        // `set_agent_outcome` merges, so this partial never clears a later
        // result. `depth` is already on the spawn request; `is_built_in` is
        // derived from the real builtin catalog rather than a second list.
        self.status_sink
            .set_agent_outcome(
                &task_id,
                platform_api::task_registry::AgentTerminalOutcome {
                    agent_depth: Some(request.depth),
                    is_built_in: Some(
                        agent::builtins::builtin_agent_definitions()
                            .iter()
                            .any(|def| def.agent_type == request.subagent_type),
                    ),
                    ..platform_api::task_registry::AgentTerminalOutcome::default()
                },
            )
            .await;
        let spawner = self.spawner.clone();
        let status_sink = self.status_sink.clone();
        let workers = self.workers.clone();
        let output_manager = self.output_manager.clone();
        let worker_spool_path = spool_path.clone();
        let worker_task_id = task_id.clone();
        let streaming = self.streaming_spawner.clone();
        let persistent_agent_id = Arc::new(StdMutex::new(None));
        let worker_streaming = streaming.clone();
        let worker_persistent_agent_id = persistent_agent_id.clone();
        let agent_ids = self.agent_ids.clone();
        let persistent_worktrees = self.persistent_worktrees.clone();
        let fork_names = self.fork_names.clone();
        let persistent_outcomes = self.persistent_outcomes.clone();
        let parked_store = self.parked_store.clone();
        let (activation_tx, activation_rx) = if status_sink.requires_explicit_activation() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let worker: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
            if is_backgrounded && worker_streaming.is_some() {
                // ── PERSISTENT / resumable path (local_agent "comes to rest"). ──
                // The agent emits ONE Completed per turn-set, then the runner
                // PARKS awaiting the next message (delivered by `send_message` →
                // `StreamingSubagentSpawner::resume`). Each rest appends to the
                // spool and KEEPS the task alive (Completed plus a park, not
                // evicted). Terminal only on Failed / Killed / channel-close.
                let streaming = worker_streaming.expect("is_some checked");
                Box::pin(async move {
                    if let Some(activation_rx) = activation_rx {
                        if activation_rx.await.is_err() {
                            if let (Some(mgr), Some(handle)) = (&worktree_manager, &agent_worktree)
                            {
                                let _ = platform_api::worktree::agent_worktree_result(
                                    mgr.as_ref(),
                                    handle,
                                )
                                .await;
                            }
                            workers.lock().await.remove(&worker_task_id);
                            return;
                        }
                    }
                    status_sink
                        .set_status(&worker_task_id, TaskStatus::Running)
                        .await;
                    let display_effort = request
                        .effort
                        .as_ref()
                        .and_then(|value| value.as_str())
                        .map(str::to_string);
                    let observer: Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver> =
                        Arc::new(AgentIdentityObserver {
                            effort: display_effort,
                            task_id: worker_task_id.clone(),
                            sink: status_sink.clone(),
                            human_epoch,
                        });
                    let started = if request.resumed_history.is_some() {
                        streaming
                            .restore_persistent_with_observer(
                                requested_agent_id,
                                request,
                                inherit,
                                observer,
                            )
                            .await
                    } else {
                        // The registry allocates the public agent id before
                        // entering this handler. Preserve it in the inner
                        // runner so its transcript path, observer events,
                        // task row, and mailbox all address the same agent.
                        streaming
                            .spawn_persistent_with_observer_for_id(
                                requested_agent_id,
                                request,
                                inherit,
                                observer,
                            )
                            .await
                    };
                    let (agent_id, mut rx) = match started {
                        Ok(v) => v,
                        Err(e) => {
                            let ready = human_ready.take();
                            if human_epoch.is_none() {
                                let _ = output_manager
                                    .append(&worker_spool_path, &e.to_string())
                                    .await;
                                status_sink
                                    .set_status(&worker_task_id, TaskStatus::Failed)
                                    .await;
                            }
                            // Terminal (spawn never ran): judge the carried
                            // isolation worktree so it never leaks.
                            if let (Some(mgr), Some(handle)) = (&worktree_manager, &agent_worktree)
                            {
                                let _ = platform_api::worktree::agent_worktree_result(
                                    mgr.as_ref(),
                                    handle,
                                )
                                .await;
                            }
                            workers.lock().await.remove(&worker_task_id);
                            if let Some(ready) = ready {
                                let _ = ready.send(Err(TaskError::Internal(e.to_string())));
                            }
                            return;
                        }
                    };
                    if let Some(ready) = human_ready.take() {
                        let _ = ready.send(Ok(()));
                    }
                    *worker_persistent_agent_id.lock().unwrap() = Some(agent_id);
                    let transcript_path = streaming.transcript_path(agent_id);
                    if let Some(target) = &transcript_path {
                        // The writer may still be creating its first message;
                        // retry at completion after its transcript flush.
                        let _ = output_manager
                            .link_transcript(&worker_spool_path, target)
                            .await;
                    }
                    // Register the live agent id so `send_message` can resume it.
                    agent_ids
                        .lock()
                        .await
                        .insert(worker_task_id.clone(), agent_id);
                    if let Some(handle) = agent_worktree.clone() {
                        persistent_worktrees
                            .lock()
                            .await
                            .insert(worker_task_id.clone(), handle);
                    }
                    if let Some(name) = fork_name.clone() {
                        fork_names.lock().await.insert(worker_task_id.clone(), name);
                    }
                    // The MOST RECENT turn-set's answer + usage. A persistent
                    // agent reports once per rest and then parks; when it finally
                    // terminates, the last rest IS its final response, so the
                    // terminal notification carries it (claude-code's async
                    // lifecycle passes `finalMessage: Vpr(y)` — the accumulated
                    // messages' final text — on every terminal branch, not only
                    // the clean one).
                    let mut outcome = platform_api::task_registry::AgentTerminalOutcome::default();
                    // Published once, after the outcome, below. A channel close
                    // (the runner went away without a terminal event) IS the
                    // completed case, so that is the initial value; the
                    // Failed / Killed arms override it before breaking.
                    let mut terminal_status = TaskStatus::Completed;
                    loop {
                        match rx.recv().await {
                            Some(SubagentEvent::Completed {
                                result,
                                usage,
                                total_tool_use_count,
                                total_duration_ms,
                                ..
                            }) => {
                                if let Some(target) = &transcript_path {
                                    let _ = output_manager
                                        .link_transcript(&worker_spool_path, target)
                                        .await;
                                }
                                let body = serde_json::to_string_pretty(&result)
                                    .unwrap_or_else(|_| result.to_string());
                                let bt = usage.billable_tokens;
                                let total = bt
                                    .input
                                    .saturating_add(bt.cache_write)
                                    .saturating_add(bt.cache_read)
                                    .saturating_add(bt.output);
                                let body = format!(
                                    "{body}\n<usage><total_tokens>{total}</total_tokens></usage>\n"
                                );
                                let _ = output_manager.append(&worker_spool_path, &body).await;
                                // The `<result>` is the agent's final-text response
                                // — binary `wc(ne.content,"\n")`, which the runner
                                // already exposes as the result's `text` field
                                // (`blocks.join("\n")`); NOT the JSON-pretty spool.
                                let rest_result = result
                                    .get("text")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned);
                                let rest_usage = Some(platform_api::task_registry::AgentRunUsage {
                                    subagent_tokens: total,
                                    tool_uses: total_tool_use_count,
                                    duration_ms: total_duration_ms,
                                });
                                // notify_rest atomically publishes Completed plus
                                // a resumable park and the completion payload.
                                // Retain the LAST non-empty answer and its
                                // usage. Guarding both on `is_some` keeps a
                                // later result-less rest from blanking what an
                                // earlier turn-set produced.
                                // The agent is now IDLE with a complete
                                // transcript — the only state a later process
                                // can resume from, so this is where the durable
                                // record is written.
                                if let Some(store) = &parked_store {
                                    store
                                        .park(
                                            &worker_task_id,
                                            agent_id,
                                            &parked_description,
                                            &parked_request,
                                        )
                                        .await;
                                }
                                if rest_result.is_some() {
                                    outcome.result = rest_result.clone();
                                }
                                if rest_usage.is_some() {
                                    outcome.usage = rest_usage.clone();
                                }
                                // Unconditional, unlike the two above: this
                                // accumulator is what a LATER terminal
                                // notification reads, so a clean rest after a
                                // turn-limited one must clear the flag rather
                                // than leave the stale "-turn limit" verb
                                // standing.
                                outcome.max_turns_reached = max_turns_reached_from(&result);
                                persistent_outcomes
                                    .lock()
                                    .await
                                    .insert(worker_task_id.clone(), outcome.clone());
                                // Report the accumulated payload to the registry
                                // BEFORE arming the rest: `RestPayload` carries
                                // only what `notify_rest` is handed, so the rest
                                // notification reads the exhausted turn budget
                                // off the stored outcome. The rest transition
                                // below opens the payload to the drain.
                                status_sink
                                    .set_agent_outcome(&worker_task_id, outcome.clone())
                                    .await;
                                status_sink
                                    .notify_rest(
                                        &worker_task_id,
                                        rest_result,
                                        rest_usage,
                                        Some(agent_id),
                                        parked_request.name.clone(),
                                        parked_request.team_name.clone(),
                                    )
                                    .await;
                            }
                            Some(SubagentEvent::Failed { error, .. }) => {
                                let _ = output_manager.append(&worker_spool_path, &error).await;
                                outcome.error = Some(error);
                                terminal_status = TaskStatus::Failed;
                                break;
                            }
                            Some(SubagentEvent::Killed { .. }) => {
                                terminal_status = TaskStatus::Killed;
                                break;
                            }
                            // Progress / Message: live streaming, not spooled here.
                            Some(_) => {}
                            None => break,
                        }
                    }
                    // Terminal (Failed / Killed / channel-close — NOT a rest):
                    // free the INNER pool runner's slot. `spawn_persistent`
                    // returns only `(agent_id, rx)` with no dealloc owner, so
                    // even a natural channel-close leaves the parked runner
                    // holding its `max_concurrent` slot forever (cap exhaustion)
                    // unless we deallocate here. `stop` is idempotent — an
                    // already-gone slot (channel-close ⇒ runner terminated) is a
                    // no-op. Mirrors `in_process_teammate` kill.
                    let _ = streaming.stop(&agent_id).await;
                    // Then run the worktree keep/cleanup judgment (claude-code
                    // `getWorktreeResult`): keep when dirty/ahead, else remove.
                    // A KEPT worktree's `(path, branch)` fills the notification's
                    // `<worktree>` section, so this runs BEFORE the terminal
                    // status publish.
                    if let (Some(mgr), Some(handle)) = (&worktree_manager, &agent_worktree) {
                        if let Some((path, branch)) =
                            platform_api::worktree::agent_worktree_result(mgr.as_ref(), handle)
                                .await
                        {
                            outcome.worktree_path = Some(path);
                            outcome.worktree_branch = Some(branch);
                        }
                    }
                    // Terminal: erase the durable record. A restore relies on
                    // its ABSENCE to know an agent must not be revived, so this
                    // runs before the terminal status is published.
                    if let Some(store) = &parked_store {
                        store.unpark(agent_id).await;
                    }
                    persistent_worktrees.lock().await.remove(&worker_task_id);
                    persistent_outcomes.lock().await.remove(&worker_task_id);
                    // Payload BEFORE status — the drain is terminal-gated (see
                    // the sync branch below for the full note).
                    status_sink
                        .set_agent_outcome(&worker_task_id, outcome)
                        .await;
                    status_sink
                        .set_status(&worker_task_id, terminal_status)
                        .await;
                    agent_ids.lock().await.remove(&worker_task_id);
                    fork_names.lock().await.remove(&worker_task_id);
                    workers.lock().await.remove(&worker_task_id);
                })
            } else {
                Box::pin(async move {
                    if let Some(activation_rx) = activation_rx {
                        if activation_rx.await.is_err() {
                            if let (Some(mgr), Some(handle)) = (&worktree_manager, &agent_worktree)
                            {
                                let _ = platform_api::worktree::agent_worktree_result(
                                    mgr.as_ref(),
                                    handle,
                                )
                                .await;
                            }
                            workers.lock().await.remove(&worker_task_id);
                            return;
                        }
                    }
                    status_sink
                        .set_status(&worker_task_id, TaskStatus::Running)
                        .await;

                    let result = spawner.spawn(request, inherit).await;

                    // Map the terminal SubagentResult onto a spool payload + status.
                    // Spool I/O is best-effort — a spool write failure must not mask
                    // the subagent result (mirrors local_bash). The shared
                    // `TaskStatusSink` has no token-usage method (it is defined in
                    // `local_bash`, which this handler must not modify), so token usage
                    // is surfaced by spooling a `<usage><total_tokens>…` footer —
                    // byte-aligned with the TS `registerAsyncAgent` notification shape.
                    let (payload, status) = match &result {
                        Ok(SubagentResult::Completed { content, usage, .. }) => {
                            // Pretty-print the JSON payload; fall back to the compact
                            // Display form if serialization somehow fails.
                            let body = serde_json::to_string_pretty(content)
                                .unwrap_or_else(|_| content.to_string());
                            let body = format!(
                                "{body}\n<usage><total_tokens>{}</total_tokens></usage>\n",
                                usage.total_tokens
                            );
                            (body, TaskStatus::Completed)
                        }
                        Ok(SubagentResult::Failed { reason, .. }) => {
                            (reason.clone(), TaskStatus::Failed)
                        }
                        Ok(SubagentResult::Killed { .. }) => (String::new(), TaskStatus::Killed),
                        Err(e) => (e.to_string(), TaskStatus::Failed),
                    };

                    // The notification payload claude-code hands to
                    // `enqueueAgentNotification` alongside the status:
                    // `finalMessage` (the final TEXT — `wc(content,"\n")`, not
                    // the JSON-pretty spool body), the
                    // `{totalTokens,toolUses,durationMs}` usage object, and the
                    // failure reason. The spool has always carried the answer;
                    // nothing carried it into the notification, so the model was
                    // told a background agent finished without being told what
                    // it found.
                    let mut outcome = platform_api::task_registry::AgentTerminalOutcome::default();
                    match &result {
                        Ok(SubagentResult::Completed {
                            content,
                            total_tool_use_count,
                            total_duration_ms,
                            total_tokens,
                            ..
                        }) => {
                            let text = crate::handle::extract_text_content(content);
                            // Claude gates the section on a TRUTHY finalMessage
                            // (`s ? "<result>…" : ""`), so an empty answer omits
                            // `<result>` rather than rendering an empty one.
                            outcome.result = (!text.is_empty()).then_some(text);
                            outcome.usage = Some(platform_api::task_registry::AgentRunUsage {
                                subagent_tokens: *total_tokens,
                                tool_uses: *total_tool_use_count,
                                duration_ms: *total_duration_ms,
                            });
                            // A run that fell through its turn budget completes
                            // with a partial answer; claude-code says so in the
                            // summary rather than reporting it as "finished".
                            outcome.max_turns_reached = max_turns_reached_from(content);
                        }
                        Ok(SubagentResult::Failed { reason, .. }) => {
                            outcome.error = Some(reason.clone());
                        }
                        Ok(SubagentResult::Killed { .. }) => {}
                        Err(e) => outcome.error = Some(e.to_string()),
                    }

                    // Routed through the output manager's `append` so the per-file 5GB
                    // disk cap is enforced (T17). The write uses O_NOFOLLOW (claude-code
                    // `diskOutput.ts`) so a symlink planted at the spool path from inside
                    // the sandbox cannot redirect the write (T18).
                    if !payload.is_empty() {
                        let _ = output_manager.append(&worker_spool_path, &payload).await;
                    }

                    // Terminal: run the worktree keep/cleanup judgment on the
                    // carried isolation worktree (claude-code `getWorktreeResult`
                    // — keep when dirty/ahead, else auto-remove). Runs for ANY
                    // outcome so a worktree never leaks on a failed/killed agent.
                    // A KEPT worktree's `(path, branch)` is the binary's
                    // `...await getWorktreeResult()` spread — it gates and fills
                    // the notification's `<worktree>` section, so the judgment
                    // must run BEFORE the status publish (claude awaits the same
                    // closure before calling `enqueueAgentNotification`), not
                    // after it as a fire-and-forget cleanup.
                    if let (Some(mgr), Some(handle)) = (&worktree_manager, &agent_worktree) {
                        if let Some((path, branch)) =
                            platform_api::worktree::agent_worktree_result(mgr.as_ref(), handle)
                                .await
                        {
                            outcome.worktree_path = Some(path);
                            outcome.worktree_branch = Some(branch);
                        }
                    }

                    // Payload BEFORE status: the registry's notification drain is
                    // gated on terminal-and-not-notified, so publishing the
                    // terminal status first opens a window in which the drain
                    // renders this completion with none of its optional
                    // sections. Claude-code has no such window — status and
                    // payload reach `enqueueAgentNotification` in one call.
                    status_sink
                        .set_agent_outcome(&worker_task_id, outcome)
                        .await;
                    status_sink.set_status(&worker_task_id, status).await;

                    // The subagent has terminated; drop the cancel record so a late
                    // kill is a graceful no-op (claude-code `status !== 'running'`).
                    workers.lock().await.remove(&worker_task_id);
                })
            };

        // Hold the `workers` lock ACROSS spawn + insert. The worker's self-remove
        // (`workers.lock().await.remove`) contends the same lock, so a
        // fast-completing worker cannot run its remove BEFORE we insert — which
        // would otherwise leave a stale record the cleanup closure moves to
        // `pending_kill`, letting `drain_pending_kills` flip an
        // already-Completed task to Killed. `RuntimeSpawner::spawn` only
        // schedules the worker (it does not await its completion), so holding
        // the lock here cannot deadlock.
        let mut workers = self.workers.lock().await;
        let bg_handle = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;

        // Record the worker-cancel handle (+ the runtime that minted it) so
        // kill / drain can cancel the in-flight worker without a fresh ctx.
        let cleanup_generation = bg_handle.task_id;
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle: bg_handle,
                runtime: ctx.runtime.clone(),
                persistent_teardown: streaming.as_ref().map(|_| PersistentTeardown {
                    agent_id: persistent_agent_id.clone(),
                }),
            },
        );
        drop(workers);

        // 6. Build the synchronous cleanup seam (claude-code `registerCleanup`
        //    parity). The closure cannot await, so it moves any live cancel
        //    record into `pending_kill`; the async `drain_pending_kills`
        //    (called by the registry on agent teardown) performs the real
        //    `RuntimeSpawner::cancel`. Authoritative cancellation also flows
        //    through `Task::kill`.
        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            cleanup_pending
                .lock()
                .unwrap()
                .push((cleanup_task_id.clone(), cleanup_generation));
        });

        let handle = TaskHandle::new(task_id, Some(cleanup));
        Ok(match activation_tx {
            Some(activation_tx) => handle.with_activation(move || {
                let _ = activation_tx.send(());
            }),
            None => handle,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    mod human_resume_test {
        include!("human_resume_test.rs");
    }
    use super::*;
    use crate::state::TaskStatus;
    use platform_api::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
    use platform_api::tool_invoker::{SubagentInvocationContext, ToolInvoker, ToolInvokerError};
    use platform_api::{BudgetError, SubagentSpawnError, SubagentUsage};
    use serde_json::json;
    use std::any::Any;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;
    use test_harness::mocks::MockRuntimeSpawner;
    use tokio::sync::Mutex as TokioMutex;

    // ---- In-memory FileSystem (mirrors local_bash test fixture) ------------

    struct InMemoryFs {
        files: TokioMutex<StdHashMap<String, String>>,
        corrupt_observer_markers: std::sync::atomic::AtomicBool,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(StdHashMap::new()),
                corrupt_observer_markers: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }
    #[async_trait]
    impl FileSystem for InMemoryFs {
        async fn read_file(
            &self,
            path: &str,
            offset: Option<u64>,
            limit: Option<u64>,
        ) -> Result<FileContent, FsError> {
            let map = self.files.lock().await;
            let content = map.get(path).cloned().unwrap_or_default();
            let off = usize::try_from(offset.unwrap_or(0)).unwrap_or(usize::MAX);
            let body: String = content.chars().skip(off).collect();
            let truncated = limit.is_some_and(|lim| body.len() as u64 > lim);
            let trimmed = match limit {
                Some(lim) => body
                    .chars()
                    .take(usize::try_from(lim).unwrap_or(usize::MAX))
                    .collect(),
                None => body,
            };
            let total_lines = content.lines().count() as u64;
            Ok(FileContent {
                content: trimmed,
                truncated,
                total_lines,
            })
        }
        async fn write_file(&self, path: &str, body: &str) -> Result<(), FsError> {
            let body = if path.ends_with(".observer.json")
                && self
                    .corrupt_observer_markers
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                "{}"
            } else {
                body
            };
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
            let mut map = self.files.lock().await;
            map.entry(path.to_string()).or_default().push_str(body);
            Ok(())
        }
        async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
            let mut map = self.files.lock().await;
            if let Some(s) = map.get_mut(path) {
                s.truncate(usize::try_from(len).unwrap_or(usize::MAX));
            }
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

    // ---- Mock SubagentSpawner (returns a canned result; records inputs) ----

    enum CannedResult {
        Completed(serde_json::Value, u64),
        Failed(String),
        Killed,
        Err(String),
        /// Never resolves — used to keep the worker alive for the kill test.
        Pending,
    }

    struct MockSpawner {
        canned: StdMutex<Option<CannedResult>>,
        seen_request: StdMutex<Option<SubagentSpawnRequest>>,
        // Pointer identity of the inherited Arcs, captured for ptr_eq checks.
        seen_invoker: StdMutex<Option<Arc<dyn ToolInvoker>>>,
        seen_budget: StdMutex<Option<Arc<dyn BudgetEnforcerHandle>>>,
    }
    impl MockSpawner {
        fn new(canned: CannedResult) -> Arc<Self> {
            Arc::new(Self {
                canned: StdMutex::new(Some(canned)),
                seen_request: StdMutex::new(None),
                seen_invoker: StdMutex::new(None),
                seen_budget: StdMutex::new(None),
            })
        }
        fn request(&self) -> Option<SubagentSpawnRequest> {
            self.seen_request.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl SubagentSpawner for MockSpawner {
        async fn spawn(
            &self,
            request: SubagentSpawnRequest,
            inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            *self.seen_request.lock().unwrap() = Some(request);
            *self.seen_invoker.lock().unwrap() = Some(inherit.tool_invoker.clone());
            *self.seen_budget.lock().unwrap() = Some(inherit.budget.clone());
            // Take the canned value out and DROP the guard before any await:
            // a `std::sync::MutexGuard` held across `.await` makes the future
            // `!Send`, but `SubagentSpawner::spawn` must return a `Send` future.
            let canned = self.canned.lock().unwrap().take();
            match canned {
                Some(CannedResult::Completed(content, total_tokens)) => {
                    Ok(SubagentResult::Completed {
                        agent_id: protocol::AgentId::new(),
                        content,
                        usage: SubagentUsage {
                            total_tokens,
                            ..Default::default()
                        },
                        total_tool_use_count: 0,
                        total_duration_ms: 0,
                        total_tokens,
                        assistant_message_count: 0,
                        response_char_count: 0,
                        last_request_id: None,
                        cumulative_usage: SubagentUsage::default(),
                        usage_complete: true,
                    })
                }
                Some(CannedResult::Failed(reason)) => Ok(SubagentResult::Failed {
                    agent_id: protocol::AgentId::new(),
                    reason,
                    usage: platform_api::subagent_spawn::SubagentUsage::default(),
                }),
                Some(CannedResult::Killed) => Ok(SubagentResult::Killed {
                    agent_id: protocol::AgentId::new(),
                }),
                Some(CannedResult::Err(msg)) => Err(SubagentSpawnError::Runtime(msg)),
                Some(CannedResult::Pending) | None => {
                    // Park forever — the test cancels via Task::kill.
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
        }
    }

    // ---- Mock ToolInvoker / BudgetEnforcerHandle (inert) -------------------

    struct MockInvoker;
    #[async_trait]
    impl ToolInvoker for MockInvoker {
        async fn invoke(
            &self,
            _name: &str,
            _input: serde_json::Value,
            _ctx: SubagentInvocationContext,
        ) -> Result<serde_json::Value, ToolInvokerError> {
            Ok(json!(null))
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    struct MockBudget;
    #[async_trait]
    impl BudgetEnforcerHandle for MockBudget {
        async fn check_and_charge(&self, _nano_usd: u64) -> Result<(), BudgetError> {
            Ok(())
        }
        async fn snapshot_total_nano_usd(&self) -> u64 {
            0
        }
    }

    // ---- Mock WorktreeManager (terminal keep/cleanup judgment) --------------

    /// Judgment-only mock: scripted change summary + a removal recorder.
    /// `create_worktree` is unreachable here — the handler never CREATES
    /// worktrees (AgentTool does, before dispatch); it only judges the one
    /// carried on `SubagentSpawnRequest::worktree`.
    struct RecordingWorktree {
        summary: Option<platform_api::worktree::WorktreeChangeSummary>,
        removed: StdMutex<Vec<platform_api::worktree::WorktreeHandle>>,
    }
    impl RecordingWorktree {
        fn new(summary: Option<platform_api::worktree::WorktreeChangeSummary>) -> Arc<Self> {
            Arc::new(Self {
                summary,
                removed: StdMutex::new(Vec::new()),
            })
        }
        fn removed_count(&self) -> usize {
            self.removed.lock().unwrap().len()
        }
    }
    #[async_trait]
    impl platform_api::worktree::WorktreeManager for RecordingWorktree {
        async fn create_worktree(
            &self,
            _slug: &str,
            _base_branch: Option<&str>,
            _copy_includes: &[PathBuf],
        ) -> Result<platform_api::worktree::WorktreeHandle, platform_api::worktree::WorktreeError>
        {
            Err(platform_api::worktree::WorktreeError::Unsupported)
        }
        async fn remove_worktree(
            &self,
            handle: &platform_api::worktree::WorktreeHandle,
        ) -> Result<(), platform_api::worktree::WorktreeError> {
            self.removed.lock().unwrap().push(handle.clone());
            Ok(())
        }
        async fn list_worktrees(
            &self,
        ) -> Result<Vec<platform_api::worktree::WorktreeInfo>, platform_api::worktree::WorktreeError>
        {
            Ok(Vec::new())
        }
        async fn cleanup_stale(
            &self,
            _max_age: std::time::Duration,
        ) -> Result<Vec<PathBuf>, platform_api::worktree::WorktreeError> {
            Ok(Vec::new())
        }
        fn is_supported(&self) -> bool {
            true
        }
        async fn worktree_change_summary(
            &self,
            _handle: &platform_api::worktree::WorktreeHandle,
        ) -> Result<
            Option<platform_api::worktree::WorktreeChangeSummary>,
            platform_api::worktree::WorktreeError,
        > {
            Ok(self.summary)
        }
    }

    #[derive(Default)]
    struct RecordingParkedStore {
        parked: StdMutex<Vec<(String, AgentId, String)>>,
        unparked: StdMutex<Vec<AgentId>>,
    }
    #[async_trait]
    impl platform_api::parked_agent_store::ParkedAgentStore for RecordingParkedStore {
        async fn park(
            &self,
            task_id: &str,
            agent_id: AgentId,
            description: &str,
            _request: &SubagentSpawnRequest,
        ) {
            self.parked.lock().unwrap().push((
                task_id.to_string(),
                agent_id,
                description.to_string(),
            ));
        }

        async fn unpark(&self, agent_id: AgentId) {
            self.unparked.lock().unwrap().push(agent_id);
        }
    }

    fn isolation_worktree_handle() -> platform_api::worktree::WorktreeHandle {
        platform_api::worktree::WorktreeHandle {
            path: PathBuf::from("/repo/.lingxi/worktrees/agent-1"),
            branch_name: "worktree-agent-1".into(),
            base_commit: None,
        }
    }

    /// A background Agent request carrying the isolation worktree the tool
    /// resolved before dispatch (the P1-01 ownership transfer).
    fn request_with_worktree(prompt: &str) -> SubagentSpawnRequest {
        SubagentSpawnRequest {
            teammate_color: None,
            subagent_type: "general-purpose".into(),
            prompt: prompt.into(),
            observer: None,
            context_paths: Vec::new(),
            description: None,
            model: None,
            model_profile: None,
            run_in_background: true,
            name: None,
            team_name: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            mode: None,
            isolation: Some("worktree".into()),
            cwd: Some("/repo/.lingxi/worktrees/agent-1".into()),
            worktree: Some(isolation_worktree_handle()),
            fork_context_messages: None,
            fork_parent_system_prompt: None,
            schema: None,
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: None,
            tool_use_id: None,
            system_prompt_override: None,
            system_prompt_addendum: None,
            additional_disallowed_tools: Vec::new(),
            depth: 1,
            origin_session_id: None,
            parent_model_override: None,
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
        }
    }

    fn input_with_worktree(prompt: &str) -> TaskSpawnInput {
        TaskSpawnInput::LocalAgent {
            agent_id: protocol::AgentId::new(),
            subagent_type: "general-purpose".into(),
            prompt: prompt.into(),
            is_backgrounded: true,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            spawn_request: Some(request_with_worktree(prompt)),
            inheritance: None,
        }
    }

    // ---- Recording status sink ---------------------------------------------

    #[derive(Default)]
    struct RecordingSink {
        parked: StdMutex<bool>,
        statuses: StdMutex<Vec<(String, TaskStatus)>>,
        explicit_activation: bool,
        rest_count: StdMutex<usize>,
        last_rest: StdMutex<
            Option<(
                Option<String>,
                Option<platform_api::task_registry::AgentRunUsage>,
                Option<protocol::AgentId>,
                Option<String>,
                Option<String>,
            )>,
        >,
        /// The terminal notification payload, and the call ORDER relative to the
        /// terminal `set_status` — the drain is terminal-gated, so the payload
        /// must land first.
        outcome: StdMutex<Option<platform_api::task_registry::AgentTerminalOutcome>>,
        calls: StdMutex<Vec<&'static str>>,
    }
    #[async_trait]
    impl TaskStatusSink for RecordingSink {
        fn requires_explicit_activation(&self) -> bool {
            self.explicit_activation
        }

        async fn set_status(&self, task_id: &str, status: TaskStatus) {
            *self.parked.lock().unwrap() = false;
            if status.is_terminal() {
                self.calls.lock().unwrap().push("status");
            }
            self.statuses
                .lock()
                .unwrap()
                .push((task_id.to_string(), status));
        }
        async fn set_agent_outcome(
            &self,
            _task_id: &str,
            outcome: platform_api::task_registry::AgentTerminalOutcome,
        ) {
            self.calls.lock().unwrap().push("outcome");
            *self.outcome.lock().unwrap() = Some(outcome);
        }
        async fn notify_rest(
            &self,
            task_id: &str,
            result: Option<String>,
            usage: Option<platform_api::task_registry::AgentRunUsage>,
            agent_id: Option<protocol::AgentId>,
            agent_name: Option<String>,
            team_name: Option<String>,
        ) {
            *self.parked.lock().unwrap() = true;
            self.statuses
                .lock()
                .unwrap()
                .push((task_id.into(), TaskStatus::Completed));
            *self.rest_count.lock().unwrap() += 1;
            *self.last_rest.lock().unwrap() =
                Some((result, usage, agent_id, agent_name, team_name));
        }
        async fn is_terminal(&self, task_id: &str) -> bool {
            !*self.parked.lock().unwrap()
                && self
                    .statuses
                    .lock()
                    .unwrap()
                    .iter()
                    .rev()
                    .find(|(id, _)| id == task_id)
                    .is_some_and(|(_, s)| s.is_terminal())
        }
    }
    impl RecordingSink {
        fn last_status(&self) -> Option<TaskStatus> {
            self.statuses.lock().unwrap().last().map(|(_, s)| *s)
        }
        fn rest_count(&self) -> usize {
            *self.rest_count.lock().unwrap()
        }
        fn outcome(&self) -> platform_api::task_registry::AgentTerminalOutcome {
            self.outcome.lock().unwrap().clone().unwrap_or_default()
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
    }

    // ---- Helpers ------------------------------------------------------------

    fn make_ctx(fs: Arc<dyn FileSystem>) -> TaskContext {
        TaskContext {
            fs,
            runtime: Arc::new(MockRuntimeSpawner::default()),
        }
    }

    fn make_output_manager(fs: Arc<dyn FileSystem>) -> (tempfile::TempDir, Arc<TaskOutputManager>) {
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(PathBuf::from(dir.path()), fs));
        (dir, mgr)
    }

    fn make_handler(
        spawner: Arc<dyn SubagentSpawner>,
        mgr: Arc<TaskOutputManager>,
        sink: Arc<dyn TaskStatusSink>,
    ) -> LocalAgentHandler {
        LocalAgentHandler::new(spawner, Arc::new(MockInvoker), Arc::new(MockBudget), mgr)
            .with_status_sink(sink)
    }

    fn local_agent_input(prompt: &str) -> TaskSpawnInput {
        TaskSpawnInput::LocalAgent {
            agent_id: protocol::AgentId::new(),
            subagent_type: "general-purpose".into(),
            prompt: prompt.into(),
            is_backgrounded: true,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            spawn_request: None,
            inheritance: None,
        }
    }

    /// Poll the sink until it reports a terminal status (the worker runs on the
    /// `MockRuntimeSpawner`'s tokio task, so yields let it finish deterministically).
    async fn await_terminal(sink: &Arc<RecordingSink>) -> TaskStatus {
        for _ in 0..200 {
            if let Some(s) = sink.last_status() {
                if s.is_terminal() && !*sink.parked.lock().unwrap() {
                    return s;
                }
            }
            tokio::task::yield_now().await;
        }
        sink.last_status().expect("worker never reported a status")
    }

    // ---- Mock StreamingSubagentSpawner (persistent / resume seam) -----------

    /// A persistent-spawner mock whose outbound event channel the TEST drives:
    /// `spawn_persistent` stashes the `Sender` in `tx_slot` (so the test can
    /// push per-turn-set `Completed`/`Failed`/… events), and `resume` bumps a
    /// counter the test asserts on.
    struct MockStreamingSpawner {
        transcript: StdMutex<Option<PathBuf>>,
        requests: StdMutex<Vec<SubagentSpawnRequest>>,
        delivered: StdMutex<Vec<String>>,
        tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>>,
        resume_count: Arc<std::sync::atomic::AtomicUsize>,
        // Records the agent id passed to `stop` (the inner-runner dealloc), so a
        // test can assert kill / terminal actually frees the pool slot.
        stopped: Arc<StdMutex<Vec<AgentId>>>,
        // The id `spawn_persistent` handed back, so a test can match it to `stop`.
        spawned_id: Arc<StdMutex<Option<AgentId>>>,
    }
    impl MockStreamingSpawner {
        fn new(
            tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>>,
            resume_count: Arc<std::sync::atomic::AtomicUsize>,
        ) -> Arc<Self> {
            Arc::new(Self {
                transcript: StdMutex::new(None),
                requests: StdMutex::new(Vec::new()),
                delivered: StdMutex::new(Vec::new()),
                tx_slot,
                resume_count,
                stopped: Arc::new(StdMutex::new(Vec::new())),
                spawned_id: Arc::new(StdMutex::new(None)),
            })
        }
    }
    #[async_trait]
    impl StreamingSubagentSpawner for MockStreamingSpawner {
        async fn spawn_persistent_with_observer(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
            observer: Arc<dyn platform_api::subagent_spawn::SubagentSpawnObserver>,
        ) -> Result<(AgentId, tokio::sync::mpsc::Receiver<SubagentEvent>), SubagentSpawnError>
        {
            let id = AgentId::new();
            let event = platform_api::subagent_spawn::SubagentObservation::Allocated {
                agent_id: id,
                agent_type: request.subagent_type.clone(),
                name: request.name.clone(),
                model: "mock".into(),
                model_profile: None,
                persistent: true,
                initial_message_index: 0,
                origin_session_id: None,
            };
            observer.on_allocated(&event);
            observer.before_start(&event).await?;
            self.requests.lock().unwrap().push(request);
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            *self.spawned_id.lock().unwrap() = Some(id);
            *self.tx_slot.lock().unwrap() = Some(tx);
            Ok((id, rx))
        }
        fn transcript_path(&self, _agent_id: AgentId) -> Option<PathBuf> {
            self.transcript.lock().unwrap().clone()
        }
        async fn spawn_persistent(
            &self,
            request: SubagentSpawnRequest,
            _inherit: SubagentInheritance,
        ) -> Result<(AgentId, tokio::sync::mpsc::Receiver<SubagentEvent>), SubagentSpawnError>
        {
            self.requests.lock().unwrap().push(request);
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            *self.tx_slot.lock().unwrap() = Some(tx);
            let id = AgentId::new();
            *self.spawned_id.lock().unwrap() = Some(id);
            Ok((id, rx))
        }
        async fn resume(
            &self,
            _agent_id: &AgentId,
            message: String,
        ) -> Result<(), SubagentSpawnError> {
            self.delivered.lock().unwrap().push(message);
            self.resume_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        async fn stop(&self, agent_id: &AgentId) -> Result<(), SubagentSpawnError> {
            self.stopped.lock().unwrap().push(*agent_id);
            Ok(())
        }
    }

    fn completed_event(marker: &str) -> SubagentEvent {
        SubagentEvent::Completed {
            agent_id: AgentId::new(),
            result: json!({ "marker": marker }),
            usage: llm_runtime::Usage::default(),
            total_tool_use_count: 0,
            total_duration_ms: 0,
            assistant_message_count: 0,
            last_request_id: None,
            cumulative_usage: llm_runtime::Usage::default(),
            usage_complete: true,
        }
    }

    async fn await_spool_contains(
        mgr: &Arc<TaskOutputManager>,
        path: &std::path::Path,
        needle: &str,
    ) {
        for _ in 0..200 {
            if let Ok(read) = mgr
                .read(path, crate::output_manager::OutputOptions::default())
                .await
            {
                if read.content.contains(needle) {
                    return;
                }
            }
            tokio::task::yield_now().await;
        }
        panic!("spool never contained {needle:?}");
    }

    // ---- Tests --------------------------------------------------------------

    #[tokio::test]
    async fn real_background_pool_identity_routes_nested_completion_to_its_parent() {
        real_background_identity_probe(false).await;
    }

    #[tokio::test]
    async fn real_restored_pool_preserves_owner_id_and_old_task_alias_before_child_completion() {
        real_background_identity_probe(true).await;
    }

    async fn real_background_identity_probe(restored: bool) {
        use platform_api::subagent_spawn::{SubagentObservation, SubagentSpawnObserver};
        #[derive(Default)]
        struct Allocations(StdMutex<HashMap<String, AgentId>>);
        #[async_trait]
        impl SubagentSpawnObserver for Allocations {
            fn on_allocated(&self, event: &SubagentObservation) {
                if let SubagentObservation::Allocated {
                    agent_id,
                    name: Some(name),
                    ..
                } = event
                {
                    self.0.lock().unwrap().insert(name.clone(), *agent_id);
                }
            }
            async fn on_event(&self, _: SubagentObservation) {}
        }
        struct Api {
            registry: std::sync::OnceLock<std::sync::Weak<crate::registry::TaskRegistry>>,
            allocations: Arc<Allocations>,
            mcp_ids: Arc<StdMutex<Vec<AgentId>>>,
            parent_calls: std::sync::atomic::AtomicUsize,
            expected_restored_id: Option<AgentId>,
            release_child: tokio::sync::Notify,
            child_task: StdMutex<Option<String>>,
        }
        fn request(prompt: &str, description: &str) -> SubagentSpawnRequest {
            SubagentSpawnRequest {
                subagent_type: "general-purpose".into(),
                prompt: prompt.into(),
                description: Some(description.into()),
                run_in_background: true,
                ..Default::default()
            }
        }
        fn input(request: SubagentSpawnRequest, creator: Option<AgentId>) -> TaskSpawnInput {
            TaskSpawnInput::LocalAgent {
                agent_id: AgentId::new(),
                subagent_type: request.subagent_type.clone(),
                prompt: request.prompt.clone(),
                is_backgrounded: true,
                tool_use_id: None,
                creator_teammate_name: None,
                creator_team_name: None,
                creator_agent_id: creator,
                spawn_request: Some(request),
                inheritance: None,
            }
        }
        #[async_trait]
        impl agent::SubagentApiClient for Api {
            async fn messages_create(
                &self,
                _: &str,
                _: Option<&str>,
                messages: Vec<protocol::ConversationMessage>,
                _: Vec<serde_json::Value>,
            ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
                let registry = self.registry.get().unwrap().upgrade().unwrap();
                let history = serde_json::to_string(&messages).unwrap();
                let child = history.contains("nested child prompt");
                let label = if child { "nested child" } else { "real parent" };
                let actual = *self
                    .allocations
                    .0
                    .lock()
                    .unwrap()
                    .get(label)
                    .expect("allocation precedes model");
                assert!(
                    self.mcp_ids.lock().unwrap().contains(&actual),
                    "MCP builder must see the actual owner ID before the first model request"
                );
                let row = registry
                    .get(&actual.to_string())
                    .await
                    .expect("actual pool id must resolve before the first model request");
                assert!(
                    matches!(row, crate::state::TaskState::LocalAgent(ref agent) if agent.agent_id == actual)
                );
                if !child {
                    if let Some(expected) = self.expected_restored_id {
                        assert_eq!(actual, expected, "restore must allocate the persisted runner ID, not merely add an alias");
                        let old = registry
                            .get("arestoredold")
                            .await
                            .expect("old task alias must resolve before the first model request");
                        assert!(
                            matches!(old, crate::state::TaskState::LocalAgent(ref agent) if agent.agent_id == expected)
                        );
                        assert!(!history.contains("original prompt must not replay"));
                    }
                }
                let text = if child {
                    self.release_child.notified().await;
                    "nested child answer"
                } else if self
                    .parent_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    let mut req = request("nested child prompt", "nested child");
                    req.creator_agent_id = Some(actual);
                    let id = registry
                        .spawn(
                            TaskType::LocalAgent,
                            input(req, Some(actual)),
                            "nested child".into(),
                        )
                        .await
                        .unwrap();
                    *self.child_task.lock().unwrap() = Some(id);
                    "waiting for child"
                } else {
                    assert!(
                        history.contains("nested child answer"),
                        "the real owner's resumed model must receive its child's result"
                    );
                    "parent folded child"
                };
                Ok(llm_runtime::HistoryResponse {
                    id: "response".into(),
                    model: "mock".into(),
                    content: vec![llm_runtime::ContentBlock::Text {
                        text: text.into(),
                        cache_control: None,
                    }],
                    stop_reason: Some("end_turn".into()),
                    stop_details: None,
                    usage: llm_runtime::Usage::default(),
                    cost: None,
                    provider_metadata: serde_json::Value::Null,
                })
            }
        }
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let allocations = Arc::new(Allocations::default());
        let mcp_ids = Arc::new(StdMutex::new(Vec::new()));
        let mcp_cleanup_ids = Arc::new(StdMutex::new(Vec::new()));
        let seen_mcp = mcp_ids.clone();
        let cleaned_mcp = mcp_cleanup_ids.clone();
        let mcp_builder: agent::agent_mcp_tools::AgentMcpToolBuilder =
            Arc::new(move |id, _, _lease| {
                seen_mcp.lock().unwrap().push(id);
                let cleaned = cleaned_mcp.clone();
                Box::pin(async move {
                    agent::agent_mcp_tools::AgentMcpToolSet {
                        tools: vec![],
                        cleanups: vec![agent::agent_mcp_tools::AgentMcpCleanupHandle {
                            server_name: "owner-identity".into(),
                            run: Arc::new(move || {
                                let cleaned = cleaned.clone();
                                Box::pin(async move {
                                    cleaned.lock().unwrap().push(id);
                                    Ok(())
                                })
                            }),
                        }],
                    }
                })
            });
        let restored_id = restored.then(AgentId::new);
        let api = Arc::new(Api {
            mcp_ids: mcp_ids.clone(),
            expected_restored_id: restored_id,
            registry: std::sync::OnceLock::new(),
            allocations: allocations.clone(),
            parent_calls: std::sync::atomic::AtomicUsize::new(0),
            release_child: tokio::sync::Notify::new(),
            child_task: StdMutex::new(None),
        });
        let pool = Arc::new(agent::StateMachinePool::new(runtime.clone(), 4));
        let spawner = Arc::new(
            agent::PoolSubagentSpawner::new(pool)
                .with_api_client(api.clone())
                .with_spawn_observer(allocations)
                .with_mcp_tool_builder(mcp_builder),
        );
        let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
        let handler = LocalAgentHandler::new(
            spawner.clone(),
            Arc::new(MockInvoker),
            Arc::new(MockBudget),
            mgr.clone(),
        )
        .with_status_sink(sink.clone())
        .with_streaming_spawner(spawner.clone());
        let mut registry = crate::registry::TaskRegistry::new(runtime, fs, mgr);
        registry.register_handler(TaskType::LocalAgent, Arc::new(handler));
        let registry = Arc::new(registry);
        sink.bind(registry.clone());
        spawner.set_task_registry(registry.clone());
        api.registry.set(Arc::downgrade(&registry)).ok().unwrap();
        let mut parent_request = request("original prompt must not replay", "real parent");
        if restored {
            parent_request.resumed_history = Some(vec![protocol::ConversationMessage::user(
                protocol::MessageId::new(),
                "recovered parent history".into(),
            )]);
        }
        let mut parent_input = input(parent_request, None);
        if let (Some(old), TaskSpawnInput::LocalAgent { agent_id, .. }) =
            (restored_id, &mut parent_input)
        {
            *agent_id = old;
        }
        let aliases = if restored {
            vec!["arestoredold".to_string()]
        } else {
            Vec::new()
        };
        let parent = registry
            .spawn_with_aliases(
                TaskType::LocalAgent,
                parent_input,
                "real parent".into(),
                &aliases,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if registry.get(&parent).await.unwrap().is_parked() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("parent parks while its child is in flight");
        assert!(
            registry.take_pending_task_notifications().await.is_empty(),
            "main cannot steal the child's result or the deferred parent rest"
        );
        if let Some(old) = restored_id {
            let mut duplicate = request("", "duplicate restore");
            duplicate.resumed_history = Some(vec![protocol::ConversationMessage::user(
                protocol::MessageId::new(),
                "duplicate history".into(),
            )]);
            let mut duplicate_input = input(duplicate, None);
            if let TaskSpawnInput::LocalAgent { agent_id, .. } = &mut duplicate_input {
                *agent_id = old;
            }
            assert!(registry
                .spawn_with_aliases(
                    TaskType::LocalAgent,
                    duplicate_input,
                    "duplicate restore".into(),
                    &["aduplicaterestore".into()]
                )
                .await
                .is_err());
            assert_eq!(
                registry.resolve_task_id(&old.to_string()).await.as_deref(),
                Some(parent.as_str()),
                "rejected restore cannot steal the live agent alias"
            );
            assert_eq!(
                registry.resolve_task_id("arestoredold").await.as_deref(),
                Some(parent.as_str())
            );
        }
        api.release_child.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(crate::state::TaskState::LocalAgent(agent)) =
                    registry.get(&parent).await
                {
                    if agent.is_parked
                        && agent.outcome.result.as_deref() == Some("parent folded child")
                    {
                        break;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nested completion wakes and reaches the actual allocated parent");
        assert_eq!(
            api.parent_calls.load(std::sync::atomic::Ordering::SeqCst),
            2
        );
        let notifications = registry.take_pending_task_notifications().await;
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].task_id, parent);
        assert_eq!(
            notifications[0].result.as_deref(),
            Some("parent folded child")
        );
        registry.kill_with_reason(&parent, "user").await.unwrap();
        let child = api.child_task.lock().unwrap().clone().unwrap();
        registry.kill_with_reason(&child, "user").await.unwrap();
        if let Some(old) = restored_id {
            assert_eq!(
                mcp_ids
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|id| **id == old)
                    .count(),
                1
            );
            assert_eq!(
                mcp_cleanup_ids
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|id| **id == old)
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn observer_sidecar_queues_activity_until_rest_without_notifying_owner() {
        use platform_api::task_registry::TaskRegistryHandle;
        let memory_fs = Arc::new(InMemoryFs::new());
        let fs: Arc<dyn FileSystem> = memory_fs.clone();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
        let tx_slot = Arc::new(StdMutex::new(None));
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming = MockStreamingSpawner::new(tx_slot.clone(), count.clone());
        let handler = make_handler(
            MockSpawner::new(CannedResult::Pending),
            mgr.clone(),
            sink.clone(),
        )
        .with_streaming_spawner(streaming.clone());
        let mut registry = crate::registry::TaskRegistry::new(
            Arc::new(MockRuntimeSpawner::default()),
            fs.clone(),
            mgr,
        );
        registry.register_handler(TaskType::LocalAgent, Arc::new(handler));
        let registry = Arc::new(registry);
        sink.bind(registry.clone());
        // The pairing table the composition root shares with the report tool.
        let coordinator = AgentId::new();
        let pairings = Arc::new(platform_api::observer_pairing::ObserverPairings::new());
        registry.set_observer_pairings(pairings.clone());
        let seed = platform_api::observer_pairing::ObserverPairingSeed {
            spec: platform_api::subagent_spawn::ObserverSpec::new("reviewer"),
            observed_name: "step two".into(),
            observed_creator: Some(coordinator),
            observed_creator_name: Some("coordinator".into()),
        };
        let request = SubagentSpawnRequest {
            subagent_type: "reviewer".into(),
            prompt: "Observe material issues".into(),
            name: Some("step two".into()),
            observer: Some(platform_api::subagent_spawn::ObserverSpec::new("reviewer")),
            // A coordinator's worker: the report must go UP to the coordinator.
            // This also pins that arming happens BEFORE the request is stripped
            // — `creator_agent_id` is cleared a few lines later, so an arming
            // that ran after the strip would read `None` and take the plain
            // branch with everything still green.
            creator_agent_id: Some(coordinator),
            creator_teammate_name: Some("coordinator".into()),
            ..Default::default()
        };
        let inherit = SubagentInheritance {
            tool_invoker: Arc::new(MockInvoker),
            budget: Arc::new(MockBudget),
        };
        let observed = AgentId::new();
        TaskRegistryHandle::observe_agent_activity(
            registry.as_ref(),
            request.clone(),
            inherit.clone(),
            observed,
            "first digest".into(),
            Some(seed.clone()),
        )
        .await
        .unwrap();
        let tx = loop {
            if let Some(tx) = tx_slot.lock().unwrap().clone() {
                break tx;
            }
            tokio::task::yield_now().await;
        };
        let rows = registry.list().await;
        assert_eq!(rows.len(), 1, "one independent task per observed agent");
        let row = &rows[0];
        assert!(matches!(row, crate::state::TaskState::LocalAgent(a) if a.is_observer));
        assert!(
            matches!(row, crate::state::TaskState::LocalAgent(a) if Some(a.agent_id) == *streaming.spawned_id.lock().unwrap()),
            "registry identity must be the actual allocated runner, not the task input placeholder"
        );
        assert!(
            matches!(row, crate::state::TaskState::LocalAgent(a) if a.observed_agent_id == Some(observed))
        );
        assert!(
            row.base().creator_agent_id.is_none(),
            "observer pairing must not create a cascade-stop ownership edge"
        );
        // THE cross-crate link nothing else covers. `agent` drives a stubbed
        // registry, `tasks::observer` tests `arm_pairing` directly, `tool-ui`
        // tests the report tool against a hand-built table, and `orchestrator`
        // proves the table reaches a tool. All four pass even if this spawn
        // path never calls `arm_pairing` — and then `ObserverReport` answers
        // "not armed" forever with nothing red. So: a REAL spawn must leave a
        // pairing, resolvable the way the tool resolves it (by the OBSERVER's
        // id, not the observed agent's key).
        let pairing = pairings
            .get(&observed.to_string())
            .expect("a real observer spawn must arm a pairing");
        assert!(pairing.state.is_armed());
        assert_eq!(pairing.observed_task_id, Some(observed));
        assert_eq!(pairing.observed_envelope_name, "step-two");
        assert!(
            pairings
                .armed_for_observer(&pairing.observer_task_id)
                .is_some(),
            "the report tool looks pairings up by the OBSERVER id"
        );
        assert_eq!(
            pairing.report_target_task_id,
            Some(coordinator),
            "a worker's observer reports UP to the coordinator"
        );
        assert_ne!(
            pairing.report_target_task_id,
            Some(observed),
            "…and never into the worker it is watching"
        );
        assert_eq!(pairing.via_worker_name.as_deref(), Some("step-two"));
        let id = row.base().id.clone();
        let marker = row.base().output_file.with_extension("observer.json");
        assert!(fs
            .read_file(&marker.to_string_lossy(), None, None)
            .await
            .unwrap()
            .content
            .contains("\"isObserver\":true"));
        assert!(streaming.requests.lock().unwrap()[0]
            .prompt
            .contains("first digest"));
        TaskRegistryHandle::observe_agent_activity(
            registry.as_ref(),
            request.clone(),
            inherit.clone(),
            observed,
            "second digest".into(),
            Some(seed.clone()),
        )
        .await
        .unwrap();
        assert_eq!(
            count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "activity never resumes/cancels a running model request"
        );
        tx.send(completed_event("rest-one")).await.unwrap();
        for _ in 0..400 {
            if count.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            streaming.delivered.lock().unwrap().as_slice(),
            &["second digest"]
        );
        assert!(registry.take_pending_task_notifications().await.is_empty());
        tx.send(completed_event("rest-two")).await.unwrap();
        for _ in 0..400 {
            if registry.get(&id).await.unwrap().is_parked() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(registry.get(&id).await.unwrap().is_parked());
        assert!(registry.take_pending_task_notifications().await.is_empty());
        registry.kill_with_reason(&id, "user").await.unwrap();
        assert!(TaskRegistryHandle::observe_agent_activity(
            registry.as_ref(),
            request.clone(),
            inherit.clone(),
            observed,
            "after stop".into(),
            Some(seed.clone()),
        )
        .await
        .is_err());
        assert_eq!(
            streaming.requests.lock().unwrap().len(),
            1,
            "a stopped observer is not recreated"
        );
        memory_fs
            .corrupt_observer_markers
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(TaskRegistryHandle::observe_agent_activity(
            registry.as_ref(),
            request,
            inherit,
            AgentId::new(),
            "corrupt marker".into(),
            Some(seed),
        )
        .await
        .is_err());
        assert_eq!(
            streaming.requests.lock().unwrap().len(),
            1,
            "bad marker must fail before starting an observer model"
        );
        assert_eq!(
            registry.list().await.len(),
            1,
            "failed publication leaves no new observer row"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn persistent_agent_output_links_transcript_without_copying_report_into_it() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (dir, manager) = make_output_manager(fs.clone());
        let target = dir.path().join("agent-real.jsonl");
        std::fs::write(&target, "transcript line\n").unwrap();
        let sink = Arc::new(RecordingSink::default());
        let tx_slot = Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        *streaming.transcript.lock().unwrap() = Some(target.clone());
        let handler = make_handler(
            MockSpawner::new(CannedResult::Pending),
            manager.clone(),
            sink.clone(),
        )
        .with_streaming_spawner(streaming);
        let ctx = make_ctx(fs);
        let handle = handler
            .spawn(local_agent_input("start"), ctx.clone())
            .await
            .unwrap();
        let output = manager.path_for(&handle.task_id).unwrap();
        let tx = loop {
            if let Some(tx) = tx_slot.lock().unwrap().clone() {
                break tx;
            }
            tokio::task::yield_now().await;
        };
        tx.send(completed_event("clean report")).await.unwrap();
        for _ in 0..200 {
            if std::fs::read_link(&output).is_ok() && *sink.parked.lock().unwrap() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(std::fs::read_link(&output).unwrap(), target);
        assert_eq!(
            std::fs::read_to_string(&output).unwrap(),
            "transcript line\n"
        );
        assert_eq!(
            manager
                .read(&output, crate::output_manager::OutputOptions::default())
                .await
                .unwrap()
                .content,
            "transcript line\n"
        );
        handler.kill(&handle.task_id, ctx).await.unwrap();
    }

    #[tokio::test]
    async fn persistent_agent_rests_after_each_turn_set_and_resumes_on_message() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let resume_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming = MockStreamingSpawner::new(tx_slot.clone(), resume_count.clone());

        // The one-shot spawner is present but UNUSED on the persistent path.
        let handler = make_handler(
            MockSpawner::new(CannedResult::Pending),
            mgr.clone(),
            sink.clone(),
        )
        .with_streaming_spawner(streaming);
        assert!(
            handler.supports_messages(),
            "streaming seam ⇒ messages supported"
        );

        let ctx = make_ctx(fs);
        let handle = handler
            .spawn(local_agent_input("start"), ctx.clone())
            .await
            .expect("spawn should succeed");
        let task_id = handle.task_id.clone();

        // Wait for the persistent worker to call spawn_persistent (stashes tx).
        let tx = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(t) = tx_slot.lock().unwrap().clone() {
                    got = Some(t);
                    break;
                }
                tokio::task::yield_now().await;
            }
            got.expect("spawn_persistent should have run")
        };

        // Turn completion is Completed while the persistent runner is retained.
        tx.send(completed_event("first")).await.unwrap();
        let spool_path = dir.path().join(format!("{task_id}.output"));
        await_spool_contains(&mgr, &spool_path, "first").await;
        // Rested, NOT terminal — still alive awaiting the next message.
        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Completed),
            "agent comes to rest (alive)"
        );
        assert_eq!(sink.rest_count(), 1, "first rest armed a notification");

        // ── Resume via send_message → the runner wakes for turn-set 2. ──
        handler
            .send_message(&task_id, "again".into(), ctx)
            .await
            .expect("resume ok");
        assert_eq!(sink.last_status(), Some(TaskStatus::Running));
        assert_eq!(
            resume_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "send_message routed to resume"
        );

        // ── Turn-set 2: another Completed ⇒ second spool + rest again. ──
        tx.send(completed_event("second")).await.unwrap();
        await_spool_contains(&mgr, &spool_path, "second").await;
        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Completed),
            "rests again after the second turn-set"
        );
        assert_eq!(
            sink.rest_count(),
            2,
            "second rest re-armed the notification"
        );

        // ── Channel close ⇒ the agent terminates (final Completed). ──
        // Drop EVERY Sender — the one the slot still holds plus our handle.
        tx_slot.lock().unwrap().take();
        drop(tx);
        let status = await_terminal(&sink).await;
        assert_eq!(
            status,
            TaskStatus::Completed,
            "channel close ⇒ terminal Completed"
        );
    }

    /// 2.1.266 `a0`, the TARGET half: an agent whose own stop has not finished
    /// must not be re-entered. Upstream refuses with its own message, distinct
    /// from the caller-side refusals — the caller here is fine; the agent being
    /// resumed is the one that is mid-teardown.
    #[tokio::test]
    async fn resuming_an_agent_that_is_still_stopping_is_refused() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (dir, mgr) = make_output_manager(fs.clone());
        let _ = &dir;
        let sink = Arc::new(RecordingSink::default());
        let ctx = make_ctx(fs);
        let tx_slot = Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink)
            .with_streaming_spawner(streaming.clone());

        let handle = handler
            .spawn(local_agent_input("start"), ctx.clone())
            .await
            .expect("spawn should succeed");
        let task_id = handle.task_id.clone();

        // Wait for the persistent worker to register its agent id.
        let agent_id = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(id) = *streaming.spawned_id.lock().unwrap() {
                    got = Some(id);
                    break;
                }
                tokio::task::yield_now().await;
            }
            got.expect("spawn_persistent should have run")
        };

        // Control: with no stop pending the resume is accepted.
        handler
            .send_message(&task_id, "first".into(), ctx.clone())
            .await
            .expect("a live agent accepts a message");

        // Now its stop is in flight.
        let _stopping = platform_api::agent_processes::mark_stop_pending(&agent_id.to_string());
        let err = handler
            .send_message(&task_id, "again".into(), ctx)
            .await
            .expect_err("an agent mid-stop must not be re-entered");
        match err {
            TaskError::Internal(message) => {
                assert!(
                    message.contains("is still stopping")
                        && message.contains("Re-run TaskStop on it"),
                    "upstream's target-side wording: {message}"
                );
            }
            other => panic!("expected Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn send_message_requires_seam_and_live_agent() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());
        let ctx = make_ctx(fs);

        // No streaming seam ⇒ one-shot ⇒ messaging unsupported.
        let one_shot = make_handler(
            MockSpawner::new(CannedResult::Pending),
            mgr.clone(),
            sink.clone(),
        );
        assert!(!one_shot.supports_messages());
        assert!(matches!(
            one_shot.send_message("any", "hi".into(), ctx.clone()).await,
            Err(TaskError::Unsupported)
        ));

        // Seam wired but no live agent for that id ⇒ TerminatedTask.
        let streaming = MockStreamingSpawner::new(
            Arc::new(StdMutex::new(None)),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let persistent = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink)
            .with_streaming_spawner(streaming);
        assert!(persistent.supports_messages());
        assert!(matches!(
            persistent.send_message("ghost", "hi".into(), ctx).await,
            Err(TaskError::TerminatedTask)
        ));
    }

    #[tokio::test]
    async fn worker_waits_for_registry_publication_before_starting_subagent() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!({ "answer": "ready" }), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink {
            explicit_activation: true,
            ..Default::default()
        });
        let handler = make_handler(spawner.clone(), mgr, sink.clone());

        let mut handle = handler
            .spawn(local_agent_input("wait for registry"), make_ctx(fs))
            .await
            .expect("handler schedules the worker");

        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            spawner.request().is_none(),
            "the subagent must not start before TaskRegistry publishes its row"
        );
        assert_eq!(
            sink.last_status(),
            None,
            "status updates must not race ahead of registry publication"
        );

        handle.activate();
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(status) = sink.last_status() {
                    if status.is_terminal() {
                        break status;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("worker should proceed once the registry row exists");
        assert_eq!(status, TaskStatus::Completed);
        assert!(spawner.request().is_some());
    }

    #[tokio::test]
    async fn dropping_unactivated_handle_cancels_prepared_subagent() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!({ "unused": true }), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink {
            explicit_activation: true,
            ..Default::default()
        });
        let handler = make_handler(spawner.clone(), mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("cancel before commit"), make_ctx(fs))
            .await
            .expect("handler prepares the worker");
        drop(handle);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !workers.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the activation owner stops the prepared worker");
        assert!(spawner.request().is_none());
        assert_eq!(sink.last_status(), None);
    }

    #[tokio::test]
    async fn spawn_runs_subagent_and_spools_completed_content() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(
            json!({ "answer": "42", "ok": true }),
            1234,
        ));
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner.clone(), mgr.clone(), sink.clone());

        let handle = handler
            .spawn(local_agent_input("do the thing"), make_ctx(fs))
            .await
            .expect("spawn should succeed");

        assert!(
            handle.task_id.starts_with('a'),
            "LocalAgent id prefix is 'a'"
        );
        assert!(handle.cleanup.is_some(), "cleanup seam is present");

        let status = await_terminal(&sink).await;
        assert_eq!(
            status,
            TaskStatus::Completed,
            "Completed result ⇒ Completed"
        );

        // The request carried the resolved subagent_type + prompt, empty context.
        let req = spawner.request().expect("spawner must have been called");
        assert_eq!(
            req.subagent_type, "general-purpose",
            "default type fallback"
        );
        assert_eq!(req.prompt, "do the thing");
        assert!(req.context_paths.is_empty());

        // The Completed content was spooled (pretty JSON ⇒ contains the keys)
        // along with the token-usage footer.
        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(read.content.contains("\"answer\""), "content key spooled");
        assert!(read.content.contains("42"), "content value spooled");
        assert!(
            read.content.contains("<total_tokens>1234</total_tokens>"),
            "token usage spooled in the usage footer"
        );
    }

    #[tokio::test]
    async fn failed_result_maps_to_failed_and_spools_reason() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Failed("model refused".into()));
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr.clone(), sink.clone());

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);

        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(read.content.contains("model refused"), "reason spooled");

        // No usage footer on the failed path.
        assert!(!read.content.contains("<total_tokens>"));
    }

    #[tokio::test]
    async fn killed_result_maps_to_killed() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn spawn_error_maps_to_failed_and_spools_error() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Err("pool full".into()));
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr.clone(), sink.clone());

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);

        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(read.content.contains("pool full"), "spawn error spooled");
    }

    /// Poll until the handler's live worker map is empty — the worker removes
    /// its own record LAST (after the terminal status + worktree judgment), so
    /// an empty map means the judgment definitely ran (or was skipped).
    async fn await_workers_drained(workers: &Arc<TokioMutex<StdHashMap<String, WorkerCancel>>>) {
        for _ in 0..400 {
            if workers.lock().await.is_empty() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("worker never drained its cancel record");
    }

    /// P1-01 (parity 2.1.207): the BACKGROUND lifecycle owns the isolation
    /// worktree's terminal keep/cleanup judgment (claude hands its
    /// `getWorktreeResult` closure to the detached task). A CLEAN worktree is
    /// auto-removed once the worker reaches a terminal state.
    #[tokio::test]
    async fn terminal_background_agent_removes_clean_worktree() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("ok"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 0,
            commits: 0,
        }));
        let handler = make_handler(spawner, mgr, sink.clone())
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>);
        let workers = handler.workers_map();

        handler
            .spawn(input_with_worktree("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;
        assert_eq!(
            wt.removed_count(),
            1,
            "clean worktree auto-removed at terminal"
        );
    }

    /// P1-01: a DIRTY worktree (uncommitted files or commits ahead) is KEPT —
    /// the terminal judgment must never discard work.
    #[tokio::test]
    async fn terminal_background_agent_keeps_dirty_worktree() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Failed("model refused".into()));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 2,
            commits: 1,
        }));
        let handler = make_handler(spawner, mgr, sink.clone())
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>);
        let workers = handler.workers_map();

        handler
            .spawn(input_with_worktree("p"), make_ctx(fs))
            .await
            .unwrap();

        // Runs for ANY terminal outcome (here: Failed) — but a dirty worktree
        // is kept, not removed.
        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);
        await_workers_drained(&workers).await;
        assert_eq!(wt.removed_count(), 0, "dirty worktree KEPT at terminal");
    }

    // ── terminal notification payload (`enqueueAgentNotification`) ──────────

    /// A completed background agent reports its FINAL TEXT and usage, not just
    /// a status. Before this the spool held the answer and the notification did
    /// not, so the model was told an agent finished without being told what it
    /// found.
    ///
    /// The `<result>` is the text-block join (claude `wc(content,"\n")`), NOT
    /// the JSON-pretty spool body.
    #[tokio::test]
    async fn completed_agent_reports_final_text_and_usage() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let content = json!([
            { "type": "text", "text": "first" },
            { "type": "tool_use", "name": "Bash", "input": {} },
            { "type": "text", "text": "second" }
        ]);
        let spawner = MockSpawner::new(CannedResult::Completed(content, 42));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        let outcome = sink.outcome();
        assert_eq!(
            outcome.result.as_deref(),
            Some("first\nsecond"),
            "text blocks joined with \\n; the tool_use block dropped"
        );
        let usage = outcome.usage.expect("<usage> section");
        assert_eq!(usage.subagent_tokens, 42);
        assert!(outcome.error.is_none(), "a clean run reports no error");
    }

    /// AGT-08 / TN-06 producer link: the runner's max-turns fall-through
    /// completes with `{"reason":"max_turns_exhausted","max_turns":N}`, and
    /// that budget has to reach the outcome or the notification cannot say the
    /// result is partial.
    #[tokio::test]
    async fn an_agent_that_exhausts_its_turns_reports_the_budget() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let content = json!({ "reason": "max_turns_exhausted", "max_turns": 12 });
        let spawner = MockSpawner::new(CannedResult::Completed(content, 7));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        assert_eq!(sink.outcome().max_turns_reached, Some(12));
    }

    /// The budget is read ONLY from the max-turns payload: an ordinary
    /// completion must leave it unset, or every finished agent would render
    /// the turn-limit verb.
    #[tokio::test]
    async fn an_ordinary_completion_reports_no_turn_budget() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let content = json!([{ "type": "text", "text": "done" }]);
        let spawner = MockSpawner::new(CannedResult::Completed(content, 7));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        assert_eq!(sink.outcome().max_turns_reached, None);
    }

    /// An agent whose final content has no text blocks omits `<result>` rather
    /// than rendering an empty one — claude gates the section on a TRUTHY
    /// `finalMessage` (`s ? "<result>…" : ""`).
    #[tokio::test]
    async fn completed_agent_with_no_text_omits_result() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!([]), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        assert!(sink.outcome().result.is_none());
    }

    /// The failure reason reaches the notification, so the summary reads
    /// `failed: model refused` instead of claude's `Unknown error` fallback —
    /// which is what every failed background agent reported while nothing in
    /// production wrote `LocalAgentTaskState::error`.
    #[tokio::test]
    async fn failed_agent_reports_its_reason() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Failed("model refused".into()));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);
        await_workers_drained(&workers).await;

        let outcome = sink.outcome();
        assert_eq!(outcome.error.as_deref(), Some("model refused"));
        assert!(outcome.result.is_none(), "a failed run has no final text");
        assert!(
            outcome.usage.is_none(),
            "no usage rollup on the failed path"
        );
    }

    /// The payload must land BEFORE the terminal status: the registry's drain
    /// is gated on terminal-and-not-notified, so the reverse order lets a drain
    /// that runs in between render the completion with none of its optional
    /// sections. Claude passes status and payload to
    /// `enqueueAgentNotification` in ONE call, so it has no such window.
    #[tokio::test]
    async fn outcome_is_published_before_the_terminal_status() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(
            json!([{ "type": "text", "text": "done" }]),
            7,
        ));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        // The invariant is the ORDER, not the transcript: an `outcome` must
        // land before the terminal `status`, so a drain running in between
        // cannot see a terminal row with none of its sections. There is also a
        // launch-time `outcome` (the spawn metadata the kill path reads), so
        // pinning an exact two-element vector would break on any additional
        // partial report while the invariant still held.
        let calls = sink.calls();
        let status_at = calls
            .iter()
            .position(|c| *c == "status")
            .expect("a terminal status is published");
        assert_eq!(
            status_at,
            calls.len() - 1,
            "the terminal status is last: {calls:?}"
        );
        assert!(
            calls[..status_at].iter().any(|c| *c == "outcome"),
            "a payload must precede the terminal status: {calls:?}"
        );
    }

    /// A KEPT isolation worktree's `(path, branch)` — the binary's
    /// `...await getWorktreeResult()` spread — fills the notification's
    /// `<worktree>` section. The handler already ran this judgment and threw
    /// the answer away.
    #[tokio::test]
    async fn kept_worktree_fills_the_worktree_section() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!([]), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 2,
            commits: 1,
        }));
        let handler = make_handler(spawner, mgr, sink.clone())
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>);
        let workers = handler.workers_map();

        handler
            .spawn(input_with_worktree("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        let outcome = sink.outcome();
        assert_eq!(
            outcome.worktree_path.as_deref(),
            Some("/repo/.lingxi/worktrees/agent-1")
        );
        assert_eq!(outcome.worktree_branch.as_deref(), Some("worktree-agent-1"));
    }

    /// A worktree the terminal judgment AUTO-REMOVED reports nothing — claude's
    /// `getWorktreeResult` resolves to `{}` there, so the whole `<worktree>`
    /// section is omitted rather than pointing at a deleted directory.
    #[tokio::test]
    async fn removed_worktree_omits_the_worktree_section() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!([]), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 0,
            commits: 0,
        }));
        let handler = make_handler(spawner, mgr, sink.clone())
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>);
        let workers = handler.workers_map();

        handler
            .spawn(input_with_worktree("p"), make_ctx(fs))
            .await
            .unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;

        assert_eq!(wt.removed_count(), 1, "clean worktree removed");
        let outcome = sink.outcome();
        assert!(outcome.worktree_path.is_none());
        assert!(outcome.worktree_branch.is_none());
    }

    /// P1-01: on the PERSISTENT path the judgment runs ONLY at a terminal
    /// state — a "comes to rest" turn-set completion must NOT remove the
    /// worktree (the agent is still alive and resumable in it).
    #[tokio::test]
    async fn persistent_agent_worktree_judged_only_at_terminal() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 0,
            commits: 0,
        }));
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink.clone())
            .with_streaming_spawner(streaming)
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>);
        let workers = handler.workers_map();

        handler
            .spawn(input_with_worktree("p"), make_ctx(fs))
            .await
            .unwrap();

        // Wait for spawn_persistent to stash the event sender.
        let tx = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(t) = tx_slot.lock().unwrap().clone() {
                    got = Some(t);
                    break;
                }
                tokio::task::yield_now().await;
            }
            got.expect("spawn_persistent should have run")
        };

        // Turn-set completes → the agent comes to REST (not terminal): the
        // worktree must survive (the resting agent still works in it).
        tx.send(completed_event("rest")).await.unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(wt.removed_count(), 0, "no judgment at a rest");

        // Channel close ⇒ terminal ⇒ the clean worktree is auto-removed.
        tx_slot.lock().unwrap().take();
        drop(tx);
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        await_workers_drained(&workers).await;
        assert_eq!(
            wt.removed_count(),
            1,
            "clean worktree auto-removed at terminal"
        );
    }

    #[tokio::test]
    async fn kill_cancels_inflight_worker_and_flips_to_killed() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        // The spawner parks forever, so the worker stays alive until kill.
        let spawner = MockSpawner::new(CannedResult::Pending);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs.clone()))
            .await
            .unwrap();

        // Let the worker reach its parked await and the Running status land.
        for _ in 0..50 {
            if !workers.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            workers.lock().await.contains_key(&handle.task_id),
            "worker-cancel record present while subagent runs"
        );

        handler
            .kill(&handle.task_id, make_ctx(fs))
            .await
            .expect("kill should succeed");

        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
        assert!(
            !workers.lock().await.contains_key(&handle.task_id),
            "cancel record removed by kill"
        );
    }

    /// P1-04 (parity 2.1.208): killing a PERSISTENT agent must free the INNER
    /// pool runner's slot, not merely cancel the outer event-pump worker. The
    /// parked runner holds a `max_concurrent` slot until `stop` (UserExit +
    /// deallocate) runs; without it, repeated kill exhausts the cap-4 pool. The
    /// handler looks up the agent id from the resume-routing `agent_ids` map and
    /// stops the runner, and a later `send_message` sees the agent as gone.
    #[tokio::test]
    async fn kill_deallocates_persistent_inner_runner() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink.clone())
            .with_streaming_spawner(streaming.clone());
        let ctx = make_ctx(fs);

        let handle = handler
            .spawn(local_agent_input("start"), ctx.clone())
            .await
            .unwrap();
        let task_id = handle.task_id.clone();

        // Wait for spawn_persistent to run (stashes tx + records the id), then
        // yield so the worker inserts the id into the resume-routing map before
        // kill reads it.
        for _ in 0..200 {
            if tx_slot.lock().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let spawned_id = streaming.spawned_id.lock().unwrap().expect("spawn ran");

        handler
            .kill(&task_id, ctx.clone())
            .await
            .expect("kill should succeed");

        // The inner pool runner was torn down (UserExit + deallocate) for the
        // exact agent that was spawned — the slot is freed, not leaked.
        assert_eq!(
            streaming.stopped.lock().unwrap().as_slice(),
            &[spawned_id],
            "kill stops the inner persistent runner (frees its pool slot)"
        );
        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
        // The resume-routing entry is gone ⇒ a later message sees a dead agent.
        assert!(matches!(
            handler.send_message(&task_id, "hi".into(), ctx).await,
            Err(TaskError::TerminatedTask)
        ));
    }

    #[tokio::test]
    async fn kill_stops_persistent_inner_runner_before_agent_id_registration_lands() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let handler = Arc::new(
            make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink.clone())
                .with_streaming_spawner(streaming.clone()),
        );
        let ctx = make_ctx(fs);

        let agent_ids_guard = handler.agent_ids.lock().await;
        let handle = handler
            .spawn(local_agent_input("start"), ctx.clone())
            .await
            .unwrap();
        let task_id = handle.task_id.clone();
        let workers = handler.workers_map();

        for _ in 0..200 {
            if tx_slot.lock().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        for _ in 0..200 {
            let ready = {
                let workers = workers.lock().await;
                workers.get(&task_id).is_some_and(|rec| {
                    rec.persistent_teardown
                        .as_ref()
                        .and_then(|teardown| *teardown.agent_id.lock().unwrap())
                        .is_some()
                })
            };
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }
        let spawned_id = streaming.spawned_id.lock().unwrap().expect("spawn ran");

        let kill = tokio::spawn({
            let handler = handler.clone();
            let task_id = task_id.clone();
            let ctx = ctx.clone();
            async move { handler.kill(&task_id, ctx).await }
        });

        for _ in 0..200 {
            if streaming.stopped.lock().unwrap().as_slice() == &[spawned_id] {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            streaming.stopped.lock().unwrap().as_slice(),
            &[spawned_id],
            "kill must stop the inner runner even before agent_ids registration finishes"
        );

        drop(agent_ids_guard);
        kill.await
            .expect("kill join should succeed")
            .expect("kill should succeed");

        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
        assert!(matches!(
            handler.send_message(&task_id, "hi".into(), ctx).await,
            Err(TaskError::TerminatedTask)
        ));
    }

    #[tokio::test]
    async fn killing_rested_persistent_agent_runs_terminal_teardown() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let wt = RecordingWorktree::new(Some(platform_api::worktree::WorktreeChangeSummary {
            changed_files: 0,
            commits: 0,
        }));
        let parked = Arc::new(RecordingParkedStore::default());
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink.clone())
            .with_streaming_spawner(streaming.clone())
            .with_worktree_manager(wt.clone() as Arc<dyn platform_api::worktree::WorktreeManager>)
            .with_parked_agent_store(
                parked.clone() as Arc<dyn platform_api::parked_agent_store::ParkedAgentStore>
            );
        let ctx = make_ctx(fs);

        let handle = handler
            .spawn(input_with_worktree("start"), ctx.clone())
            .await
            .unwrap();
        let task_id = handle.task_id.clone();

        let tx = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(t) = tx_slot.lock().unwrap().clone() {
                    got = Some(t);
                    break;
                }
                tokio::task::yield_now().await;
            }
            got.expect("spawn_persistent should have run")
        };
        let spawned_id = streaming.spawned_id.lock().unwrap().expect("spawn ran");

        tx.send(SubagentEvent::Completed {
            agent_id: AgentId::new(),
            result: json!({ "text": "rest answer" }),
            usage: llm_runtime::Usage::default(),
            total_tool_use_count: 3,
            total_duration_ms: 1500,
            assistant_message_count: 0,
            last_request_id: None,
            cumulative_usage: llm_runtime::Usage::default(),
            usage_complete: true,
        })
        .await
        .unwrap();
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(sink.rest_count(), 1, "first turn-set came to rest");
        assert_eq!(
            parked.parked.lock().unwrap().len(),
            1,
            "rest parked the agent"
        );

        handler
            .kill(&task_id, ctx)
            .await
            .expect("kill should succeed");

        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
        // Two payloads must precede the terminal status: the rest's accumulated
        // one (so the rest notification can read the exhausted turn budget off
        // it) and the terminal one. Counted rather than transcribed — there is
        // also a launch-time payload (the spawn metadata the kill path reads),
        // and pinning the exact vector would break on any additional partial
        // report while the invariant still held.
        let calls = sink.calls();
        let status_at = calls
            .iter()
            .position(|c| *c == "status")
            .expect("a terminal status is published");
        assert_eq!(status_at, calls.len() - 1, "status is last: {calls:?}");
        assert!(
            calls[..status_at]
                .iter()
                .filter(|c| **c == "outcome")
                .count()
                >= 2,
            "the rest payload AND the terminal payload must both precede Killed: {calls:?}"
        );
        let outcome = sink.outcome();
        assert_eq!(outcome.result.as_deref(), Some("rest answer"));
        assert_eq!(outcome.usage.as_ref().map(|u| u.tool_uses), Some(3));
        assert_eq!(wt.removed_count(), 1, "kill runs worktree judgment");
        assert_eq!(
            parked.unparked.lock().unwrap().as_slice(),
            &[spawned_id],
            "kill removes the durable parked-agent row"
        );
        assert_eq!(
            streaming.stopped.lock().unwrap().as_slice(),
            &[spawned_id],
            "kill deallocates the inner persistent runner"
        );
        assert!(matches!(
            handler
                .send_message(&task_id, "hi".into(), make_ctx(Arc::new(InMemoryFs::new())))
                .await,
            Err(TaskError::TerminatedTask)
        ));
        tx_slot.lock().unwrap().take();
        drop(tx);
    }

    /// A raced kill must not overwrite a terminal status the sink already
    /// observed, even if the worker-cancel record is still live.
    #[tokio::test]
    async fn kill_preserves_terminal_status_when_sink_already_knows_task_is_done() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Pending);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs.clone()))
            .await
            .unwrap();

        for _ in 0..50 {
            if !workers.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        sink.set_status(&handle.task_id, TaskStatus::Completed)
            .await;

        handler
            .kill(&handle.task_id, make_ctx(fs))
            .await
            .expect("kill should still succeed");

        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Completed),
            "kill must not overwrite an already-terminal status"
        );
    }

    /// P1-04: reaching a terminal state on the persistent path (here a natural
    /// channel-close) also frees the inner pool slot — `spawn_persistent`
    /// returns no dealloc owner, so even normal termination would leak the slot
    /// without the worker calling `stop` at terminal.
    #[tokio::test]
    async fn terminal_persistent_agent_deallocates_inner_runner() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let streaming = MockStreamingSpawner::new(
            tx_slot.clone(),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        );
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink.clone())
            .with_streaming_spawner(streaming.clone());

        handler
            .spawn(local_agent_input("start"), make_ctx(fs))
            .await
            .unwrap();

        // Wait for spawn_persistent, then close the channel ⇒ terminal Completed.
        let tx = {
            let mut got = None;
            for _ in 0..200 {
                if let Some(t) = tx_slot.lock().unwrap().clone() {
                    got = Some(t);
                    break;
                }
                tokio::task::yield_now().await;
            }
            got.expect("spawn_persistent should have run")
        };
        let spawned_id = streaming.spawned_id.lock().unwrap().expect("spawn ran");
        tx_slot.lock().unwrap().take();
        drop(tx);

        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        assert_eq!(
            streaming.stopped.lock().unwrap().as_slice(),
            &[spawned_id],
            "terminal state deallocates the inner persistent runner's slot"
        );
    }

    #[tokio::test]
    async fn kill_absent_task_is_graceful_noop() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());

        // No spawn ⇒ no worker record. kill must still succeed and flip Killed.
        handler
            .kill("adeadbeef", make_ctx(fs))
            .await
            .expect("kill of unknown task is a no-op success");
        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
    }

    #[tokio::test]
    async fn cleanup_then_drain_cancels_inflight_worker() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Pending);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        // Wait for the worker-cancel record to be present.
        for _ in 0..50 {
            if !workers.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Fire the synchronous cleanup closure (records a pending teardown
        // request without needing the async worker map lock).
        (handle.cleanup.as_ref().unwrap())();

        assert!(
            workers.lock().await.contains_key(&handle.task_id),
            "cleanup itself no longer mutates the live worker map"
        );

        // Drain performs the real async cancel + flips status.
        handler.drain_pending_kills().await;
        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
    }

    /// A fast-completing agent leaves NO stale worker record (the `workers` lock
    /// is held across spawn+insert so the worker's self-remove is serialized
    /// after the insert), so a later cleanup + drain finds nothing to kill and
    /// the reported terminal status stays `Completed` — never clobbered to
    /// `Killed` by a raced pending-kill record.
    #[tokio::test]
    async fn completed_agent_leaves_no_worker_record_and_survives_cleanup() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("ok"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        // The worker removed its OWN record; no stale insert remains.
        await_workers_drained(&workers).await;

        // Cleanup finds no live record ⇒ nothing queued; drain is a no-op.
        (handle.cleanup.as_ref().unwrap())();
        handler.drain_pending_kills().await;

        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Completed),
            "a completed agent's terminal status is never flipped to Killed"
        );
    }

    /// `drain_pending_kills` MUST NOT flip an already-terminal task to `Killed`:
    /// if a still-live record is moved to `pending_kill` by cleanup AFTER the
    /// worker reported a terminal status, draining it keeps the real terminal
    /// status (the guard on `TaskStatusSink::is_terminal`).
    #[tokio::test]
    async fn drain_pending_kills_preserves_terminal_status() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        // Pending: the worker parks, so a live record persists in `workers`.
        let spawner = MockSpawner::new(CannedResult::Pending);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(local_agent_input("p"), make_ctx(fs))
            .await
            .unwrap();

        // Wait for the live worker-cancel record.
        for _ in 0..50 {
            if !workers.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        // Model the worker having reported a terminal status just before the
        // teardown races in (the record is still live in `workers`).
        sink.set_status(&handle.task_id, TaskStatus::Completed)
            .await;

        // Cleanup moves the live record to pending_kill; drain then runs.
        (handle.cleanup.as_ref().unwrap())();
        handler.drain_pending_kills().await;

        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Completed),
            "drain must not overwrite an already-terminal Completed with Killed"
        );
    }

    #[tokio::test]
    async fn variant_subagent_type_drives_the_request() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("ok"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner.clone(), mgr, sink.clone());

        // A non-default subagent_type on the variant must flow through verbatim
        // (parity with TS `agentType` — no AgentId-to-type derivation).
        let input = TaskSpawnInput::LocalAgent {
            agent_id: protocol::AgentId::new(),
            subagent_type: "code-reviewer".into(),
            prompt: "p".into(),
            is_backgrounded: true,
            tool_use_id: None,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
            spawn_request: None,
            inheritance: None,
        };

        handler.spawn(input, make_ctx(fs)).await.unwrap();

        await_terminal(&sink).await;
        assert_eq!(
            spawner.request().unwrap().subagent_type,
            "code-reviewer",
            "the variant's subagent_type drives the request verbatim"
        );
    }

    /// Background Agent invocations arrive through `TaskSpawnInput` after the
    /// Agent tool has already resolved every override. The task boundary must
    /// not reconstruct a stripped request or replace the immediate parent's
    /// inheritance handles with composition-root defaults.
    #[tokio::test]
    async fn full_background_request_and_inheritance_survive_task_boundary() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("ok"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner.clone(), mgr, sink.clone());

        let inherited_invoker: Arc<dyn ToolInvoker> = Arc::new(MockInvoker);
        let inherited_budget: Arc<dyn BudgetEnforcerHandle> = Arc::new(MockBudget);
        let creator_agent_id = protocol::AgentId::new();
        let expected = SubagentSpawnRequest {
            teammate_color: None,
            subagent_type: "code-reviewer".into(),
            prompt: "inspect the background request".into(),
            observer: None,
            context_paths: vec![PathBuf::from("/workspace/CONTEXT.md")],
            description: Some("review request".into()),
            model: Some("opus".into()),
            model_profile: Some("preferred-provider".into()),
            run_in_background: true,
            name: Some("reviewer".into()),
            team_name: Some("team-a".into()),
            creator_teammate_name: Some("lead".into()),
            creator_team_name: Some("alpha".into()),
            creator_agent_id: Some(creator_agent_id),
            mode: Some("plan".into()),
            isolation: Some("worktree".into()),
            cwd: Some("/workspace/subdir".into()),
            worktree: None,
            fork_context_messages: None,
            fork_parent_system_prompt: None,
            schema: Some(r#"{\"type\":\"object\"}"#.into()),
            structured_output_mode: Default::default(),
            structured_output_parse_retries: 0,
            effort: Some(json!("high")),
            tool_use_id: Some("toolu_background".into()),
            system_prompt_override: Some("override".into()),
            system_prompt_addendum: Some("addendum".into()),
            additional_disallowed_tools: vec!["Bash".into()],
            depth: 3,
            origin_session_id: None,
            parent_model_override: Some("claude-opus-4-6".into()),
            forked_skill_name: None,
            forked_skill_attribution: None,
            forked_skill_effort: None,
            frozen_command_denies: Vec::new(),
            resumed_history: None,
            max_turns_override: None,
            max_output_tokens_per_turn: None,
            max_input_bytes_per_turn: None,
            query_source_label: None,
            correlation_id: None,
            model_attempt: None,
        };
        let input = TaskSpawnInput::LocalAgent {
            agent_id: protocol::AgentId::new(),
            // These compact task-index fields deliberately disagree with the
            // full request so forwarding the stripped legacy reconstruction is
            // observable.
            subagent_type: "general-purpose".into(),
            prompt: "legacy prompt".into(),
            is_backgrounded: true,
            tool_use_id: Some("toolu_background".into()),
            creator_teammate_name: Some("lead".into()),
            creator_team_name: Some("alpha".into()),
            creator_agent_id: Some(creator_agent_id),
            spawn_request: Some(expected.clone()),
            inheritance: Some(SubagentInheritance {
                tool_invoker: inherited_invoker.clone(),
                budget: inherited_budget.clone(),
            }),
        };

        handler.spawn(input, make_ctx(fs)).await.unwrap();
        await_terminal(&sink).await;

        let observed = spawner.request().expect("worker received a request");
        assert_eq!(observed, expected, "all Agent spawn overrides survive");
        assert!(Arc::ptr_eq(
            spawner
                .seen_invoker
                .lock()
                .unwrap()
                .as_ref()
                .expect("worker received inheritance"),
            &inherited_invoker,
        ));
        assert!(Arc::ptr_eq(
            spawner
                .seen_budget
                .lock()
                .unwrap()
                .as_ref()
                .expect("worker received inheritance"),
            &inherited_budget,
        ));
    }

    #[tokio::test]
    async fn non_localagent_input_is_rejected() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs.clone());

        let handler = make_handler(spawner, mgr, Arc::new(RecordingSink::default()));

        let result = handler
            .spawn(
                TaskSpawnInput::LocalWorkflow {
                    session_uuid: None,
                    workflow_id: "wf".into(),
                    script: String::new(),
                    resume_from_run_id: None,
                    args: None,
                    run_id: None,
                    invocation_mode: None,
                    workflow_source: None,
                    script_is_verbatim_builtin: None,
                    transcript_subdir: None,
                    launched_from_subagent: false,
                    tool_use_id: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                    scope: None,
                },
                make_ctx(fs),
            )
            .await;
        match result {
            Err(TaskError::Internal(_)) => {}
            Err(other) => panic!("expected Internal, got {other:?}"),
            Ok(_) => panic!("non-LocalAgent input must be rejected"),
        }
    }

    #[test]
    fn name_and_type_are_local_agent() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs);
        let handler = make_handler(spawner, mgr, Arc::new(RecordingSink::default()));
        assert_eq!(handler.name(), "local_agent");
        assert_eq!(handler.task_type(), TaskType::LocalAgent);
        assert!(!handler.supports_messages());
    }

    // ── forked-skill resume gate ─────────────────────────────────────────────

    /// A gate that refuses everything, recording what it was asked about.
    struct RefusingGate {
        seen: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl platform_api::fork_resume_gate::ForkResumeGate for RefusingGate {
        async fn check_resume(
            &self,
            _agent_id: protocol::AgentId,
            task_forked_skill_name: Option<&str>,
        ) -> Result<(), String> {
            self.seen
                .lock()
                .unwrap()
                .push(task_forked_skill_name.map(str::to_string));
            Err("refusing to resume it without the skill's permission scoping.".into())
        }
    }

    struct AllowingGate {
        seen: StdMutex<Vec<Option<String>>>,
    }
    #[async_trait]
    impl platform_api::fork_resume_gate::ForkResumeGate for AllowingGate {
        async fn check_resume(
            &self,
            _agent_id: protocol::AgentId,
            task_forked_skill_name: Option<&str>,
        ) -> Result<(), String> {
            self.seen
                .lock()
                .unwrap()
                .push(task_forked_skill_name.map(str::to_string));
            Ok(())
        }
    }

    /// Drive a persistent agent to rest, then attempt a resume.
    async fn parked_fork_agent(
        fs: Arc<dyn FileSystem>,
        mgr: Arc<TaskOutputManager>,
        sink: Arc<RecordingSink>,
        gate: Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>,
        fork_name: Option<&str>,
    ) -> (
        LocalAgentHandler,
        String,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let resume_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming = MockStreamingSpawner::new(tx_slot.clone(), resume_count.clone());
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink)
            .with_streaming_spawner(streaming)
            .with_fork_resume_gate(gate);

        let mut input = local_agent_input("start");
        if let TaskSpawnInput::LocalAgent { spawn_request, .. } = &mut input {
            let mut req = request_with_worktree("start");
            req.worktree = None;
            req.forked_skill_name = fork_name.map(str::to_string);
            *spawn_request = Some(req);
        }
        let handle = handler
            .spawn(input, make_ctx(fs))
            .await
            .expect("spawn should succeed");

        // Wait for the persistent worker to register its agent id.
        for _ in 0..400 {
            if tx_slot.lock().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        (handler, handle.task_id, resume_count)
    }

    /// A refused resume never reaches the runner. This is the security
    /// property: a forked skill whose scoping cannot be re-established must not
    /// re-enter under the parent's (strictly wider) permissions.
    #[tokio::test]
    async fn a_refused_fork_resume_never_reaches_the_runner() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let gate = Arc::new(RefusingGate {
            seen: StdMutex::new(Vec::new()),
        });
        let (handler, task_id, resume_count) = parked_fork_agent(
            fs.clone(),
            mgr,
            sink,
            gate.clone() as Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>,
            Some("review"),
        )
        .await;

        let err = handler
            .send_message(&task_id, "keep going".into(), make_ctx(fs))
            .await
            .expect_err("the gate refuses");
        assert!(
            format!("{err:?}").contains("permission scoping"),
            "the refusal message surfaces: {err:?}"
        );
        assert_eq!(
            resume_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the message never reached the runner"
        );
        // The gate was told WHICH skill, so it can corroborate against the
        // on-disk record.
        assert_eq!(
            gate.seen.lock().unwrap().as_slice(),
            &[Some("review".to_string())]
        );
    }

    /// An allowed resume proceeds, and an agent that never forked is reported
    /// to the gate as `None` — the gate must not be a tax on ordinary agents.
    #[tokio::test]
    async fn an_allowed_resume_proceeds_and_a_non_fork_reports_no_identity() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let gate = Arc::new(AllowingGate {
            seen: StdMutex::new(Vec::new()),
        });
        let (handler, task_id, resume_count) = parked_fork_agent(
            fs.clone(),
            mgr,
            sink,
            gate.clone() as Arc<dyn platform_api::fork_resume_gate::ForkResumeGate>,
            None,
        )
        .await;

        handler
            .send_message(&task_id, "keep going".into(), make_ctx(fs))
            .await
            .expect("the gate allows");
        assert_eq!(
            resume_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the message reached the runner"
        );
        assert_eq!(gate.seen.lock().unwrap().as_slice(), &[None]);
    }

    /// A host with NO gate wired resumes as before — the seam is additive.
    #[tokio::test]
    async fn an_unwired_gate_leaves_resume_untouched() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let tx_slot: Arc<StdMutex<Option<tokio::sync::mpsc::Sender<SubagentEvent>>>> =
            Arc::new(StdMutex::new(None));
        let resume_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let streaming = MockStreamingSpawner::new(tx_slot.clone(), resume_count.clone());
        let handler = make_handler(MockSpawner::new(CannedResult::Pending), mgr, sink)
            .with_streaming_spawner(streaming);
        let handle = handler
            .spawn(local_agent_input("start"), make_ctx(fs.clone()))
            .await
            .unwrap();
        for _ in 0..400 {
            if tx_slot.lock().unwrap().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        handler
            .send_message(&handle.task_id, "go".into(), make_ctx(fs))
            .await
            .expect("no gate ⇒ resume proceeds");
        assert_eq!(resume_count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
