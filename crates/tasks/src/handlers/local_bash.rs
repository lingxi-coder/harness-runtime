//! Local-bash task handler — M2 implementation.
//!
//! Spawns a `bash -c "<command>"` child through the [`Sandbox`] +
//! [`ProcessRunner`] traits (the D2 / spec A1 sandbox-decision invariant) and
//! streams its captured stdout/stderr into the task's spool file, mirroring the
//! claude-code `LocalShellTask` lifecycle (`spawnShellTask` → `killTask`).
//!
//! ## One child, both streamed-to-spool and killable
//!
//! claude-code's `LocalShellTask` owns a single `ShellCommand`: the SAME process
//! whose output streams to disk (`shellCommand.background(taskId)`) is the one
//! `killTask` later signals (`shellCommand.kill()`). There is no second child;
//! `background()` only transitions the already-running process — it does not
//! re-execute the command (`LocalShellTask.tsx` / `killShellTasks.ts`).
//!
//! This handler mirrors that single-child invariant. [`crate::task_trait::Task`]
//! hands the handler a [`TaskContext`] carrying only `fs` + `runtime`; the
//! engine must not call `tokio::spawn` directly (D17), so the one child runs via
//! [`ProcessRunner::run`] inside a single `ctx.runtime.spawn(..)` worker future.
//! The captured [`ProcessOutput`] is appended to the spool via
//! `FileSystem::append_file_no_follow`.
//!
//! [`Task::kill`] terminates that exact child by cancelling the worker future
//! through [`RuntimeSpawner::cancel`] — the analogue of TS
//! `shellCommand.kill()`. Cancelling the worker drops the `run()` future, which
//! drops the platform `Child`; the foreground runner spawns with
//! `kill_on_drop(true)` (see `platforms/posix/src/process/runner.rs`), so the
//! drop sends `SIGKILL` to the real OS process. The in-flight `bash -c` is thus
//! authoritatively killed — not merely flagged. The worker-cancel record (the
//! [`BackgroundTaskHandle`] + the `runtime` that minted it) is captured at spawn
//! time because [`TaskContext`] is per-call and the synchronous
//! [`TaskHandle::cleanup`] closure receives no `ctx`. This is exactly the seam
//! the sibling [`crate::handlers::local_agent::LocalAgentHandler`] uses.
//!
//! There is deliberately NO [`ProcessRunner::spawn_background`] call: that would
//! launch a *second*, independent OS child (detached `setsid`, output to a
//! runner-private file) for one logical task — double-executing any
//! side-effecting command. The single `run()` child is the whole task.

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use async_trait::async_trait;
use lingxi_core::host::{
    BackgroundTaskHandle, ProcessCommand, ProcessRunner, RuntimeSpawner, Sandbox,
};
use mobile_linux_api::{ProcessError, ProcessOutput};

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

/// A worker-cancel record: the [`BackgroundTaskHandle`] returned by
/// [`RuntimeSpawner::spawn`] plus the `runtime` Arc that minted it.
///
/// [`TaskContext`] (which carries `runtime`) is per-call and the synchronous
/// [`TaskHandle::cleanup`] closure receives no `ctx`, so the handler captures
/// the `runtime` alongside the handle at spawn time. That lets both
/// [`Task::kill`] and [`LocalBashHandler::drain_pending_kills`] issue
/// [`RuntimeSpawner::cancel`] without a fresh `ctx` — cancelling the in-flight
/// `run()` future, which drops the platform `Child` and (via the runner's
/// `kill_on_drop`) `SIGKILL`s the real OS process. Mirrors the sibling
/// `LocalAgentHandler::WorkerCancel`.
///
/// Public because it appears in the signature of the public
/// [`LocalBashHandler::workers_map`] accessor; fields stay private.
pub struct WorkerCancel {
    handle: BackgroundTaskHandle,
    runtime: Arc<dyn RuntimeSpawner>,
}

/// Audit reason recorded on the sandboxed command when no policy is supplied,
/// mirroring the hooks Command arm's `"hook_command"` bypass tag
/// (`hooks/src/executor.rs:276`).
const BYPASS_REASON: &str = "local_bash_task";

/// Handler name reported by [`Task::name`] / used as the runtime task-name
/// prefix. Byte-aligned with the `"local_bash"` wire string in `handle.rs`.
const HANDLER_NAME: &str = "local_bash";

/// Sink for terminal-status / exit-code updates produced by the background
/// worker once the child completes (or is killed).
///
/// The handler does not own a [`crate::registry::TaskRegistry`] reference
/// (it would be a cyclic / over-broad dependency), so terminal transitions are
/// reported through this narrow seam instead. The wire-step that registers the
/// handler can plug in an adapter that calls `registry.set_status(..)` and
/// writes `LocalBashTaskState.exit_code`. The default [`NoopStatusSink`] makes
/// the handler usable standalone (and in unit tests).
#[async_trait]
pub trait TaskStatusSink: Send + Sync {
    async fn set_agent_display(&self, _id: &str, _model: String, _effort: Option<String>) {}

