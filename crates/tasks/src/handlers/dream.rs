//! Dream (memory-consolidation) task handler — M2 implementation.
//!
//! The Rust analogue of the claude-code auto-dream task
//! (`services/autoDream/autoDream.ts` + `tasks/DreamTask/DreamTask.ts`): a
//! one-shot *forked* subagent that runs the memory-consolidation prompt to
//! completion, then completes/fails/kills. Structurally identical to
//! [`crate::handlers::local_agent::LocalAgentHandler`] (the canonical pattern):
//! it drives a [`SubagentSpawner`] to a terminal [`SubagentResult`] on the
//! engine's [`RuntimeSpawner`] (never `tokio::spawn` directly — D17), spools the
//! terminal payload into the task's spool file, reports the terminal
//! [`TaskStatus`] through a narrow status sink, and supports cooperative kill via
//! [`RuntimeSpawner::cancel`] on the recorded [`BackgroundTaskHandle`].
//!
//! ## Why kill rides on the runtime handle, not a process handle
//!
//! As with [`crate::handlers::local_agent::LocalAgentHandler`],
//! [`SubagentSpawner::spawn`] is *await-to-completion*: it returns the terminal
//! [`SubagentResult`] and hands back no live handle while the subagent runs. The
//! only cancellation primitive is therefore cooperative cancellation of the
//! worker future via [`RuntimeSpawner::cancel`] on the
//! [`BackgroundTaskHandle`] returned by [`RuntimeSpawner::spawn`]. The handler
//! records that handle (plus the `runtime` Arc that minted it, since
//! [`TaskContext`] is per-call and the synchronous cleanup path has no `ctx`) in
//! a map keyed by `task_id`, so both [`Task::kill`] and
//! [`DreamHandler::drain_pending_kills`] can cancel the worker.
//!
//! Empty prompts are rendered from the pinned 2.1.319 template using the
//! runtime's real memory and transcript directories. Explicit caller prompts
//! retain their exact bytes. The renderer covers native team-memory and MCP
//! memory branches and the instruction-file reconciliation guidance.

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{
    DreamPromptContext, Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput,
};
use async_trait::async_trait;
use lingxi_core::host::{
    BackgroundTaskHandle, BudgetEnforcerHandle, RuntimeSpawner, SubagentInheritance,
    SubagentResult, SubagentSpawnRequest, SubagentSpawner, ToolInvoker,
};
use memory::agent_tools::{MemoryAgentShell, MemoryAgentToolInvoker};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::Mutex;

// Re-use the status-sink seam from the bash handler so callers wire a single
// implementation across handlers (defined once in `local_bash` to avoid
// divergence — same line `local_agent` uses).
pub use crate::handlers::local_bash::{NoopStatusSink, TaskStatusSink};

/// Handler name reported by [`Task::name`] / used as the runtime task-name
/// prefix. Byte-aligned with the `"dream"` wire string the registration step
/// uses.
const HANDLER_NAME: &str = "dream";

/// The subagent type used for the forked dream agent. TS forks via
/// `runForkedAgent` with no *named* subagent type, so `"general-purpose"` is the
/// faithful mapping (consistent with the local-agent handler's
/// `'general-purpose'` fallback).
const DREAM_SUBAGENT_TYPE: &str = "general-purpose";

/// Native `runForkedAgent` applies this cap when dream omits `maxTurns`.
pub const DREAM_MAX_TURNS: u32 = 50;

#[path = "dream_prompt.rs"]
mod prompt;

pub use prompt::render_consolidation_prompt;

/// Keep an explicit directive intact; an automatically built prompt requires
/// the host to supply concrete memory and transcript directories.
fn build_consolidation_prompt(
    prompt: &str,
    context: Option<&DreamPromptContext>,
) -> Result<String, TaskError> {
    if context.is_some_and(|context| {
        context.memory_directory.is_empty() || context.transcript_directory.is_empty()
    }) {
        return Err(TaskError::Internal(
            "dream consolidation requires memory and transcript directories".into(),
        ));
    }
    if !prompt.is_empty() {
        return Ok(prompt.to_owned());
    }
    let context = context.ok_or_else(|| {
        TaskError::Internal("dream consolidation requires memory and transcript directories".into())
    })?;
    Ok(render_consolidation_prompt(context))
}

/// A worker-cancel record: the [`BackgroundTaskHandle`] returned by
/// [`RuntimeSpawner::spawn`] plus the `runtime` Arc that minted it. Mirrors
/// [`crate::handlers::local_agent::WorkerCancel`]. Public (fields stay private)
/// because it appears in the signature of the public [`DreamHandler::workers_map`]
/// accessor — exactly as the sibling handler does.
///
/// [`TaskContext`] (which carries `runtime`) is per-call and the synchronous
/// [`TaskHandle::cleanup`] closure receives no `ctx`, so the handler captures
/// the `runtime` alongside the handle at spawn time. That lets both
/// [`Task::kill`] and [`DreamHandler::drain_pending_kills`] issue
/// [`RuntimeSpawner::cancel`] without a fresh `ctx`.
pub struct WorkerCancel {
    handle: BackgroundTaskHandle,
    runtime: Arc<dyn RuntimeSpawner>,
    tool_gate: Option<Arc<MemoryAgentToolInvoker>>,
    finishing: bool,
    done: tokio::sync::watch::Receiver<bool>,
    settlement: Arc<agent::memory_fork::MemoryForkSettlement>,
    stopping: Option<tokio::sync::watch::Receiver<Option<Result<(), String>>>>,
}

