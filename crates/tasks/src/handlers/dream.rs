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
//! ## The Rust `Dream` variant carries no `memoryRoot` / `transcriptDir`
//!
//! The TS `buildConsolidationPrompt` interpolates `${memoryRoot}`,
//! `${transcriptDir}`, and `${extra}` into the 4-phase consolidation markdown.
//! The Rust [`TaskSpawnInput::Dream`] variant carries only `prompt: String` and
//! `max_iterations: Option<u32>` — there is no source for those paths — so this
//! handler seeds the subagent as follows: a non-empty caller-supplied `prompt`
//! is used verbatim (the manual `/dream` path), otherwise a static consolidation
//! prompt body is used. The static body reproduces the rendered TS phases —
//! inlining the `${ENTRYPOINT_NAME}`/`${MAX_ENTRYPOINT_LINES}` constants as the
//! literals `MEMORY.md`/`200` — and drops ONLY the runtime
//! `${memoryRoot}`/`${transcriptDir}`/`${extra}` interpolations the variant
//! cannot supply (mirroring how the local-agent handler passes an empty
//! `context_paths`). `max_iterations` is recorded on `DreamTaskState` by the
//! registry layer; the one-shot fork ignores it (TS dream is single-pass —
//! `iteration_count` exists for the deferred cron/auto-dream *scheduler*, not the
//! handler).

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use async_trait::async_trait;
use platform_api::{
    BackgroundTaskHandle, BudgetEnforcerHandle, RuntimeSpawner, SubagentInheritance,
    SubagentResult, SubagentSpawnRequest, SubagentSpawner, ToolInvoker,
};
use std::collections::HashMap;
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