    /// Bind a prepared task to the real pool identity before its first model call.
    async fn bind_agent_id(
        &self,
        _task_id: &str,
        _agent_id: lingxi_core::types::AgentId,
    ) -> Result<(), String> {
        Ok(())
    }
    /// Shared registry for recipient-scoped child notifications.
    fn task_registry(
        &self,
    ) -> Option<Arc<dyn lingxi_core::host::task_registry::TaskRegistryHandle>> {
        None
    }
    /// Whether handler workers must remain prepared-but-paused until their
    /// returned [`TaskHandle`](crate::task_trait::TaskHandle) is activated by
    /// the owning registry. Registry-backed sinks opt in; standalone/test sinks
    /// keep the historical immediate-start behavior by default.
    fn requires_explicit_activation(&self) -> bool {
        false
    }

    /// Record the task's final status. Called exactly once per task on
    /// completion, failure, timeout, or kill.
    async fn set_status(&self, task_id: &str, status: TaskStatus);

    /// Publish a successful local-Agent completion and Native `finalizing`
    /// fact in one task-row mutation. Other status transitions use
    /// [`Self::set_status`].
    async fn set_agent_status_with_finalizing(
        &self,
        task_id: &str,
        status: TaskStatus,
        _finalizing: bool,
    ) {
        self.set_status(task_id, status).await;
    }

    /// Atomically publish a persistent Agent rest with its Native finalizing
    /// state. The registry clears that state after it drains the matching
    /// result notification.
    async fn notify_rest_with_finalizing(
        &self,
        task_id: &str,
        result: Option<String>,
        usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
        agent_id: Option<lingxi_core::types::AgentId>,
        agent_name: Option<String>,
        team_name: Option<String>,
        run: Option<lingxi_core::host::handback::HandbackRunKey>,
        _finalizing: bool,
    ) {
        self.notify_rest(task_id, result, usage, agent_id, agent_name, team_name, run)
            .await;
    }

    /// Apply one source-owned local-Agent list fact update without deriving
    /// neighboring facts from task status.
    async fn update_agent_list_local_fact(
        &self,
        agent_id: lingxi_core::types::AgentId,
        update: lingxi_core::host::task_registry::AgentListLocalFactUpdate,
    ) {
        if let Some(registry) = self.task_registry() {
            let _ = registry
                .update_agent_list_local_fact(agent_id, update)
                .await;
        }
    }