struct WorkerDone(tokio::sync::watch::Sender<bool>);
impl Drop for WorkerDone {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

/// Dream (memory-consolidation) task handler.
///
/// Holds the constructor-injected dependencies that are *not* on
/// [`TaskContext`] — the subagent spawner, the parent's `tool_invoker` +
/// `budget` (bundled into a per-spawn [`SubagentInheritance`]), the spool
/// manager, and the terminal-status sink — plus the shared worker-handle map
/// keyed by `task_id` so [`Task::kill`] can cancel the in-flight worker future.
pub struct DreamHandler {
    /// Allocates a subagent slot and pumps it to a terminal [`SubagentResult`].
    spawner: Arc<dyn SubagentSpawner>,
    /// Parent's tool invoker. Automatic dreams retain its registry behind the
    /// memory-only scope wrapper in [`SubagentInheritance`].
    tool_invoker: Arc<dyn ToolInvoker>,
    /// Parent's budget enforcer — passed through *unchanged* so budget charges
    /// aggregate across the whole agent tree (`Arc::ptr_eq` invariant).
    budget: Arc<dyn BudgetEnforcerHandle>,
    /// Owns the spool directory + path allocation for the result payload.
    output_manager: Arc<TaskOutputManager>,
    /// Where terminal status transitions / token usage are reported.
    status_sink: Arc<dyn TaskStatusSink>,
    /// `task_id` → live worker-cancel record. Populated for the duration of the
    /// spawn; natural completion or the retained stop owner removes its entry
    /// only after the worker and its pool children settle.
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    /// Task IDs queued for teardown by the synchronous
    /// [`TaskHandle::cleanup`] closure (which cannot await). Drained by
    /// [`DreamHandler::drain_pending_kills`].
    pending_kill: Arc<Mutex<HashSet<String>>>,
}

impl DreamHandler {
    /// Construct a handler with the injected dependencies.
    ///
    /// `fs` and `runtime` arrive per-call via [`TaskContext`] and are
    /// deliberately *not* injected here. `tool_invoker` + `budget` are stored
    /// so each spawn can bundle them into a [`SubagentInheritance`] (cloning the
    /// `Arc` preserves pointer identity — required by the recursion-lock +
    /// budget-aggregation invariants).
    #[must_use]
    pub fn new(
        spawner: Arc<dyn SubagentSpawner>,
        tool_invoker: Arc<dyn ToolInvoker>,
        budget: Arc<dyn BudgetEnforcerHandle>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            spawner,
            tool_invoker,
            budget,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workers: Arc::new(Mutex::new(HashMap::new())),
            pending_kill: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Attach a [`TaskStatusSink`] so terminal transitions / token usage are
    /// reported (e.g. an adapter over [`crate::registry::TaskRegistry`]).
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
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
    /// teardown so a dream subagent outliving its parent is aborted (claude-code
    /// `killTask`-on-cleanup parity). Status is flipped to `Killed` regardless.
    pub async fn drain_pending_kills(&self) {
        let pending: Vec<String> = self.pending_kill.lock().await.drain().collect();
        for task_id in pending {
            let _ = self.stop_worker(&task_id).await;
        }
    }

    async fn stop_worker(&self, task_id: &str) -> Result<bool, TaskError> {
        let mut stopped = {
            let mut workers = self.workers.lock().await;
            let Some(rec) = workers.get_mut(task_id) else {
                return Ok(true);
            };
            if rec.finishing {
                return Ok(false);
            }
            if rec
                .stopping
                .as_ref()
                .is_some_and(|done| matches!(done.borrow().as_ref(), Some(Err(_))))
            {
                rec.stopping = None;
            }
            if let Some(stopped) = &rec.stopping {
                stopped.clone()
            } else {
                if let Some(gate) = &rec.tool_gate {
                    gate.set_active(false);
                }
                let runtime = rec.runtime.clone();
                let handle = rec.handle.clone();
                let settlement = rec.settlement.clone();
                let mut done = rec.done.clone();
                let status_sink = self.status_sink.clone();
                let workers = self.workers.clone();
                let task_id = task_id.to_owned();
                let (complete, stopped) = tokio::sync::watch::channel(None);
                rec.stopping = Some(stopped.clone());
                // Every stop joins this retained transaction, even when the
                // original caller is dropped or a scheduler stop overlaps it.
                tokio::spawn(async move {
                    let result = async {
                        runtime
                            .cancel(&handle)
                            .await
                            .map_err(|error| error.to_string())?;
                        while !*done.borrow_and_update() {
                            done.changed().await.map_err(|error| error.to_string())?;
                        }
                        settlement.wait().await;
                        status_sink.set_status(&task_id, TaskStatus::Killed).await;
                        workers.lock().await.remove(&task_id);
                        Ok(())
                    }
                    .await;
                    complete.send_replace(Some(result));
                });
                stopped
            }
        };
        loop {
            if let Some(result) = stopped.borrow_and_update().clone() {
                return result.map(|()| true).map_err(TaskError::Io);
            }
            stopped.changed().await.map_err(|error| {
                TaskError::Io(format!("dream stop owner ended before settlement: {error}"))
            })?;
        }
    }
}

#[async_trait]
impl Task for DreamHandler {
    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::Dream
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        // 1. Only the Dream variant is accepted; reject the other six.
        //    `max_iterations` is recorded on `DreamTaskState` by the registry
        //    layer; the one-shot fork ignores it (TS dream is single-pass).
        let TaskSpawnInput::Dream {
            prompt,
            max_iterations: _max_iterations,
            context,
            fork_request,
            fork_wire,
            writes,
        } = input
        else {
            return Err(TaskError::Internal(
                "dream handler received a non-Dream spawn input".into(),
            ));
        };