/// Build the consolidation prompt seeded into the forked dream subagent.
///
/// Ports `buildConsolidationPrompt`
/// (`claude-code/src/services/autoDream/consolidationPrompt.ts`): the
/// `# Dream: Memory Consolidation` 4-phase (Orient / Gather / Consolidate /
/// Prune) markdown. A non-empty caller `prompt` (manual `/dream` path) is used
/// verbatim; an empty one falls back to the static body. The static body inlines
/// the TS `${ENTRYPOINT_NAME}` / `${MAX_ENTRYPOINT_LINES}` constants verbatim as
/// the literals `MEMORY.md` / `200` (with the hard-coded `~25KB` / `~150` /
/// `~200` thresholds), and drops ONLY the runtime `${memoryRoot}` /
/// `${transcriptDir}` / `${extra}` interpolations the Rust `Dream` variant
/// cannot supply — the transcript `grep` example uses a `<transcript-dir>`
/// placeholder in their place (see the module docs).
fn build_consolidation_prompt(prompt: &str) -> String {
    if !prompt.is_empty() {
        return prompt.to_string();
    }
    // Static fallback — the rendered TS prompt with the entrypoint constants
    // inlined; only the runtime path/extra interpolations are dropped.
    "# Dream: Memory Consolidation\n\
\n\
You are performing a dream — a reflective pass over your memory files. \
Synthesize what you've learned recently into durable, well-organized memories \
so that future sessions can orient quickly.\n\
\n\
---\n\
\n\
## Phase 1 — Orient\n\
\n\
- `ls` the memory directory to see what already exists\n\
- Read `MEMORY.md` to understand the current index\n\
- Skim existing topic files so you improve them rather than creating duplicates\n\
- If `logs/` or `sessions/` subdirectories exist (assistant-mode layout), review recent entries there\n\
\n\
## Phase 2 — Gather recent signal\n\
\n\
Look for new information worth persisting. Sources in rough priority order:\n\
\n\
1. **Daily logs** (`logs/YYYY/MM/YYYY-MM-DD.md`) if present — these are the append-only stream\n\
2. **Existing memories that drifted** — facts that contradict something you see in the codebase now\n\
3. **Transcript search** — if you need specific context (e.g., \"what was the error message from yesterday's build failure?\"), grep the JSONL transcripts for narrow terms:\n\
   `grep -rn \"<narrow term>\" <transcript-dir>/ --include=\"*.jsonl\" | tail -50`\n\
\n\
Don't exhaustively read transcripts. Look only for things you already suspect matter.\n\
\n\
## Phase 3 — Consolidate\n\
\n\
For each thing worth remembering, write or update a memory file at the top level \
of the memory directory. Use the memory file format and type conventions from \
your system prompt's auto-memory section — it's the source of truth for what to \
save, how to structure it, and what NOT to save.\n\
\n\
Focus on:\n\
- Merging new signal into existing topic files rather than creating near-duplicates\n\
- Converting relative dates (\"yesterday\", \"last week\") to absolute dates so they remain interpretable after time passes\n\
- Deleting contradicted facts — if today's investigation disproves an old memory, fix it at the source\n\
\n\
## Phase 4 — Prune and index\n\
\n\
Update `MEMORY.md` so it stays under 200 lines AND under ~25KB. It's an \
**index**, not a dump — each entry should be one line under ~150 characters: \
`- [Title](file.md) — one-line hook`. Never write memory content directly \
into it.\n\
\n\
- Remove pointers to memories that are now stale, wrong, or superseded\n\
- Demote verbose entries: if an index line is over ~200 chars, it's carrying content that belongs in the topic file — shorten the line, move the detail\n\
- Add pointers to newly important memories\n\
- Resolve contradictions — if two files disagree, fix the wrong one\n\
\n\
---\n\
\n\
Return a brief summary of what you consolidated, updated, or pruned. If nothing \
changed (memories are already tight), say so.\n"
        .to_string()
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
    /// Parent's tool invoker — passed through *unchanged* in
    /// [`SubagentInheritance`] (the recursion lock relies on `Arc::ptr_eq`).
    tool_invoker: Arc<dyn ToolInvoker>,
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
    /// `task_id` → record queued for teardown by the synchronous
    /// [`TaskHandle::cleanup`] closure (which cannot await). Drained by
    /// [`DreamHandler::drain_pending_kills`].
    pending_kill: Arc<Mutex<HashMap<String, WorkerCancel>>>,
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
            pending_kill: Arc::new(Mutex::new(HashMap::new())),
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
        let pending: Vec<(String, WorkerCancel)> = self.pending_kill.lock().await.drain().collect();
        for (task_id, rec) in pending {
            let _ = rec.runtime.cancel(&rec.handle).await;
            self.status_sink
                .set_status(&task_id, TaskStatus::Killed)
                .await;
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
        } = input
        else {
            return Err(TaskError::Internal(
                "dream handler received a non-Dream spawn input".into(),
            ));
        };

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

        // 3. Build the spawn request. The consolidation prompt is built from the
        //    caller prompt (manual `/dream`) or the static fallback body.
        //    `context_paths` has no source on the variant, so an empty vec is
        //    passed.
        let request = SubagentSpawnRequest {
            teammate_color: None,
            subagent_type: DREAM_SUBAGENT_TYPE.to_string(),
            prompt: build_consolidation_prompt(&prompt),
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

        // 4. Bundle the inheritance. Cloning the Arcs preserves pointer
        //    identity — required by the recursion-lock + budget-aggregation
        //    invariants (the spawner asserts `Arc::ptr_eq` on these).
        let inherit = SubagentInheritance {
            tool_invoker: self.tool_invoker.clone(),
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
        let (activation_tx, activation_rx) = if status_sink.requires_explicit_activation() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let worker = Box::pin(async move {
            if let Some(activation_rx) = activation_rx {
                if activation_rx.await.is_err() {
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
            // the subagent result (mirrors local_bash / local_agent). The
            // shared `TaskStatusSink` has no token-usage method, so token usage
            // is surfaced by spooling a `<usage><total_tokens>…` footer.
            let (payload, status) = match &result {
                Ok(SubagentResult::Completed { content, usage, .. }) => {
                    let body = serde_json::to_string_pretty(content)
                        .unwrap_or_else(|_| content.to_string());
                    let body = format!(
                        "{body}\n<usage><total_tokens>{}</total_tokens></usage>\n",
                        usage.total_tokens
                    );
                    (body, TaskStatus::Completed)
                }
                Ok(SubagentResult::Failed { reason, .. }) => (reason.clone(), TaskStatus::Failed),
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

            // The subagent has terminated; drop the cancel record so a late
            // kill is a graceful no-op (claude-code `status !== 'running'`).
            workers.lock().await.remove(&worker_task_id);
        });

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
            },
        );
        drop(workers);

        // 6. Build the synchronous cleanup seam (claude-code `registerCleanup`
        //    parity). The closure cannot await, so it moves any live cancel
        //    record into `pending_kill`; the async `drain_pending_kills`
        //    (called by the registry on agent teardown) performs the real
        //    `RuntimeSpawner::cancel`. Authoritative cancellation also flows
        //    through `Task::kill`.
        let cleanup_workers = self.workers.clone();
        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let (Ok(mut workers), Ok(mut pending)) =
                (cleanup_workers.try_lock(), cleanup_pending.try_lock())
            {
                if let Some(rec) = workers.remove(&cleanup_task_id) {
                    pending.insert(cleanup_task_id.clone(), rec);
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
        // Recover the live worker-cancel record (if any) and cancel the
        // in-flight worker future — the analogue of TS `abortController.abort()`.
        // An absent record ⇒ the subagent already terminated ⇒ graceful no-op
        // (TS `DreamTask.kill`: `if task.status !== 'running' return`).
        //
        // NOTE: TS `DreamTask.kill` also calls `rollbackConsolidationLock`
        // (rewinds the consolidation-lock mtime so the next session can retry).
        // There is NO consolidation-lock concept in the Rust tree — that lock
        // belongs to the deferred auto-dream *scheduler* (`autoDream.ts`), not
        // the handler — so the rollback is intentionally omitted here.
        let rec = self.workers.lock().await.remove(task_id);
        if let Some(rec) = rec {
            rec.runtime
                .cancel(&rec.handle)
                .await
                .map_err(|e| TaskError::Io(e.to_string()))?;
        }
        // Flip status to Killed regardless (best-effort; a worker that already
        // reported a terminal status simply gets a redundant Killed).
        self.status_sink
            .set_status(task_id, TaskStatus::Killed)
            .await;
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

    // ---- In-memory FileSystem (mirrors local_agent test fixture) -----------

    struct InMemoryFs {
        files: TokioMutex<StdHashMap<String, String>>,
    }
    impl InMemoryFs {
        fn new() -> Self {
            Self {
                files: TokioMutex::new(StdHashMap::new()),
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
    }
    impl MockSpawner {
        fn new(canned: CannedResult) -> Arc<Self> {
            Arc::new(Self {
                canned: StdMutex::new(Some(canned)),
                seen_request: StdMutex::new(None),
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
            _inherit: SubagentInheritance,
        ) -> Result<SubagentResult, SubagentSpawnError> {
            *self.seen_request.lock().unwrap() = Some(request);
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
                    scope: None,
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

        // Empty caller prompt ⇒ the static consolidation body is used.
        let req = spawner.request().expect("spawner must have been called");
        assert_eq!(req.subagent_type, "general-purpose");
        assert!(
            req.prompt.contains("# Dream: Memory Consolidation"),
            "static consolidation prompt seeded"
        );
        assert!(req.context_paths.is_empty());

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
            spawner.request().unwrap().prompt,
            "custom dream directive",
            "a non-empty caller prompt is forwarded verbatim"
        );
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