    /// Publish an explicit coordinator membership activity transition for an
    /// in-process teammate. Registry-backed sinks resolve the exact team/name
    /// from the stored teammate row; standalone sinks have no team writer.
    async fn set_team_member_active(
        &self,
        agent_id: lingxi_core::types::AgentId,
        active: bool,
    ) -> Result<(), String> {
        if let Some(registry) = self.task_registry() {
            registry
                .set_team_member_active(agent_id, active)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// A persistent teammate finished a turn and remains available for messages.
    async fn set_teammate_idle(&self, _task_id: &str) {}

    /// Teammate plan-review wait, independent of running/idle lifecycle state.
    async fn set_awaiting_plan_approval(&self, _task_id: &str, _awaiting: bool) {}

    /// Record a FAILED terminal status together with the failure reason.
    ///
    /// claude-code 2.1.198: a teammate dying on an API error reports "failed"
    /// to the team lead WITH the reason — the in-process runner's catch block
    /// sends a failed idle notification `{idleReason:"failed",
    /// completedStatus:"failed", failureReason}` to the leader (binary
    /// @216293689), and `zTt` caps `failureReason` at 200 chars (@215144889,
    /// `kd(t.failureReason).slice(0,RXn)`, `RXn=200` @215149471).
    /// [`TaskStatus::Failed`] is payload-less, so the reason travels through
    /// this dedicated defaulted method instead (frozen-trait idiom). The
    /// default forwards to [`Self::set_status`] (reason dropped) so existing
    /// sinks keep their exact behavior; sinks with a lead-facing surface (the
    /// coordinator's team registry) override it.
    async fn set_failed(&self, task_id: &str, _error: &str) {
        self.set_status(task_id, TaskStatus::Failed).await;
    }

    /// Record the child's exit code (the value from
    /// [`ProcessOutput::exit_code`]). Called once on natural completion.
    async fn set_exit_code(&self, _task_id: &str, _exit_code: i32) {}

    /// Report how many bytes a MONITOR's script wrote to stdout over its life
    /// (claude-code `taskOutput.pipedStdoutBytes`). Defaulted so every existing
    /// sink compiles unchanged; only the monitor worker calls it.
    async fn set_monitor_stdout_bytes(&self, _task_id: &str, _bytes: u64) {}

    /// Signal that a PERSISTENT task came to rest: it produced a turn-set result
    /// and PARKED (still alive, awaiting the next message). Unlike
    /// [`Self::set_status`] this does NOT mark the task terminal — it arms a
    /// one-shot "came to rest" notification the registry surfaces once (the
    /// model's "you will be notified" promise for a backgrounded agent),
    /// re-armed on each subsequent rest. Defaulted to a no-op (claude-code:
    /// the `<note>` fires "each time this agent comes to rest").
    ///
    /// `result` is the agent's final-text response and `usage` its run usage,
    /// surfaced as the optional `<result>` / `<usage>` notification sections (the
    /// binary `enqueueAgentNotification` always passes them when a result exists).
    /// A delayed completion may park only the run that produced it.
    async fn notify_rest(
        &self,
        _task_id: &str,
        _result: Option<String>,
        _usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
        _agent_id: Option<lingxi_core::types::AgentId>,
        _agent_name: Option<String>,
        _team_name: Option<String>,
        _run: Option<lingxi_core::host::handback::HandbackRunKey>,
    ) {
    }

    /// Report a TERMINATING `local_agent`'s notification payload — final text,
    /// usage, failure reason, and the kept-worktree coordinates.
    ///
    /// The counterpart of [`Self::notify_rest`] for the terminal case. Both the
    /// resting and the terminal notification carry `<result>`/`<usage>` in
    /// claude-code (`enqueueAgentNotification` receives `finalMessage`, `usage`
    /// and the `...getWorktreeResult()` spread alongside the status), but only
    /// the rest path had a seam here — so a terminating background agent
    /// reported a bare status and the model never saw its answer.
    ///
    /// Call this BEFORE the terminal [`Self::set_status`]: the registry's drain
    /// is terminal-gated, so the reverse order can publish a notification whose
    /// optional sections have not landed yet.
    ///
    /// Defaulted to a no-op (frozen-trait idiom) — standalone sinks that only
    /// track status keep compiling unchanged.
    async fn set_agent_outcome(
        &self,
        _task_id: &str,
        _outcome: lingxi_core::host::task_registry::AgentTerminalOutcome,
    ) {
    }

    /// Replace a local Agent task's typed transcript with a settled snapshot
    /// from its runner. Unlike incremental `Message` events, this final image
    /// reflects accepted hooks, tombstones, and resume history.
    async fn replace_agent_transcript(
        &self,
        _task_id: &str,
        _messages: Vec<lingxi_core::types::ConversationMessage>,
    ) {
    }

    /// Record a workflow's result/failure/usage payload before its terminal
    /// status is published. Registry-backed sinks override this; standalone
    /// handlers remain payload-agnostic.
    async fn set_workflow_outcome(
        &self,
        _task_id: &str,
        _outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
    ) {
    }

    /// Record a Fusion run's sanitized final text before its terminal status.
    /// Registry-backed sinks override this; standalone sinks stay no-ops.
    async fn set_fusion_outcome(&self, _task_id: &str, _run_id: String, _final_text: String) {}

    /// Record a Fusion run's failure reason before its terminal `Failed`
    /// status. Registry-backed sinks override this; standalone sinks stay
    /// no-ops. Additive — call sites that never fail a Fusion run need not
    /// change.
    async fn set_fusion_error(&self, _task_id: &str, _error: String) {}

    /// Record a Fusion run's egress profiles and usage summary. Registry-
    /// backed sinks override this; standalone sinks stay no-ops. Additive.
    async fn set_fusion_egress_and_usage(
        &self,
        _task_id: &str,
        _egress_profiles: Vec<String>,
        _usage: Option<lingxi_core::host::task_registry::AgentRunUsage>,
    ) {
    }

    /// Record a Fusion run's current progress-stage label (F005), e.g.
    /// "Running panels 2/3" — the SAME text `FusionStage::label()` produces
    /// for the Agent-tool progress path. Registry-backed sinks override this
    /// to write `LocalFusionTaskState.stage`; standalone sinks stay no-ops.
    /// Additive — call sites that never forward Fusion progress need not
    /// change.
    async fn set_fusion_stage(&self, _task_id: &str, _stage: String) {}

    /// Record the typed parent-session publication receipt for a Fusion run.
    /// The legacy `result_published` boolean is updated only for an actual
    /// `Published` receipt; `Queued`, `NotRequired`, and failures remain
    /// distinguishable to readiness callers.
    async fn set_fusion_publication(
        &self,
        _task_id: &str,
        _receipt: lingxi_core::host::FusionPublicationReceipt,
    ) {
    }

    /// Atomically publish a Fusion run's terminal payload together with its
    /// terminal task status. Registry-backed sinks override this to close the
    /// outcome/status race; the default preserves legacy standalone behavior.
    async fn finish_fusion_terminal(
        &self,
        task_id: &str,
        run_id: String,
        final_text: String,
        status: TaskStatus,
    ) {
        self.set_fusion_outcome(task_id, run_id, final_text).await;
        self.set_status(task_id, status).await;
    }

    /// Atomically publish a workflow's terminal payload plus terminal status.
    ///
    /// Registry-backed sinks override this so a workflow outcome cannot be
    /// observed in the "payload written, status still non-terminal" window.
    /// Defaulted to the historical two-step sequence for standalone sinks.
    async fn finish_workflow_terminal(
        &self,
        task_id: &str,
        outcome: lingxi_core::host::task_registry::WorkflowTerminalOutcome,
        status: TaskStatus,
    ) {
        self.set_workflow_outcome(task_id, outcome).await;
        self.set_status(task_id, status).await;
    }

    /// Queue a live stdout event from a `monitor_ws` task. Default no-op keeps
    /// standalone handlers and existing test sinks source-compatible.
    ///
    /// `housekeeping` marks a harness line ABOUT the monitor rather than script
    /// output (claude-code `GM`'s `isHousekeeping`).
    async fn notify_monitor_event(&self, _task_id: &str, _event: &str, _housekeeping: bool) {}

    /// Whether the owning registry has inserted the task row. Handler-backed
    /// tasks receive their generated id before [`crate::registry::TaskRegistry`]
    /// can publish the state, so a very short command must wait for this handoff
    /// or its first event/terminal update can race ahead and be lost. Standalone
    /// sinks default to ready.
    async fn is_registered(&self, _task_id: &str) -> bool {
        true
    }

    /// Report whether `task_id` has ALREADY terminated (a completed persistent
    /// park is still alive). Backs the `drain_pending_kills` guard: a
    /// worker that finished on its own must not be retroactively flipped to
    /// `Killed` by a raced pending-kill record (which would clobber the real
    /// terminal status). Default `false` (no lifecycle info ⇒ flip as before,
    /// preserving existing behavior); the registry-backed sink overrides it to
    /// consult the stored task status.
    async fn is_terminal(&self, _task_id: &str) -> bool {
        false
    }
}

/// No-op [`TaskStatusSink`] — the default when the handler is constructed
/// without an explicit sink. State transitions become observable only through
/// the spool file in this mode.
pub struct NoopStatusSink;

#[async_trait]
impl TaskStatusSink for NoopStatusSink {
    async fn set_status(&self, _task_id: &str, _status: TaskStatus) {}
}

/// Local-bash task handler.
///
/// Holds the constructor-injected execution dependencies that are *not*
/// available on [`TaskContext`] ([`ProcessRunner`] + [`Sandbox`]), the spool
/// manager, the terminal-status sink, and the shared worker-cancel map keyed by
/// `task_id` so [`Task::kill`] can cancel the in-flight worker future (which
/// drops the `run()` child and `SIGKILL`s the real OS process).
pub struct LocalBashHandler {
    /// Runs the sandboxed bash command.
    process: Arc<dyn ProcessRunner>,
    /// Mints the [`lingxi_core::host::SandboxedCommand`] the runner accepts (D2 / A1).
    sandbox: Arc<dyn Sandbox>,
    /// Owns the spool directory + path allocation for stdout/stderr.
    output_manager: Arc<TaskOutputManager>,
    /// Where terminal status / exit-code transitions are reported.
    status_sink: Arc<dyn TaskStatusSink>,
    /// `task_id` → live worker-cancel record. Populated for the duration of the
    /// spawn; the worker removes its own entry on exit, and [`Task::kill`]
    /// removes + cancels it if still present (cancelling the worker drops the
    /// in-flight `run()`, which `SIGKILL`s the real OS child via the runner's
    /// `kill_on_drop`).
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    /// Task ids queued for teardown by the synchronous [`TaskHandle::cleanup`]
    /// closure (which cannot await). The closure records the request here so a
    /// contended async mutex cannot silently drop cancellation.
    pending_kill: Arc<StdMutex<Vec<String>>>,
}

impl LocalBashHandler {
    /// Construct a handler with the injected execution dependencies.
    ///
    /// `fs` and `runtime` arrive per-call via [`TaskContext`] and are
    /// deliberately *not* injected here. `process` + `sandbox` are required
    /// because they are absent from [`TaskContext`] yet the handler cannot run
    /// a command without them — exactly as
    /// `HookExecutorImpl::with_process_runner` wires them.
    #[must_use]
    pub fn new(
        process: Arc<dyn ProcessRunner>,
        sandbox: Arc<dyn Sandbox>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            process,
            sandbox,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workers: Arc::new(Mutex::new(HashMap::new())),
            pending_kill: Arc::new(StdMutex::new(Vec::new())),
        }
    }

    /// Attach a [`TaskStatusSink`] so terminal transitions / exit codes are
    /// reported (e.g. an adapter over [`crate::registry::TaskRegistry`]).
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Share the same `workers` map with an external owner (e.g. the registry
    /// wiring) so a [`TaskHandle::cleanup`] closure and [`Task::kill`] observe
    /// the same worker-cancel records.
    #[must_use]
    pub fn workers_map(&self) -> Arc<Mutex<HashMap<String, WorkerCancel>>> {
        self.workers.clone()
    }

    /// Drain records queued by [`TaskHandle::cleanup`] and cancel each worker
    /// future for real. This is the async counterpart of the synchronous
    /// cleanup closure: the registry/cleanup-registry calls it on agent teardown
    /// so a child outliving its agent is terminated (claude-code
    /// `killTask`-on-cleanup parity — `killShellTasksForAgent`). Cancelling the
    /// worker drops the in-flight `run()`, which `SIGKILL`s the real OS child;
    /// status is flipped to `Killed` UNLESS the task already reached a terminal
    /// status (a raced pending-kill record must not clobber a real
    /// Completed/Failed with `Killed`).
    pub async fn drain_pending_kills(&self) {
        let pending = {
            let mut pending = self.pending_kill.lock().unwrap();
            std::mem::take(&mut *pending)
        };
        for task_id in pending {
            let Some(rec) = self.workers.lock().await.remove(&task_id) else {
                continue;
            };
            let _ = rec.runtime.cancel(&rec.handle).await;
            // Don't overwrite an already-reported terminal status: a worker that
            // finished on its own before this (possibly raced) record was
            // drained keeps its real terminal status rather than being flipped
            // to Killed.
            if !self.status_sink.is_terminal(&task_id).await {
                self.status_sink
                    .set_status(&task_id, TaskStatus::Killed)
                    .await;
            }
        }
    }

    /// Build the `bash -c "<command>"` [`ProcessCommand`], carrying the
    /// optional timeout (the runner enforces it and reports
    /// [`ProcessError::Timeout`] / [`ProcessOutput::timed_out`]).
    fn build_process_command(
        command: String,
        timeout: Option<std::time::Duration>,
    ) -> ProcessCommand {
        ProcessCommand {
            command: "bash".into(),
            args: vec!["-c".into(), command],
            cwd: None,
            env: HashMap::new(),
            timeout,
            stdin: None,
        }
    }

    /// Map a [`ProcessRunner::run`] result onto the terminal status to report,
    /// honoring `timed_out` (→ `Killed`) and the exit code (`0` → `Completed`,
    /// non-zero → `Failed`). Returns `(status, exit_code)`.
    fn classify(result: &Result<ProcessOutput, ProcessError>) -> (TaskStatus, Option<i32>) {
        match result {
            Ok(o) if o.timed_out => (TaskStatus::Killed, Some(o.exit_code)),
            Ok(o) if o.exit_code == 0 => (TaskStatus::Completed, Some(o.exit_code)),
            Ok(o) => (TaskStatus::Failed, Some(o.exit_code)),
            Err(ProcessError::Timeout) => (TaskStatus::Killed, None),
            Err(_) => (TaskStatus::Failed, None),
        }
    }
}

#[async_trait]
impl Task for LocalBashHandler {
    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::LocalBash
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        // 1. Only the LocalBash variant is accepted; reject the other six.
        let TaskSpawnInput::LocalBash {
            command, timeout, ..
        } = input
        else {
            return Err(TaskError::Internal(
                "local_bash handler received a non-LocalBash spawn input".into(),
            ));
        };

        // 2. Generate the task id and allocate its spool file.
        let task_id = crate::id::generate_task_id(TaskType::LocalBash);
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

        // 3. Build the bash command.
        let pcmd = Self::build_process_command(command, timeout);

        // 4. Mint the SandboxedCommand. Mirror the hooks Command arm: with no
        //    policy in scope, an audited bypass is the parity-honest construction
        //    (hooks/src/executor.rs:276).
        let sandboxed = self.sandbox.bypass_with_audit(pcmd, BYPASS_REASON);

        // 5. Drive the ONE child to completion inside a runtime-spawned worker
        //    (engine code must not call tokio::spawn directly — D17). The worker
        //    captures output via a single run(), appends it to the spool, and
        //    reports the terminal status / exit code. This is the only OS child
        //    for the task (no second `spawn_background` copy — TS's
        //    `shellCommand.background()` re-uses the already-running process, it
        //    does not re-execute the command).
        let process = self.process.clone();
        let status_sink = self.status_sink.clone();
        let workers = self.workers.clone();
        let output_manager = self.output_manager.clone();
        let worker_spool_path = spool_path.clone();
        let worker_task_id = task_id.clone();
        let worker = Box::pin(async move {
            status_sink
                .set_status(&worker_task_id, TaskStatus::Running)
                .await;

            let result = process.run(&sandboxed).await;

            // Append captured output to the spool (best effort — spool I/O
            // failure must not mask the command result). Routed through the
            // output manager's `append`, which enforces the per-file 5GB disk
            // cap (T17) and appends with O_NOFOLLOW (claude-code `diskOutput.ts`)
            // so a symlink planted at the spool path from inside the sandbox
            // cannot redirect the write (T18).
            if let Ok(out) = &result {
                if !out.stdout.is_empty() {
                    let _ = output_manager.append(&worker_spool_path, &out.stdout).await;
                }
                if !out.stderr.is_empty() {
                    let _ = output_manager.append(&worker_spool_path, &out.stderr).await;
                }
            }

            let (status, exit_code) = LocalBashHandler::classify(&result);
            if let Some(code) = exit_code {
                status_sink.set_exit_code(&worker_task_id, code).await;
            }
            status_sink.set_status(&worker_task_id, status).await;

            // The child has exited; drop the cancel record so a late kill is a
            // graceful no-op (claude-code `status !== 'running'` early return).
            workers.lock().await.remove(&worker_task_id);
        });

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
        // kill / drain can cancel the in-flight worker — i.e. terminate the one
        // `run()` child — without a fresh ctx.
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle: bg_handle,
                runtime: ctx.runtime.clone(),
            },
        );
        drop(workers);