        if fork_request.is_some() && context.is_none() {
            return Err(TaskError::Internal(
                "automatic dream requires its scoped memory and transcript directories".into(),
            ));
        }
        let consolidation_prompt = build_consolidation_prompt(&prompt, context.as_ref())?;

        // 2. Generate the task id (prefix 'd') and allocate its spool file.
        let task_id = crate::id::generate_task_id(TaskType::Dream);
        let spool_path = self
            .output_manager
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;
        // Validate UTF-8 once up front (the output manager's `append` assumes a
        // UTF-8 spool path); spool paths under the manager's dir always are.
        if spool_path.to_str().is_none() {
            return Err(TaskError::Internal("spool path is not valid UTF-8".into()));
        }

        // 3. The caller directive or source-locked renderer supplies the body.
        let mut request =
            fork_request
                .map(|request| *request)
                .unwrap_or_else(|| SubagentSpawnRequest {
                    agent_spawn_token: None,
                    stop_hook_scope:
                        lingxi_core::host::subagent_spawn::SubagentStopScope::AgentScoped,
                    agent_spawn_provenance: Default::default(),
                    parent_native_query_depth: None,
                    handback_opt_in: false,
                    parent_permission_mode: None,
                    handback_enabled: None,
                    handback_ends_turn_enabled: None,
                    restored_handback_state: None,
                    restored_handback_history: Vec::new(),
                    restore_handback_start: None,
                    teammate_color: None,
                    subagent_type: DREAM_SUBAGENT_TYPE.to_string(),
                    prompt: String::new().into(),
                    observer: None,
                    context_paths: Vec::new(),
                    // AgentTool spawn-surface parity params — the dream consolidation
                    // path sets no model/teammate/isolation/cwd override.
                    description: None,
                    model: None,
                    model_profile: None,
                    // Dream consolidation is a synchronous subagent, never a background
                    // AgentTool spawn.
                    run_in_background: false,
                    name: None,
                    team_name: None,
                    creator_teammate_name: None,
                    creator_team_name: None,
                    creator_agent_id: None,
                    mode: None,
                    isolation: None,
                    cwd: None,
                    worktree: None,
                    // Non-fork synchronous spawn.
                    fork_context_messages: None,
                    instruction_context: None,
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
                    // Top-level spawn ⇒ the spawner's own default model anchors resolution.
                    parent_model_override: None,
                    parent_model_profile_override: None,
                    forked_skill_name: None,
                    forked_skill_attribution: None,
                    forked_skill_effort: None,
                    frozen_command_denies: Vec::new(),
                    resumed_history: None,
                    max_turns_override: None,
                    max_output_tokens_per_turn: None,
                    max_input_bytes_per_turn: None,
                    query_source_label: Some("auto_dream".into()),
                    correlation_id: None,
                    model_attempt: None,
                });
        request.prompt = consolidation_prompt.clone().into();
        request.query_source_label = Some("auto_dream".into());
        request.max_turns_override = Some(DREAM_MAX_TURNS);
        request.run_in_background = false;
        if let Some(prefix) = request.fork_context_messages.as_mut() {
            // The snapshot contains the unchanged parent prefix; fork spawns
            // replay it directly, so its directive must be added here once.
            prefix.push(lingxi_core::types::ConversationMessage::user(
                lingxi_core::types::MessageId::new(),
                consolidation_prompt,
            ));
        }
        let tool_gate = context.as_ref().map(|context| {
            let shell = fork_wire
                .as_ref()
                .map_or_else(MemoryAgentShell::default, |wire| {
                    MemoryAgentShell::from_tools(&wire.tools)
                });
            Arc::new(
                MemoryAgentToolInvoker::new(
                    fork_wire
                        .as_ref()
                        .and_then(|wire| wire.tool_invoker.clone())
                        .unwrap_or_else(|| self.tool_invoker.clone()),
                    std::path::PathBuf::from(&context.memory_directory),
                )
                .with_shell(shell)
                .with_read_directory(std::path::PathBuf::from(&context.transcript_directory)),
            )
        });

        // 4. Retain the parent's budget and tool registry. The wrapper narrows
        //    this worker's allowed calls without constructing a second registry.
        let inherit = SubagentInheritance {
            tool_invoker: tool_gate.as_ref().map_or_else(
                || self.tool_invoker.clone(),
                |gate| gate.clone() as Arc<dyn ToolInvoker>,
            ),
            budget: self.budget.clone(),
        };