        // 6. Build the cleanup seam (claude-code `registerCleanup` parity —
        //    `spawnShellTask` registers a cleanup that calls `killTask`). The
        //    closure is *synchronous* (the `TaskHandle::cleanup` type is `Fn()`),
        //    while `RuntimeSpawner::cancel` is async and engine code must not
        //    spawn its own runtime tasks (D17). So cleanup cannot itself await a
        //    cancel. Its honest job is to flag the worker for teardown by moving
        //    any live cancel record into the shared `pending_kill` queue, which
        //    the registry/cleanup-registry drains via the async
        //    `drain_pending_kills`. Authoritative termination also flows through
        //    `Task::kill`.
        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            cleanup_pending
                .lock()
                .unwrap()
                .push(cleanup_task_id.clone());
        });

        Ok(TaskHandle::new(task_id, Some(cleanup)))
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        // Recover the live worker-cancel record (if any) and cancel the
        // in-flight worker future — the analogue of TS `shellCommand.kill()`.
        // Cancelling drops the `run()` future, which drops the platform `Child`;
        // the foreground runner spawns with `kill_on_drop(true)`, so the drop
        // `SIGKILL`s the real OS process. An absent record ⇒ the child already
        // exited ⇒ graceful no-op (claude-code `status !== 'running'` early
        // return in `killTask`).
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
        false
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::state::TaskStatus;
    use lingxi_core::host::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
    use lingxi_core::host::sandbox::{SandboxCapability, SandboxedTag};
    use lingxi_core::host::{
        ProcessCommand, ProcessHandle, SandboxError, SandboxPolicy, SandboxedCommand,
    };
    use mobile_linux_api::SandboxBackend;
    use std::collections::HashMap as StdHashMap;
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use tempfile::tempdir;
    use test_harness::mocks::MockRuntimeSpawner;
    use tokio::sync::Mutex as TokioMutex;

    // ---- In-memory FileSystem (mirrors handle.rs InMemoryFs) ---------------

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

    // ---- Mock ProcessRunner (records the command it ran) -------------------
    //
    // `run()` completes immediately with a fixed `ProcessOutput`. `kill()` /
    // `spawn_background()` are trait stubs: the single-child design never calls
    // them (the OS child is owned by the worker future and killed via
    // `RuntimeSpawner::cancel`). The kill test uses `BlockingRunner` below.

    struct MockRunner {
        output: ProcessOutput,
        ran_command: StdMutex<Option<Vec<String>>>,
    }
    impl MockRunner {
        fn new(output: ProcessOutput) -> Arc<Self> {
            Arc::new(Self {
                output,
                ran_command: StdMutex::new(None),
            })
        }
        fn ran_args(&self) -> Option<Vec<String>> {
            self.ran_command.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl ProcessRunner for MockRunner {
        async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            let inner = cmd.inner();
            let mut recorded = vec![inner.command.clone()];
            recorded.extend(inner.args.clone());
            *self.ran_command.lock().unwrap() = Some(recorded);
            Ok(self.output.clone())
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    /// A long-running [`ProcessRunner`] whose `run()` future stands in for an
    /// in-flight `bash -c "<long>"` child: it parks forever until the future is
    /// dropped. A drop-guard records that the child future was actually
    /// aborted, so a kill test can assert real termination — not merely a
    /// status flip. This is the unit-test analogue of the platform runner's
    /// `kill_on_drop(true)`: dropping the `run()` future is what `SIGKILL`s the
    /// real OS process in production.
    struct BlockingRunner {
        /// Flipped to `true` inside the `run()` future's drop-guard when the
        /// future is cancelled (i.e. the in-flight child was aborted).
        aborted: Arc<std::sync::atomic::AtomicBool>,
        /// Released once `run()` has started and is parked, so the test can
        /// kill only after the child is genuinely in-flight.
        started: Arc<tokio::sync::Notify>,
    }
    impl BlockingRunner {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                aborted: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                started: Arc::new(tokio::sync::Notify::new()),
            })
        }
        fn was_aborted(&self) -> bool {
            self.aborted.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    #[async_trait]
    impl ProcessRunner for BlockingRunner {
        async fn run(&self, _cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            // Drop-guard: set `aborted` if this future is dropped (cancelled)
            // before it completes — proof the in-flight child was terminated.
            struct AbortGuard(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for AbortGuard {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _guard = AbortGuard(self.aborted.clone());
            self.started.notify_one();
            // Park forever; the only way out is cancellation (drop).
            std::future::pending::<()>().await;
            unreachable!("BlockingRunner::run only resolves via cancellation");
        }
        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }
        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    // ---- Stub Sandbox (mints via the documented external-impl seam) --------

    struct StubSandbox {
        last_reason: StdMutex<Option<String>>,
    }
    impl StubSandbox {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                last_reason: StdMutex::new(None),
            })
        }
        fn reason(&self) -> Option<String> {
            self.last_reason.lock().unwrap().clone()
        }
    }
    #[async_trait]
    impl Sandbox for StubSandbox {
        fn is_available(&self) -> bool {
            true
        }
        fn backend(&self) -> SandboxBackend {
            SandboxBackend::None
        }
        fn prepare(
            &self,
            cmd: ProcessCommand,
            _policy: &SandboxPolicy,
        ) -> Result<SandboxedCommand, SandboxError> {
            Ok(SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: "test".into(),
                },
            ))
        }
        fn bypass_with_audit(&self, cmd: ProcessCommand, reason: &str) -> SandboxedCommand {
            *self.last_reason.lock().unwrap() = Some(reason.to_string());
            SandboxedCommand::__new_sandboxed(
                cmd,
                SandboxedTag::BypassAuditedWithReason {
                    reason: reason.into(),
                },
            )
        }
        async fn probe_capability(&self) -> SandboxCapability {
            SandboxCapability {
                available: true,
                reason: None,
                features: lingxi_core::host::SandboxFeatures::default(),
            }
        }
    }

    // ---- Recording status sink ---------------------------------------------

    #[derive(Default)]
    struct RecordingSink {
        statuses: StdMutex<Vec<(String, TaskStatus)>>,
        exit_codes: StdMutex<Vec<(String, i32)>>,
    }
    #[async_trait]
    impl TaskStatusSink for RecordingSink {
        async fn set_status(&self, task_id: &str, status: TaskStatus) {
            self.statuses
                .lock()
                .unwrap()
                .push((task_id.to_string(), status));
        }
        async fn set_exit_code(&self, task_id: &str, exit_code: i32) {
            self.exit_codes
                .lock()
                .unwrap()
                .push((task_id.to_string(), exit_code));
        }
        async fn is_terminal(&self, task_id: &str) -> bool {
            self.statuses
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
        fn exit_codes(&self) -> Vec<(String, i32)> {
            self.exit_codes.lock().unwrap().clone()
        }
    }

    // ---- Helpers ------------------------------------------------------------

    fn output(stdout: &str, stderr: &str, exit_code: i32, timed_out: bool) -> ProcessOutput {
        ProcessOutput {
            stdout: stdout.into(),
            stderr: stderr.into(),
            exit_code,
            timed_out,
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

    /// Poll the sink until it has recorded a terminal status (or the budget is
    /// exhausted). The worker runs on the `MockRuntimeSpawner`'s tokio task, so a
    /// few yields let it finish deterministically without a fixed sleep.
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
    async fn spawn_returns_handle_and_runs_bash_dash_c() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("hello\n", "", 0, false));
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = LocalBashHandler::new(runner.clone(), sandbox.clone(), mgr)
            .with_status_sink(sink.clone());

        let handle = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "echo hello".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                make_ctx(fs),
            )
            .await
            .expect("spawn should succeed");

        assert!(handle.task_id.starts_with('b'), "LocalBash id prefix");
        assert!(handle.cleanup.is_some(), "cleanup seam is present");

        let status = await_terminal(&sink).await;
        assert_eq!(status, TaskStatus::Completed, "exit 0 ⇒ Completed");

        // Command ran as `bash -c "echo hello"`.
        let args = runner.ran_args().expect("run() must have been called");
        assert_eq!(args, vec!["bash", "-c", "echo hello"]);

        // Audited bypass reason matches the hooks-arm parity tag.
        assert_eq!(sandbox.reason().as_deref(), Some(BYPASS_REASON));

        // Exit code recorded.
        let codes = sink.exit_codes();
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].1, 0);
    }

    #[tokio::test]
    async fn spawn_spools_stdout_and_stderr() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("out-line\n", "err-line\n", 0, false));
        let sandbox = StubSandbox::new();
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(
            PathBuf::from(dir.path()),
            fs.clone(),
        ));
        let sink = Arc::new(RecordingSink::default());

        let handler =
            LocalBashHandler::new(runner, sandbox, mgr.clone()).with_status_sink(sink.clone());

        let handle = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "do stuff".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                make_ctx(fs),
            )
            .await
            .unwrap();

        await_terminal(&sink).await;

        // The handler allocates `{task_id}.output` under the manager's dir
        // (the tempdir). Read it straight back.
        let spool_path = dir.path().join(format!("{}.output", handle.task_id));
        let read = mgr
            .read(&spool_path, crate::output_manager::OutputOptions::default())
            .await
            .unwrap();
        assert!(read.content.contains("out-line"), "stdout spooled");
        assert!(read.content.contains("err-line"), "stderr spooled");
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_failed() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("", "boom\n", 1, false));
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = LocalBashHandler::new(runner, sandbox, mgr).with_status_sink(sink.clone());

        handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "false".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                make_ctx(fs),
            )
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Failed);
    }

    #[tokio::test]
    async fn timed_out_output_maps_to_killed() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("", "", -1, true));
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = LocalBashHandler::new(runner, sandbox, mgr).with_status_sink(sink.clone());

        handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "sleep 100".into(),
                    timeout: Some(std::time::Duration::from_millis(1)),
                    tool_use_id: None,
                },
                make_ctx(fs),
            )
            .await
            .unwrap();

        assert_eq!(await_terminal(&sink).await, TaskStatus::Killed);
    }

    #[tokio::test]
    async fn kill_terminates_the_in_flight_child() {
        // T2: kill() must REALLY terminate the in-flight `bash -c "<long>"`
        // child — not merely flip status to Killed. We model the long-running
        // child with a `BlockingRunner` whose `run()` future parks forever and
        // sets an `aborted` flag in its drop-guard when cancelled. The handler's
        // single child is owned by the worker future; `kill()` cancels that
        // future via `RuntimeSpawner::cancel`, which drops the parked `run()`
        // (in production: drops the platform `Child`, whose `kill_on_drop(true)`
        // sends SIGKILL). We assert the child future was actually aborted AND
        // the status flipped to Killed.
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = BlockingRunner::new();
        let started = runner.started.clone();
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler =
            LocalBashHandler::new(runner.clone(), sandbox, mgr).with_status_sink(sink.clone());

        // The worker must run on the SAME runtime the handler records for the
        // cancel seam, so `kill()`'s `RuntimeSpawner::cancel` aborts THIS task.
        let runtime = Arc::new(MockRuntimeSpawner::default());
        let ctx = TaskContext {
            fs: fs.clone(),
            runtime,
        };

        let handle = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "sleep 100000".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                ctx.clone(),
            )
            .await
            .unwrap();

        // Wait until the child is genuinely in-flight (run() parked).
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("run() should start (child in-flight)");
        assert!(!runner.was_aborted(), "child still running before kill");

        // Kill: cancels the worker future ⇒ drops the in-flight run() child.
        handler
            .kill(&handle.task_id, ctx)
            .await
            .expect("kill should succeed");

        assert_eq!(
            sink.last_status(),
            Some(TaskStatus::Killed),
            "status ⇒ Killed"
        );

        // The in-flight child future was actually aborted (its drop-guard ran),
        // proving a real termination rather than a bare status flip. Abort is
        // observed at the next scheduler tick.
        for _ in 0..200 {
            if runner.was_aborted() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            runner.was_aborted(),
            "kill aborted/reaped the in-flight child (drop-guard fired)"
        );

        // The cancel record is gone — a second kill is a graceful no-op.
        assert!(
            handler
                .workers_map()
                .lock()
                .await
                .get(&handle.task_id)
                .is_none(),
            "worker-cancel record removed on kill"
        );
    }

    #[tokio::test]
    async fn kill_absent_handle_is_graceful_noop() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("", "", 0, false));
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler = LocalBashHandler::new(runner, sandbox, mgr).with_status_sink(sink.clone());

        // No spawn ⇒ no worker-cancel record. kill must still succeed (nothing
        // to cancel) and flip to Killed (claude-code `status !== 'running'`).
        handler
            .kill("bdeadbeef", make_ctx(fs))
            .await
            .expect("kill of unknown task is a no-op success");
        assert!(
            handler.workers_map().lock().await.is_empty(),
            "no worker record ⇒ nothing to cancel"
        );
        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));
    }

    #[tokio::test]
    async fn non_localbash_input_is_rejected() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("", "", 0, false));
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());

        let handler = LocalBashHandler::new(runner, sandbox, mgr);

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
            Ok(_) => panic!("non-LocalBash input must be rejected"),
        }
    }

    #[tokio::test]
    async fn cleanup_then_drain_kills_in_flight_child() {
        // claude-code `spawnShellTask` registers a cleanup that calls `killTask`
        // (`killShellTasksForAgent` on agent teardown). The synchronous cleanup
        // closure cannot await, so it moves the live worker-cancel record into
        // `pending_kill`; the async `drain_pending_kills` then performs the real
        // `RuntimeSpawner::cancel`, terminating the in-flight child.
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = BlockingRunner::new();
        let started = runner.started.clone();
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler =
            LocalBashHandler::new(runner.clone(), sandbox, mgr).with_status_sink(sink.clone());

        let runtime = Arc::new(MockRuntimeSpawner::default());
        let ctx = TaskContext {
            fs: fs.clone(),
            runtime,
        };

        let handle = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "long".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                ctx,
            )
            .await
            .unwrap();

        // Wait until the child is genuinely in-flight before tearing it down.
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("run() should start (child in-flight)");

        // Fire the synchronous cleanup closure (moves the live record to
        // pending_kill).
        (handle.cleanup.as_ref().unwrap())();

        // Drain performs the real async cancel ⇒ drops the in-flight run() child.
        handler.drain_pending_kills().await;

        assert_eq!(sink.last_status(), Some(TaskStatus::Killed));

        for _ in 0..200 {
            if runner.was_aborted() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            runner.was_aborted(),
            "drain aborted/reaped the in-flight child (drop-guard fired)"
        );
    }

    /// `drain_pending_kills` MUST NOT flip an already-terminal task to `Killed`:
    /// if a still-live record is moved to `pending_kill` by cleanup AFTER the
    /// worker reported a terminal status, draining it keeps the real terminal
    /// status (the guard on `TaskStatusSink::is_terminal`).
    #[tokio::test]
    async fn drain_pending_kills_preserves_terminal_status() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = BlockingRunner::new();
        let started = runner.started.clone();
        let sandbox = StubSandbox::new();
        let (_dir, mgr) = make_output_manager(fs.clone());
        let sink = Arc::new(RecordingSink::default());

        let handler =
            LocalBashHandler::new(runner.clone(), sandbox, mgr).with_status_sink(sink.clone());

        let runtime = Arc::new(MockRuntimeSpawner::default());
        let ctx = TaskContext {
            fs: fs.clone(),
            runtime,
        };

        let handle = handler
            .spawn(
                TaskSpawnInput::LocalBash {
                    command: "long".into(),
                    timeout: None,
                    tool_use_id: None,
                },
                ctx,
            )
            .await
            .unwrap();

        // Wait until the child is genuinely in-flight (a live record exists).
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("run() should start (child in-flight)");

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

    #[test]
    fn name_and_type_are_local_bash() {
        let fs: Arc<dyn FileSystem> = Arc::new(InMemoryFs::new());
        let runner = MockRunner::new(output("", "", 0, false));
        let sandbox = StubSandbox::new();
        let dir = tempdir().unwrap();
        let mgr = Arc::new(TaskOutputManager::new(PathBuf::from(dir.path()), fs));
        let handler = LocalBashHandler::new(runner, sandbox, mgr);
        assert_eq!(handler.name(), "local_bash");
        assert_eq!(handler.task_type(), TaskType::LocalBash);
        assert!(!handler.supports_messages());
    }
}