        // 5. Drive the subagent to completion inside a runtime-spawned worker
        //    (engine code must not call tokio::spawn directly — D17). The worker
        //    awaits `spawner.spawn`, spools the terminal payload, reports the
        //    terminal status, and removes its own cancel record on exit.
        let spawner = self.spawner.clone();
        let status_sink = self.status_sink.clone();
        let workers = self.workers.clone();
        let output_manager = self.output_manager.clone();
        let worker_spool_path = spool_path.clone();
        let worker_task_id = task_id.clone();
        let worker_tool_gate = tool_gate.clone();
        let (activation_tx, activation_rx) = if status_sink.requires_explicit_activation() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        let settlement = Arc::new(agent::memory_fork::MemoryForkSettlement::default());
        let worker_settlement = settlement.clone();
        let lease = settlement.enter();
        let done = WorkerDone(done_tx);
        let worker = Box::pin(agent::memory_fork::with_settlement(
            settlement.clone(),
            async move {
                let _lease = lease;
                let _done = done;
                if let Some(activation_rx) = activation_rx {
                    if activation_rx.await.is_err() {
                        workers.lock().await.remove(&worker_task_id);
                        return;
                    }
                }
                status_sink
                    .set_status(&worker_task_id, TaskStatus::Running)
                    .await;

                let result = match fork_wire {
                    Some(wire) => {
                        agent::memory_fork::with_wire_snapshot(
                            wire,
                            spawner.spawn(request, inherit),
                        )
                        .await
                    }
                    None => spawner.spawn(request, inherit).await,
                };
                worker_settlement.wait_for_children().await;
                if let Some(gate) = worker_tool_gate.as_ref() {
                    gate.set_active(false);
                }
                // Claim finalization while retaining a joinable record. A stop
                // that loses this claim must wait for the real terminal row.
                {
                    let mut workers = workers.lock().await;
                    let Some(worker) = workers.get_mut(&worker_task_id) else {
                        return;
                    };
                    if worker.stopping.is_some() {
                        return;
                    }
                    worker.finishing = true;
                }
                if let (Some(writes), Some(gate)) = (writes.as_ref(), worker_tool_gate.as_ref()) {
                    *writes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        gate.completed_touched_paths();
                }

                // Map the terminal SubagentResult onto a spool payload + status.
                // Spool I/O is best-effort — a spool write failure must not mask
                // the subagent result (mirrors local_bash / local_agent). The
                // shared `TaskStatusSink` has no token-usage method, so token usage
                // is surfaced by spooling a `<usage><total_tokens>…` footer.
                let (payload, status) = match &result {
                    Ok(SubagentResult::Completed { content, usage, .. }) => {
                        let body = content
                            .to_pretty_json_string()
                            .or_else(|_| content.to_json_string())
                            .expect("validated subagent projection serializes");
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

                // Routed through the output manager's `append` so the per-file 5GB
                // disk cap is enforced (T17) and the write uses O_NOFOLLOW
                // (claude-code `diskOutput.ts`, T18).
                if !payload.is_empty() {
                    let _ = output_manager.append(&worker_spool_path, &payload).await;
                }

                status_sink.set_status(&worker_task_id, status).await;
                workers.lock().await.remove(&worker_task_id);
            },
        ));

        // Serialize runtime scheduling with insertion of the cancel record. A
        // standalone sink is registered immediately, so a canned/failed spawn
        // can otherwise remove its record before this method inserts it.
        let mut workers = self.workers.lock().await;
        let bg_handle = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;

        // Record the worker-cancel handle (+ the runtime that minted it) so
        // kill / drain can cancel the in-flight worker without a fresh ctx.
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle: bg_handle,
                runtime: ctx.runtime.clone(),
                tool_gate,
                finishing: false,
                done: done_rx,
                settlement,
                stopping: None,
            },
        );
        drop(workers);

        // 6. Build the synchronous cleanup seam (claude-code `registerCleanup`
        //    parity). The closure cannot await, so it queues the task ID while
        //    retaining its joinable record; the async `drain_pending_kills`
        //    (called by the registry on agent teardown) performs the real
        //    `RuntimeSpawner::cancel`. Authoritative cancellation also flows
        //    through `Task::kill`.
        let cleanup_workers = self.workers.clone();
        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let (Ok(workers), Ok(mut pending)) =
                (cleanup_workers.try_lock(), cleanup_pending.try_lock())
            {
                if workers
                    .get(&cleanup_task_id)
                    .is_some_and(|worker| worker.finishing)
                {
                    return;
                }
                if let Some(rec) = workers.get(&cleanup_task_id) {
                    if let Some(gate) = rec.tool_gate.as_ref() {
                        gate.set_active(false);
                    }
                    pending.insert(cleanup_task_id.clone());
                }
                // If no live record remains the worker already exited; nothing
                // to queue (the terminal status was already reported).
            }
        });

        let handle = TaskHandle::new(task_id, Some(cleanup));
        Ok(match activation_tx {
            Some(activation_tx) => handle.with_activation(move || {
                let _ = activation_tx.send(());
            }),
            None => handle,
        })
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        self.stop_worker(task_id).await?;
        Ok(())
    }

    async fn request_stop(&self, task_id: &str, _ctx: TaskContext) -> Result<bool, TaskError> {
        self.stop_worker(task_id).await
    }

    async fn drain_shutdown(&self) -> Result<(), TaskError> {
        self.drain_pending_kills().await;
        let workers = self
            .workers
            .lock()
            .await
            .iter()
            .map(|(id, worker)| (id.clone(), worker.done.clone(), worker.settlement.clone()))
            .collect::<Vec<_>>();
        for (id, mut done, settlement) in workers {
            self.stop_worker(&id).await?;
            while !*done.borrow_and_update() {
                done.changed()
                    .await
                    .map_err(|error| TaskError::Io(error.to_string()))?;
            }
            settlement.wait().await;
        }
        Ok(())
    }

    fn supports_messages(&self) -> bool {
        // The TS dream takes no inbound messages — it is a one-shot fork with no
        // send path.
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::TaskStatus;
    use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
    use lingxi_core::host::tool_invoker::{
        SubagentInvocationContext, ToolInvoker, ToolInvokerError,
    };
    use lingxi_core::host::{BudgetError, SubagentSpawnError, SubagentUsage};
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use serde_json::json;
    use std::any::Any;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;
    use test_harness::mocks::MockRuntimeSpawner;
    use tokio::sync::Mutex as TokioMutex;

    // ---- In-memory FileSystem (mirrors local_agent test fixture) -----------

    struct InMemoryFs {
        files: TokioMutex<StdHashMap<String, String>>,
        append_barrier: StdMutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(StdHashMap::new()),
                append_barrier: StdMutex::new(None),
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
            let barrier = self.append_barrier.lock().unwrap().clone();
            if let Some((entered, release)) = barrier {
                entered.notify_one();
                release.notified().await;
            }
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
        Pruned(PathBuf),
        Failed(String),
        Killed,
        Err(String),
        /// Never resolves — used to keep the worker alive for the kill test.
        Pending,
    }

    struct MockSpawner {
        canned: StdMutex<Option<CannedResult>>,
        seen_request: StdMutex<Option<SubagentSpawnRequest>>,
        seen_invoker: StdMutex<Option<Arc<dyn ToolInvoker>>>,
    }
    impl MockSpawner {
        fn new(canned: CannedResult) -> Arc<Self> {
            Arc::new(Self {
                canned: StdMutex::new(Some(canned)),
                seen_request: StdMutex::new(None),
                seen_invoker: StdMutex::new(None),
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
            // Take the canned value out and DROP the guard before any await:
            // a `std::sync::MutexGuard` held across `.await` makes the future
            // `!Send`, but `SubagentSpawner::spawn` must return a `Send` future.
            let canned = self.canned.lock().unwrap().take();
            match canned {
                Some(CannedResult::Completed(content, total_tokens)) => {
                    Ok(completed_result(content, total_tokens))
                }
                Some(CannedResult::Pruned(path)) => {
                    let command = format!("rm -f '{}'", path.display());
                    inherit
                        .tool_invoker
                        .invoke(
                            "Bash",
                            Utf16JsonProjection::from_serde_value(&json!({"command": command}))
                                .unwrap(),
                            memory_call_context(),
                        )
                        .await
                        .map_err(|error| SubagentSpawnError::Runtime(error.to_string()))?;
                    Ok(completed_result(json!("pruned"), 0))
                }
                Some(CannedResult::Failed(reason)) => Ok(SubagentResult::Failed {
                    agent_id: lingxi_core::types::AgentId::new(),
                    reason,
                    usage: lingxi_core::host::subagent_spawn::SubagentUsage::default(),
                }),
                Some(CannedResult::Killed) => Ok(SubagentResult::Killed {
                    agent_id: lingxi_core::types::AgentId::new(),
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

    fn completed_result(content: serde_json::Value, total_tokens: u64) -> SubagentResult {
        SubagentResult::Completed {
            handback: None,
            agent_id: lingxi_core::types::AgentId::new(),
            content: Utf16JsonProjection::from_serde_value(&content)
                .expect("generated dream fixture is valid JSON"),
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
        }
    }

    // ---- Mock ToolInvoker / BudgetEnforcerHandle (inert) -------------------

    struct MockInvoker;
    #[async_trait]
    impl ToolInvoker for MockInvoker {
        async fn invoke(
            &self,
            name: &str,
            input: Utf16JsonProjection,
            _ctx: SubagentInvocationContext,
        ) -> Result<Utf16JsonProjection, ToolInvokerError> {
            if name == "Bash" {
                if let Some(path) = input
                    .string_at("/command")
                    .and_then(|command| command.strip_prefix("rm -f '"))
                    .and_then(|path| path.strip_suffix('\''))
                {
                    std::fs::remove_file(path)
                        .map_err(|error| ToolInvokerError::Internal(error.to_string().into()))?;
                }
            }
            Ok(Utf16JsonProjection::parse("null").unwrap())
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

    // ---- Recording status sink ---------------------------------------------

    #[derive(Default)]
    struct RecordingSink {
        statuses: StdMutex<Vec<(String, TaskStatus)>>,
        explicit_activation: bool,
    }
    #[async_trait]
    impl TaskStatusSink for RecordingSink {
        fn requires_explicit_activation(&self) -> bool {
            self.explicit_activation
        }

        async fn set_status(&self, task_id: &str, status: TaskStatus) {
            self.statuses
                .lock()
                .unwrap()
                .push((task_id.to_string(), status));
        }
    }
    impl RecordingSink {
        fn last_status(&self) -> Option<TaskStatus> {
            self.statuses.lock().unwrap().last().map(|(_, s)| *s)
        }
    }

    // ---- Helpers ------------------------------------------------------------

    fn memory_call_context() -> SubagentInvocationContext {
        SubagentInvocationContext {
            available_tool_names: None,
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            permission_pause_observer: None,
            parent_agent_id: None,
            native_query_depth: None,
            origin_session_id: None,
            instruction_context: None,
            fork_context: None,
            tool_execution_policy: lingxi_core::host::tool_invoker::ToolExecutionPolicy::Ordinary,
            agent_name: None,
            team_name: None,
            is_async: false,
            is_non_interactive_session: true,
            can_show_permission_prompts: false,
            cwd: None,
            tool_use_id: None,
            assistant_message_id: None,
            depth: 0,
            observer: None,
            parent_model: None,
            parent_model_profile: None,
            agent_spawn_provenance: Default::default(),
            tool_context_state: None,
            assistant_message: None,
            same_turn_tool_uses: Vec::new(),
            current_history: Vec::new(),
            mode_override: None,
            request_source: None,
            frozen_command_denies: Vec::new(),
        }
    }

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
    ) -> DreamHandler {
        DreamHandler::new(spawner, Arc::new(MockInvoker), Arc::new(MockBudget), mgr)
            .with_status_sink(sink)
    }

    fn dream_input(prompt: &str) -> TaskSpawnInput {
        TaskSpawnInput::Dream {
            prompt: prompt.into(),
            max_iterations: None,
            fork_request: None,
            fork_wire: None,
            writes: None,
            context: Some(DreamPromptContext {
                memory_directory: "/home/u/.lingxi/projects/-repo/memdir".into(),
                transcript_directory: "/home/u/.lingxi/projects/-repo".into(),
                additional_context: None,
                team_memory: false,
                mcp_memory: false,
            }),
        }
    }

    /// Poll the sink until it reports a terminal status (the worker runs on the
    /// `MockRuntimeSpawner`'s tokio task, so yields let it finish deterministically).
    async fn await_terminal(sink: &Arc<RecordingSink>) -> TaskStatus {
        for _ in 0..200 {
            if let Some(s) = sink.last_status() {
                if s.is_terminal() {
                    return s;
                }
            }
            tokio::task::yield_now().await;
        }
        sink.last_status().expect("worker never reported a status")
    }

    // ---- Tests --------------------------------------------------------------

    #[tokio::test]
    async fn empty_prompt_without_runtime_directories_is_rejected_before_spawn() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let handler = make_handler(spawner.clone(), mgr, Arc::new(RecordingSink::default()));
        let result = handler
            .spawn(
                TaskSpawnInput::Dream {
                    prompt: String::new(),
                    max_iterations: None,
                    context: None,
                    fork_request: None,
                    fork_wire: None,
                    writes: None,
                },
                make_ctx(fs),
            )
            .await;
        assert!(
            matches!(result, Err(TaskError::Internal(message)) if message.contains("memory and transcript directories"))
        );
        assert!(spawner.request().is_none());
    }

    #[test]
    fn explicit_prompt_keeps_whitespace_and_requires_no_directory_context() {
        let directive = "\n Consolidate 🧠 memories.\r\n{memory_directory}\n";
        assert_eq!(
            build_consolidation_prompt(directive, None).unwrap(),
            directive
        );
    }

    #[test]
    fn dream_turn_cap_matches_native_fork_default() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/fixtures/dream_319.json"
        ))
        .unwrap();
        assert_eq!(
            fixture["default_max_turns"].as_u64(),
            Some(u64::from(DREAM_MAX_TURNS))
        );
    }

    #[tokio::test]
    async fn spawn_rejects_wrong_input_variant() {
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
                },
                make_ctx(fs),
            )
            .await;
        match result {
            Err(TaskError::Internal(_)) => {}
            Err(other) => panic!("expected Internal, got {other:?}"),
            Ok(_) => panic!("non-Dream input must be rejected"),
        }
    }

    #[tokio::test]
    async fn worker_waits_for_registry_publication_before_starting_dream() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Err("pool full".into()));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink {
            explicit_activation: true,
            ..Default::default()
        });
        let handler = make_handler(spawner.clone(), mgr, sink.clone());

        let mut handle = handler
            .spawn(dream_input("scheduled dream"), make_ctx(fs))
            .await
            .expect("handler schedules the dream worker");

        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(
            spawner.request().is_none(),
            "Dream must not consume a pool slot before its registry row exists"
        );
        assert_eq!(sink.last_status(), None);

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
        .expect("dream worker should proceed after registry publication");
        assert_eq!(status, TaskStatus::Failed);
        assert!(spawner.request().is_some());
    }

    #[tokio::test]
    async fn dropping_unactivated_handle_cancels_prepared_dream() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("unused"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink {
            explicit_activation: true,
            ..Default::default()
        });
        let handler = make_handler(spawner.clone(), mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(dream_input("cancel before commit"), make_ctx(fs))
            .await
            .expect("handler prepares the dream worker");
        drop(handle);

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !workers.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("dropping the activation owner stops the prepared dream");
        assert!(spawner.request().is_none());
        assert_eq!(sink.last_status(), None);
    }

    #[tokio::test]
    async fn completed_result_spools_json_and_usage_footer() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(
            json!({ "consolidated": 3, "ok": true }),
            777,
        ));
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner.clone(), mgr.clone(), sink.clone());

        let handle = handler
            .spawn(dream_input(""), make_ctx(fs))
            .await
            .expect("spawn should succeed");

        assert!(handle.task_id.starts_with('d'), "Dream id prefix is 'd'");
        assert!(handle.cleanup.is_some(), "cleanup seam is present");

        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);

        // Empty caller prompt renders the real runtime directory context.
        let req = spawner.request().expect("spawner must have been called");
        assert_eq!(req.subagent_type, "general-purpose");
        assert!(
            req.prompt
                .string_at("")
                .is_some_and(|prompt| prompt.contains("# Dream: Memory Consolidation")),
            "source-locked consolidation prompt seeded"
        );
        assert!(req.context_paths.is_empty());
        assert_eq!(req.query_source_label.as_deref(), Some("auto_dream"));
        assert_eq!(
            req.prompt.string_at(""),
            Some(
                render_consolidation_prompt(&DreamPromptContext {
                    memory_directory: "/home/u/.lingxi/projects/-repo/memdir".into(),
                    transcript_directory: "/home/u/.lingxi/projects/-repo".into(),
                    additional_context: None,
                    team_memory: false,
                    mcp_memory: false,
                })
                .as_str()
            )
        );

        // The Completed content was spooled (pretty JSON + usage footer).
        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(
            read.content.contains("\"consolidated\""),
            "content key spooled"
        );
        assert!(
            read.content.contains("<total_tokens>777</total_tokens>"),
            "token usage spooled in the usage footer"
        );
    }

    #[tokio::test]
    async fn failed_result_maps_to_failed_and_spools_reason() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Failed("consolidation refused".into()));
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr.clone(), sink.clone());

        let handle = handler.spawn(dream_input(""), make_ctx(fs)).await.unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);

        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(
            read.content.contains("consolidation refused"),
            "reason spooled"
        );
        // No usage footer on the failed path.
        assert!(!read.content.contains("<total_tokens>"));
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

        let handle = handler.spawn(dream_input(""), make_ctx(fs)).await.unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);

        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(read.content.contains("pool full"), "spawn error spooled");
    }

    #[tokio::test]
    async fn non_empty_prompt_is_used_verbatim() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("ok"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner.clone(), mgr, sink.clone());

        handler
            .spawn(dream_input("custom dream directive"), make_ctx(fs))
            .await
            .unwrap();

        await_terminal(&sink).await;
        assert_eq!(
            spawner.request().unwrap().prompt.string_at(""),
            Some("custom dream directive"),
            "a non-empty caller prompt is forwarded verbatim"
        );
    }

    #[tokio::test]
    async fn automatic_dream_keeps_parent_prefix_system_prompt_and_provider_route() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let seed_spawner = MockSpawner::new(CannedResult::Completed(json!("seed"), 0));
        let (_seed_dir, seed_mgr) = make_output_manager(fs.clone());
        let seed_sink = Arc::new(RecordingSink::default());
        let seed_handler = make_handler(seed_spawner.clone(), seed_mgr, seed_sink.clone());
        seed_handler
            .spawn(dream_input("seed template"), make_ctx(fs.clone()))
            .await
            .unwrap();
        assert_eq!(await_terminal(&seed_sink).await, TaskStatus::Completed);
        let mut template = seed_spawner.request().unwrap();
        let prefix = lingxi_core::types::ConversationMessage::user(
            lingxi_core::types::MessageId::new(),
            "unchanged parent prefix 🧠".into(),
        );
        template.fork_context_messages = Some(vec![prefix.clone()]);
        template.fork_parent_system_prompt = Some("already rendered system prompt".into());
        template.parent_model_override = Some("custom-model".into());
        template.parent_model_profile_override = Some("provider-profile".into());
        template.model = Some("custom-model".into());
        template.model_profile = Some("provider-profile".into());
        template.parent_permission_mode = Some("plan".into());
        template.cwd = Some("/workspace/repo".into());
        template.run_in_background = true;
        template.max_turns_override = Some(3);

        let completed_writes = Arc::new(StdMutex::new(vec![PathBuf::from("stale.md")]));
        let mut input = dream_input("");
        let TaskSpawnInput::Dream {
            fork_request,
            writes,
            ..
        } = &mut input
        else {
            unreachable!()
        };
        *fork_request = Some(Box::new(template));
        *writes = Some(completed_writes.clone());
        let spawner = MockSpawner::new(CannedResult::Completed(json!("consolidated"), 0));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner.clone(), mgr, sink.clone());
        let handle = handler.spawn(input, make_ctx(fs.clone())).await.unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        let request = spawner.request().unwrap();
        assert_eq!(
            request.parent_model_override.as_deref(),
            Some("custom-model")
        );
        assert_eq!(
            request.parent_model_profile_override.as_deref(),
            Some("provider-profile")
        );
        assert_eq!(request.model.as_deref(), Some("custom-model"));
        assert_eq!(request.model_profile.as_deref(), Some("provider-profile"));
        assert_eq!(request.parent_permission_mode.as_deref(), Some("plan"));
        assert_eq!(request.cwd.as_deref(), Some("/workspace/repo"));
        assert_eq!(
            request
                .fork_parent_system_prompt
                .as_ref()
                .unwrap()
                .string_at(""),
            Some("already rendered system prompt")
        );
        assert!(!request.run_in_background);
        assert_eq!(request.max_turns_override, Some(DREAM_MAX_TURNS));
        let messages = request.fork_context_messages.as_ref().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].id(), prefix.id());
        assert_eq!(messages[0].text_content(), prefix.text_content());
        assert_eq!(
            messages[1].text_content(),
            request.prompt.string_at("").unwrap()
        );
        assert!(spawner
            .seen_invoker
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .as_any()
            .is::<MemoryAgentToolInvoker>());
        assert!(
            completed_writes.lock().unwrap().is_empty(),
            "write paths are published before terminal status"
        );

        let statuses_before = sink.statuses.lock().unwrap().len();
        handler.kill(&handle.task_id, make_ctx(fs)).await.unwrap();
        assert_eq!(sink.last_status(), Some(TaskStatus::Completed));
        assert_eq!(
            sink.statuses.lock().unwrap().len(),
            statuses_before,
            "late kill is a no-op after completion wins"
        );
    }

    #[tokio::test]
    async fn successful_memory_removal_is_reported_before_terminal_status() {
        let memory = tempdir().unwrap();
        let memory_dir = memory.path().canonicalize().unwrap();
        let path = memory_dir.join("stale note.md");
        std::fs::write(&path, "stale").unwrap();
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Pruned(path.clone()));
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());
        let handler = make_handler(spawner, mgr, sink.clone());
        let changed = Arc::new(StdMutex::new(Vec::new()));
        let mut input = dream_input("");
        let TaskSpawnInput::Dream {
            context, writes, ..
        } = &mut input
        else {
            unreachable!()
        };
        context.as_mut().unwrap().memory_directory = memory_dir.to_string_lossy().into_owned();
        *writes = Some(changed.clone());
        handler.spawn(input, make_ctx(fs)).await.unwrap();
        assert_eq!(await_terminal(&sink).await, TaskStatus::Completed);
        assert!(!path.exists());
        assert_eq!(*changed.lock().unwrap(), [path]);
    }

    #[tokio::test]
    async fn concurrent_memory_dream_stops_join_worker_and_child_even_if_first_waiter_is_dropped() {
        struct CancelGate {
            inner: MockRuntimeSpawner,
            entered: tokio::sync::Semaphore,
            release: tokio::sync::Semaphore,
            calls: std::sync::atomic::AtomicUsize,
        }
        #[async_trait]
        impl RuntimeSpawner for CancelGate {
            async fn spawn(
                &self,
                name: &str,
                work: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
            ) -> Result<BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
                self.inner.spawn(name, work).await
            }
            async fn sleep(&self, duration: std::time::Duration) {
                self.inner.sleep(duration).await;
            }
            async fn cancel(
                &self,
                handle: &BackgroundTaskHandle,
            ) -> Result<(), lingxi_core::host::RuntimeError> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.entered.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                self.inner.cancel(handle).await
            }
        }
        let fs = Arc::new(InMemoryFs::new());
        let runtime = Arc::new(CancelGate {
            inner: MockRuntimeSpawner::default(),
            entered: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let (_dir, manager) = make_output_manager(fs.clone());
        let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
        let handler = Arc::new(make_handler(
            MockSpawner::new(CannedResult::Pending),
            manager.clone(),
            sink.clone(),
        ));
        let mut registry = crate::registry::TaskRegistry::new(runtime.clone(), fs, manager);
        registry.register_handler(TaskType::Dream, handler.clone());
        let registry = Arc::new(registry);
        sink.bind(registry.clone());
        let id = registry
            .spawn(
                TaskType::Dream,
                dream_input("consolidate"),
                "dreaming".into(),
            )
            .await
            .unwrap();
        // Stand in for a pool child whose runtime has acknowledged cancellation
        // but still retains actual execution. The pool regression verifies the
        // production allocation path creates exactly this lease.
        let child = handler.workers.lock().await[&id].settlement.enter();
        let first_registry = registry.clone();
        let first_id = id.clone();
        let first =
            tokio::spawn(async move { first_registry.kill_with_reason(&first_id, "user").await });
        runtime.entered.acquire().await.unwrap().forget();
        first.abort();
        let _ = first.await;
        let second = registry.kill(&id);
        tokio::pin!(second);
        assert!(futures::poll!(&mut second).is_pending());
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Running
        );
        runtime.release.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if *handler.workers.lock().await[&id].done.borrow() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            futures::poll!(&mut second).is_pending(),
            "wrapper exit must not settle the retained child"
        );
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Running
        );
        assert_eq!(runtime.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(child);
        tokio::time::timeout(std::time::Duration::from_secs(2), second)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Killed
        );
        assert!(handler.workers.lock().await.is_empty());
    }

    #[tokio::test]
    async fn registry_stop_joins_a_dream_finishing_its_spool_without_marking_it_killed() {
        let fs = Arc::new(InMemoryFs::new());
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *fs.append_barrier.lock().unwrap() = Some((entered.clone(), release.clone()));
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let (_dir, manager) = make_output_manager(fs.clone());
        let sink = Arc::new(crate::registry_status_sink::RegistryStatusSink::new());
        let handler = Arc::new(make_handler(
            MockSpawner::new(CannedResult::Completed(json!("consolidated"), 0)),
            manager.clone(),
            sink.clone(),
        ));
        let mut registry = crate::registry::TaskRegistry::new(runtime, fs, manager);
        registry.register_handler(TaskType::Dream, handler.clone());
        let registry = Arc::new(registry);
        sink.bind(registry.clone());
        let id = registry
            .spawn(
                TaskType::Dream,
                dream_input("consolidate"),
                "dreaming".into(),
            )
            .await
            .unwrap();
        entered.notified().await;
        assert!(handler.workers.lock().await.get(&id).unwrap().finishing);
        let kill = registry.kill(&id);
        tokio::pin!(kill);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(kill.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Running
        );
        release.notify_one();
        kill.await.unwrap();
        handler.drain_shutdown().await.unwrap();
        assert_eq!(
            registry.get(&id).await.unwrap().base().status,
            TaskStatus::Completed
        );
        assert!(handler.workers.lock().await.is_empty());
    }

    #[tokio::test]
    async fn spawn_then_kill_lifecycle() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        // The spawner parks forever, so the worker stays alive until kill.
        let spawner = MockSpawner::new(CannedResult::Pending);
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = make_handler(spawner, mgr, sink.clone());
        let workers = handler.workers_map();

        let handle = handler
            .spawn(dream_input(""), make_ctx(fs.clone()))
            .await
            .unwrap();
        assert!(handle.task_id.starts_with('d'), "Dream id prefix is 'd'");

        // Let the worker reach its parked await and register its cancel record.
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
            .kill(&handle.task_id, make_ctx(fs.clone()))
            .await
            .expect("kill should succeed");

        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
        assert!(
            !workers.lock().await.contains_key(&handle.task_id),
            "cancel record removed by kill"
        );

        // Killing an unknown id is a graceful no-op success.
        handler
            .kill("ddeadbeef", make_ctx(fs))
            .await
            .expect("kill of unknown task is a no-op success");
    }

    #[test]
    fn name_and_type_are_dream() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let spawner = MockSpawner::new(CannedResult::Killed);
        let (_dir, mgr) = make_output_manager(fs);
        let handler = make_handler(spawner, mgr, Arc::new(RecordingSink::default()));
        assert_eq!(handler.name(), "dream");
        assert_eq!(handler.task_type(), TaskType::Dream);
        assert!(!handler.supports_messages());
    }
}
