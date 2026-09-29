//! Fusion deliberation task handler.
//!
//! `/fusion` spawns a [`TaskType::LocalFusion`] row, runs
//! [`platform_api::FusionExecutor`] on the engine runtime (never `tokio::spawn`
//! — D17), spools a sanitized [`platform_api::FusionResult`], and publishes one
//! `user_meta` fusion-result envelope through [`platform_api::FusionCompletionSink`].
//! Sink failures must not rewrite the task's terminal status.

use crate::id::TaskType;
use crate::output_manager::TaskOutputManager;
use crate::state::TaskStatus;
use crate::task_trait::{Task, TaskContext, TaskError, TaskHandle, TaskSpawnInput};
use async_trait::async_trait;
use platform_api::{
    BackgroundTaskHandle, BudgetEnforcerHandle, FusionActivation, FusionCompletionSink,
    FusionError, FusionExecutor, FusionInheritance, FusionOrigin, FusionPreparedSummary,
    FusionResult, FusionRunFacts, FusionRunId, FusionRunIdentity, FusionRunOutcome,
    FusionRunRecorder, FusionRunRecorderFactory, FusionSlashPublicationTarget, FusionStatus,
    FusionSubmission, FusionTerminalCapability, PreparedFusionRun, RuntimeSpawner,
    SubagentInheritance, ToolInvoker,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::{oneshot, Mutex};
use tokio_util::sync::CancellationToken;

pub use crate::handlers::local_bash::{NoopStatusSink, TaskStatusSink};

const HANDLER_NAME: &str = "local_fusion";

/// Bounded wait `kill`/`drain_pending_kills` give the worker to unwind
/// through its own finalize path (`FusionOrchestrator::run`'s cooperative
/// `cancel.cancelled()` branch — CANCELLED telemetry, `Cancelled` progress,
/// `run_panels`' abort+join, explicit lease release) after firing `cancel`,
/// before falling back to `runtime.cancel` (a hard `JoinHandle::abort()` that
/// would skip all of that — F012). Mirrors `local_workflow`'s
/// `cancel_workflow_worker`.
const FUSION_KILL_COMPLETION_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long host shutdown waits for handler-owned Fusion workers that already
/// won natural finalization. Bounded because every desktop exit path awaits
/// it; an overrun is reported rather than parked on.
const FUSION_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

struct WorkerCancel {
    handle: BackgroundTaskHandle,
    runtime: Arc<dyn RuntimeSpawner>,
    control: platform_api::FusionRunControl,
    /// Worker completion signal, fired (via [`WorkerCompletionSignal`]'s
    /// `Drop`) once the spawned worker future is fully done — naturally, or
    /// forcibly on `runtime.cancel`'s own abort. `kill`/`drain_pending_kills`
    /// wait on this for up to [`FUSION_KILL_COMPLETION_GRACE`] before falling
    /// back to the hard abort, so a cooperative `cancel()` gets a real chance
    /// to run `FusionOrchestrator::run`'s own finalize path (F012).
    completion_rx: StdMutex<Option<oneshot::Receiver<()>>>,
}

/// Fires its held oneshot on drop, unconditionally — whichever path the
/// worker future exits through (normal completion, the early
/// `may_finalize == false` return, or a forced `abort()` dropping the task
/// mid-poll). Held as the worker future's own first local so its lifetime
/// exactly brackets "the worker is done, one way or another".
struct WorkerCompletionSignal(Option<oneshot::Sender<()>>);

impl WorkerCompletionSignal {
    fn new(tx: oneshot::Sender<()>) -> Self {
        Self(Some(tx))
    }
}

impl Drop for WorkerCompletionSignal {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}

/// `cancel()` the worker, then wait up to [`FUSION_KILL_COMPLETION_GRACE`] for
/// its own completion signal before falling back to `runtime.cancel` (a hard
/// abort that bypasses `FusionOrchestrator::run`'s cooperative cancel branch —
/// F012). Mirrors `local_workflow::cancel_workflow_worker`.
async fn cancel_fusion_worker(rec: WorkerCancel) -> Result<(), TaskError> {
    rec.control.cancel().cancel();
    let mut completion_rx = rec
        .completion_rx
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let completed = if let Some(completion_rx) = completion_rx.as_mut() {
        tokio::time::timeout(FUSION_KILL_COMPLETION_GRACE, completion_rx)
            .await
            .is_ok()
    } else {
        false
    };
    if !completed {
        rec.runtime
            .cancel(&rec.handle)
            .await
            .map_err(|error| TaskError::Io(error.to_string()))?;
        if let Some(completion_rx) = completion_rx {
            let _ = completion_rx.await;
        }
    }
    Ok(())
}

/// Terminal-outcome half of `spawn`'s worker future: write the spool body /
/// error, push the egress+usage summary, flip the task to its terminal
/// status, and (on success) publish the sanitized result envelope. Split out
/// of `spawn` purely to keep that function under the line-count lint — same
/// ordering, same branches, same side effects.
async fn finalize_fusion_outcome(
    outcome: &FusionRunOutcome,
    output_manager: &TaskOutputManager,
    worker_spool_path: &std::path::Path,
    worker_task_id: &str,
    status_sink: &Arc<dyn TaskStatusSink>,
    sink: &Arc<dyn FusionCompletionSink>,
    conversation_id: &str,
    // [Finding 14] The last `realized_output_tokens` seen on the progress
    // channel before the run ended in `Err` — `None` when nothing egressed
    // yet (a preflight refusal) or the orchestrator never reported it for
    // this failure shape. Used to give the Err arms a best-effort `<usage>`
    // disclosure instead of the structural silence a bare `None` produces
    // downstream.
    last_realized_output_tokens: Option<u64>,
    // [Round-3 review B2] Same latching as `last_realized_output_tokens`,
    // for the resolved egress profile list — see
    // `platform_api::FusionProgress::egress_profiles`.
    last_egress_profiles: Option<Vec<String>>,
) {
    let accounting_failed =
        if let Some(platform_api::FusionAttemptSettlementStatus::Failed { reason }) =
            &outcome.facts.attempt_settlement
        {
            status_sink
                .set_fusion_error(
                    worker_task_id,
                    format!("Fusion accounting failed: {reason}"),
                )
                .await;
            true
        } else {
            false
        };
    match &outcome.result {
        Ok(result) => {
            // The parent model synthesizes the final answer from this
            // material; it reaches the parent through the task
            // notification's `<result>`, and the spool keeps the full JSON.
            let material = platform_api::render_fusion_material(result);
            let body = serde_json::to_string_pretty(result).unwrap_or_else(|_| material.clone());
            let _ = output_manager.append(worker_spool_path, &body).await;
            // Write the egress/usage summary BEFORE the terminal status
            // transition (same ordering rule as `set_agent_outcome`): the
            // registry's notification drain is terminal-status-gated, so the
            // reverse order could let a drain observe a completed task whose
            // usage summary hasn't landed yet.
            let usage_summary = platform_api::task_registry::AgentRunUsage {
                // [Round-7 items 5+6] `AgentRunUsage.subagent_tokens` is
                // main-owned and documented as claude-code's `totalTokens`
                // (`platform_api::task_registry::AgentRunUsage`); main's own
                // producer sums the four BILLABLE buckets —
                // `local_agent.rs`'s `bt.input + bt.cache_write +
                // bt.cache_read + bt.output`, mirrored in
                // `agent::handle::subagent_usage_from_llm_usage`. Summing
                // only `input + output` here (as this did) dropped the two
                // cache buckets `FusionUsage` already carries, so a cached
                // multi-panel run reported a small fraction of the tokens an
                // equivalent background-agent run reports under the very same
                // `<subagent_tokens>` tag and the model could not compare the
                // two. `reasoning_tokens` stays OUT on purpose: `local_agent`
                // excludes `bt.reasoning_output` too, and the contract here is
                // to match main's definition of the tag, not to invent a third
                // one.
                subagent_tokens: result
                    .usage
                    .input_tokens
                    .saturating_add(result.usage.cache_write_tokens)
                    .saturating_add(result.usage.cache_read_tokens)
                    .saturating_add(result.usage.output_tokens),
                tool_uses: u64::from(result.usage.provider_requests),
                duration_ms: result.timing.total_ms,
            };
            status_sink
                .set_fusion_egress_and_usage(
                    worker_task_id,
                    result.egress_profiles.clone(),
                    Some(usage_summary),
                )
                .await;
            // Production prepared runs have already recorded the sealed
            // terminal outcome before `activate()` returned. Only legacy
            // callers without a capability use the old completion sink. Keep
            // that adapter's historical ordering (task outcome first, then
            // sink publication) so old hosts can observe a completed answer
            // even when their best-effort UI sink fails. Typed production
            // receipts are projected before the terminal row becomes visible;
            // they never call the legacy sink or get published twice.
            let legacy_sink =
                outcome.publication.status == platform_api::FusionPublicationStatus::NotRequired;
            let mut receipt = outcome.publication.clone();
            if !legacy_sink {
                status_sink
                    .set_fusion_publication(worker_task_id, receipt.clone())
                    .await;
            }
            status_sink
                .finish_fusion_terminal(
                    worker_task_id,
                    result.run_id.clone(),
                    material,
                    if accounting_failed {
                        TaskStatus::Failed
                    } else {
                        TaskStatus::Completed
                    },
                )
                .await;
            if legacy_sink {
                receipt = sink.publish(conversation_id, result).await;
                status_sink
                    .set_fusion_publication(worker_task_id, receipt.clone())
                    .await;
            }
            // Publication is independent from computation: the terminal
            // answer is retained even when the append fails, and the typed
            // receipt written above is the only thing that decides readiness.
        }
        Err(FusionError::Cancelled) => {
            // [Finding 14] Same ordering rule as the `Ok` arm above: write
            // whatever usage disclosure we have BEFORE the terminal status
            // transition, so the notification drain (terminal-status-gated)
            // never observes a `Killed` task whose partial usage hasn't
            // landed yet.
            disclose_facts_or_partial_usage(
                status_sink,
                worker_task_id,
                &outcome.facts,
                last_realized_output_tokens,
                last_egress_profiles,
            )
            .await;
            status_sink
                .set_fusion_publication(worker_task_id, outcome.publication.clone())
                .await;
            status_sink
                .set_status(worker_task_id, TaskStatus::Killed)
                .await;
        }
        Err(err) => {
            let _ = output_manager
                .append(worker_spool_path, &err.to_string())
                .await;
            disclose_facts_or_partial_usage(
                status_sink,
                worker_task_id,
                &outcome.facts,
                last_realized_output_tokens,
                last_egress_profiles,
            )
            .await;
            status_sink
                .set_fusion_publication(worker_task_id, outcome.publication.clone())
                .await;
            status_sink
                .set_fusion_error(worker_task_id, err.to_string())
                .await;
            status_sink
                .set_status(worker_task_id, TaskStatus::Failed)
                .await;
        }
    }
}

/// [Finding 14; round-3 review B2 follow-up] Best-effort `<usage>` AND
/// `<egress-profiles>` disclosure for a fusion run that ends in `Err` after
/// real panel spend: `FusionError` carries neither payload, so the progress
/// channel's last-seen `realized_output_tokens` / `egress_profiles`
/// (latched by `run_fusion_worker`'s forwarder — see
/// `platform_api::FusionProgress::egress_profiles`'s doc for where the
/// orchestrator populates it) are the only signals available at this seam.
/// `tool_uses`/`duration_ms` are still unknown too and stay at `0` rather
/// than fabricated. A no-op when nothing ever egressed (e.g. a preflight
/// refusal — both `last_*` arguments are `None`), matching the render arm's
/// existing `usage: None` omission.
///
/// [Round-7 items 5+6] **The `subagent_tokens` written here is deliberately
/// NOT the same quantity the `Ok` arm writes.** The `Ok` arm has the whole
/// `FusionUsage` and sums main's four billable buckets (`input +
/// cache_write + cache_read + output`, matching `local_agent.rs`); this arm
/// has only `FusionProgress::realized_output_tokens` — OUTPUT tokens alone,
/// because that is the single figure the progress channel carries (it exists
/// for the workflow budget bridge, which charges from it). So the value
/// below is a true LOWER BOUND on the run's total, not the total, and is
/// reported that way rather than reporting nothing at all. Widening it would
/// mean widening `fusion::progress::emit_with_realized_tokens` to carry a
/// full realized total alongside the output-only figure — a producer-side
/// change, not one this seam can make.
async fn disclose_partial_usage(
    status_sink: &Arc<dyn TaskStatusSink>,
    worker_task_id: &str,
    last_realized_output_tokens: Option<u64>,
    last_egress_profiles: Option<Vec<String>>,
) {
    if let Some(tokens) = last_realized_output_tokens {
        status_sink
            .set_fusion_egress_and_usage(
                worker_task_id,
                last_egress_profiles.unwrap_or_default(),
                Some(platform_api::task_registry::AgentRunUsage {
                    subagent_tokens: tokens,
                    tool_uses: 0,
                    duration_ms: 0,
                }),
            )
            .await;
    }
}

/// Prefer the prepared run's reliable receipt on error. Progress remains only
/// a compatibility fallback for a legacy executor whose adapter could not
/// observe exact usage/counts.
async fn disclose_facts_or_partial_usage(
    status_sink: &Arc<dyn TaskStatusSink>,
    worker_task_id: &str,
    facts: &FusionRunFacts,
    last_realized_output_tokens: Option<u64>,
    last_egress_profiles: Option<Vec<String>>,
) {
    let mut egress = facts.confirmed_egress.clone();
    egress.extend(facts.possible_egress.iter().cloned());
    egress.sort();
    egress.dedup();
    let usage = facts
        .usage
        .as_ref()
        .map(|usage| platform_api::task_registry::AgentRunUsage {
            subagent_tokens: usage
                .input_tokens
                .saturating_add(usage.cache_write_tokens)
                .saturating_add(usage.cache_read_tokens)
                .saturating_add(usage.output_tokens),
            tool_uses: u64::from(usage.provider_requests),
            duration_ms: facts.timing.total_ms,
        });
    if usage.is_some() || !egress.is_empty() {
        status_sink
            .set_fusion_egress_and_usage(worker_task_id, egress, usage)
            .await;
        return;
    }
    disclose_partial_usage(
        status_sink,
        worker_task_id,
        last_realized_output_tokens,
        last_egress_profiles,
    )
    .await;
}

/// Owned inputs the fusion background worker future needs, collected into a
/// struct so `spawn` can build it in one call instead of moving a dozen
/// separate captures into the closure — the struct itself is what keeps
/// `spawn` under the argument-count and line-count lints.
struct FusionWorkerArgs {
    prepared: Option<PreparedFusionRun>,
    conversation_id: String,
    sink: Arc<dyn FusionCompletionSink>,
    status_sink: Arc<dyn TaskStatusSink>,
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    output_manager: Arc<TaskOutputManager>,
    worker_spool_path: std::path::PathBuf,
    worker_task_id: String,
    activation_rx: Option<oneshot::Receiver<FusionActivation>>,
    /// Held for its `Drop` side effect only — fires the completion signal
    /// unconditionally once this future is done, whichever path it exits
    /// through. Never read.
    #[allow(dead_code)]
    completion_signal: WorkerCompletionSignal,
}

/// `spawn`'s background worker body: run the deliberation, forward its
/// progress into the task DTO's `stage`, and finalize the terminal outcome.
/// Split out of `spawn` (as a plain fn taking owned args, not an inline
/// `async move` closure) purely to keep that function under the line-count
/// lint — same ordering, same finalize race handling, same side effects.
async fn run_fusion_worker(args: FusionWorkerArgs) {
    let FusionWorkerArgs {
        prepared,
        conversation_id,
        sink,
        status_sink,
        workers,
        output_manager,
        worker_spool_path,
        worker_task_id,
        activation_rx,
        completion_signal: _completion_signal,
    } = args;
    let activation = if let Some(activation_rx) = activation_rx {
        match activation_rx.await {
            Ok(activation) => activation,
            Err(_) => {
                workers.lock().await.remove(&worker_task_id);
                return;
            }
        }
    } else {
        FusionActivation::now()
    };
    let Some(prepared) = prepared else {
        workers.lock().await.remove(&worker_task_id);
        return;
    };
    status_sink
        .set_status(&worker_task_id, TaskStatus::Running)
        .await;

    // F005: forward progress into `LocalFusionTaskState.stage` so a client
    // polling the task DTO sees the same `FusionStage::label()` text the
    // Agent-tool path forwards as `subagent_activity` — before this the
    // task carried NO progress at all between `Running` and its terminal
    // status.
    let (prog_tx, mut prog_rx) = tokio::sync::mpsc::channel::<platform_api::FusionProgress>(32);
    let forward_status_sink = status_sink.clone();
    let forward_task_id = worker_task_id.clone();
    // [Finding 14; round-3 review B2 follow-up] Track the LAST
    // `realized_output_tokens` AND `egress_profiles` the orchestrator emits
    // as a compatibility fallback for legacy executors. Reliable prepared
    // facts are preferred at terminalization.
    let forwarder = tokio::spawn(async move {
        let mut last_realized_output_tokens: Option<u64> = None;
        let mut last_egress_profiles: Option<Vec<String>> = None;
        while let Some(event) = prog_rx.recv().await {
            if let Some(tokens) = event.realized_output_tokens {
                last_realized_output_tokens = Some(tokens);
            }
            if let Some(profiles) = event.egress_profiles {
                last_egress_profiles = Some(profiles);
            }
            forward_status_sink
                .set_fusion_stage(&forward_task_id, event.stage.label())
                .await;
        }
        (last_realized_output_tokens, last_egress_profiles)
    });

    // `PreparedFusionRun::activate` owns the common panic boundary and always
    // publishes the terminal envelope. Do not wrap it here: an outer panic
    // conversion could fabricate an outcome without publishing the control's
    // terminal receipt, leaving quota/task waiters blocked forever.
    let outcome = prepared.activate(activation, Some(prog_tx)).await;
    // Natural completion and TaskStop race on this same worker-map lock.
    // The prepared control is the lifecycle authority. Once it reaches
    // Finalizing or Terminal, kill cannot remove the record from underneath
    // the commit→completion-sink window.
    let may_finalize = {
        let mut workers = workers.lock().await;
        workers.get_mut(&worker_task_id).is_some()
    };
    let (last_realized_output_tokens, last_egress_profiles) =
        forwarder.await.unwrap_or((None, None));
    if !may_finalize {
        // [Round-4 review finding 10] `kill`/`drain_pending_kills` both
        // remove this worker's record from `workers` BEFORE firing
        // `cancel` (see their own comments), so `finalize_fusion_outcome`'s
        // `Err(FusionError::Cancelled)` arm — and the `disclose_partial_usage`
        // it calls — can never run for a user-stopped `/fusion`: by the
        // time `executor.run` returns `Err(Cancelled)` here, the record is
        // already gone and `may_finalize` is `false`. `kill`/
        // `drain_pending_kills` still write the terminal `Killed` status
        // themselves; this call recovers only the disclosure that arm
        // would otherwise have written — the egress profiles / realized
        // token usage the forwarder above already latched from the
        // progress channel before the record was removed. A no-op when
        // nothing ever egressed (both `last_*` are `None`), matching
        // `disclose_partial_usage`'s own guard.
        disclose_facts_or_partial_usage(
            &status_sink,
            &worker_task_id,
            &outcome.facts,
            last_realized_output_tokens,
            last_egress_profiles,
        )
        .await;
        return;
    }
    finalize_fusion_outcome(
        &outcome,
        &output_manager,
        &worker_spool_path,
        &worker_task_id,
        &status_sink,
        &sink,
        &conversation_id,
        last_realized_output_tokens,
        last_egress_profiles,
    )
    .await;
    workers.lock().await.remove(&worker_task_id);
}

/// Handler for [`TaskType::LocalFusion`].
pub struct LocalFusionHandler {
    executor: Arc<dyn FusionExecutor>,
    sink: Arc<dyn FusionCompletionSink>,
    tool_invoker: Arc<dyn ToolInvoker>,
    budget: Arc<dyn BudgetEnforcerHandle>,
    output_manager: Arc<TaskOutputManager>,
    status_sink: Arc<dyn TaskStatusSink>,
    workers: Arc<Mutex<HashMap<String, WorkerCancel>>>,
    pending_kill: Arc<StdMutex<Vec<String>>>,
    terminal_recorder: Option<Arc<dyn FusionRunRecorder>>,
    terminal_recorder_factory: Option<Arc<dyn FusionRunRecorderFactory>>,
}

impl LocalFusionHandler {
    /// Construct with composition-root handles.
    #[must_use]
    pub fn new(
        executor: Arc<dyn FusionExecutor>,
        sink: Arc<dyn FusionCompletionSink>,
        tool_invoker: Arc<dyn ToolInvoker>,
        budget: Arc<dyn BudgetEnforcerHandle>,
        output_manager: Arc<TaskOutputManager>,
    ) -> Self {
        Self {
            executor,
            sink,
            tool_invoker,
            budget,
            output_manager,
            status_sink: Arc::new(NoopStatusSink),
            workers: Arc::new(Mutex::new(HashMap::new())),
            pending_kill: Arc::new(StdMutex::new(Vec::new())),
            terminal_recorder: None,
            terminal_recorder_factory: None,
        }
    }

    /// Attach a registry-backed status sink.
    #[must_use]
    pub fn with_status_sink(mut self, sink: Arc<dyn TaskStatusSink>) -> Self {
        self.status_sink = sink;
        self
    }

    /// Attach the host-owned common terminal recorder. Preparation remains
    /// pure; the capability is attached only after trusted identity creation.
    #[must_use]
    pub fn with_terminal_recorder(mut self, recorder: Arc<dyn FusionRunRecorder>) -> Self {
        self.terminal_recorder = Some(recorder);
        self
    }

    /// Attach a pure per-session recorder factory for hot session switches.
    #[must_use]
    pub fn with_terminal_recorder_factory(
        mut self,
        factory: Arc<dyn FusionRunRecorderFactory>,
    ) -> Self {
        self.terminal_recorder_factory = Some(factory);
        self
    }

    /// Drain kill requests queued by the synchronous cleanup closure.
    pub async fn drain_pending_kills(&self) {
        let pending = {
            let mut pending = self
                .pending_kill
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *pending)
        };
        for task_id in pending {
            if self.status_sink.is_terminal(&task_id).await {
                continue;
            }
            let rec = {
                let mut workers = self.workers.lock().await;
                let cancel_won = workers
                    .get(&task_id)
                    .is_some_and(|rec| rec.control.request_cancel());
                if cancel_won {
                    workers.remove(&task_id)
                } else {
                    None
                }
            };
            let Some(rec) = rec else { continue };
            let _ = cancel_fusion_worker(rec).await;
            if !self.status_sink.is_terminal(&task_id).await {
                self.status_sink
                    .set_status(&task_id, TaskStatus::Killed)
                    .await;
            }
        }
    }
}

#[async_trait]
impl Task for LocalFusionHandler {
    fn name(&self) -> &str {
        HANDLER_NAME
    }

    fn task_type(&self) -> TaskType {
        TaskType::LocalFusion
    }

    async fn spawn(
        &self,
        input: TaskSpawnInput,
        ctx: TaskContext,
    ) -> Result<TaskHandle, TaskError> {
        let TaskSpawnInput::LocalFusion {
            request,
            conversation_id,
        } = input
        else {
            return Err(TaskError::Internal(
                "fusion handler received a non-LocalFusion spawn input".into(),
            ));
        };

        // The task input is the host-trusted session identity. Validate it
        // before allocating a spool or publishing any task artifacts so a
        // malformed slash request cannot become an unscoped run or leave an
        // orphaned output file behind. The task input is the only trusted
        // source for the originating session; request text is compatibility
        // data and never supplies identity.
        let trusted_session = protocol::SessionId::parse_prefixed(&conversation_id);
        if trusted_session.is_none() {
            return Err(TaskError::Internal(
                "fusion task conversation_id must be a valid session id".into(),
            ));
        }

        let task_id = crate::id::generate_task_id(TaskType::LocalFusion);
        let spool_path = self
            .output_manager
            .allocate(&task_id)
            .await
            .map_err(|e| TaskError::Io(e.to_string()))?;

        let cancel = CancellationToken::new();
        let executor = self.executor.clone();
        let inherit = FusionInheritance::new(
            SubagentInheritance {
                tool_invoker: self.tool_invoker.clone(),
                budget: self.budget.clone(),
            },
            cancel.clone(),
        );
        // The task input is the host-trusted session identity, and it reaches
        // preparation only through the identity below. Request text has no say
        // in which ledger this run bills.
        let identity = FusionRunIdentity::new(
            FusionRunId::generated(),
            trusted_session,
            FusionOrigin::Slash,
            Some(task_id.clone()),
        );
        // Preparation failures are represented by an inert worker. Do not
        // reload mutable settings merely to populate that fallback row.
        let fallback_duration_ms = 0;
        let fallback_summary = FusionPreparedSummary {
            identity: identity.clone(),
            duration_ms: fallback_duration_ms,
            planned_panels: None,
        };
        let prepared = match executor.clone().prepare(FusionSubmission {
            request,
            inherit,
            identity: identity.clone(),
        }) {
            Ok(prepared) => prepared,
            Err(error) => {
                let control = platform_api::FusionRunControl::new(
                    identity,
                    fallback_duration_ms,
                    cancel.clone(),
                    platform_api::FusionRunFactsRecorder::default(),
                );
                PreparedFusionRun::failed(fallback_summary.clone(), control, error)
            }
        };
        let recorder = if let Some(factory) = self.terminal_recorder_factory.as_ref() {
            factory.recorder_for(trusted_session.expect("validated session identity"))
        } else {
            self.terminal_recorder.clone()
        };
        let prepared = if let Some(recorder) = recorder.as_ref() {
            let capability = FusionTerminalCapability::new(recorder.clone());
            let capability = trusted_session.map_or(capability.clone(), |session_id| {
                capability.with_slash_target(FusionSlashPublicationTarget { session_id })
            });
            prepared.with_terminal_capability(capability)
        } else {
            prepared
        };
        let prepared_summary = prepared.summary().clone();
        let prepared_control = prepared.control();
        let sink = self.sink.clone();
        let status_sink = self.status_sink.clone();
        let workers = self.workers.clone();
        let output_manager = self.output_manager.clone();
        let worker_spool_path = spool_path;
        let worker_task_id = task_id.clone();
        let (activation_tx, activation_rx) = if status_sink.requires_explicit_activation() {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let (completion_tx, completion_rx) = oneshot::channel();

        let worker = Box::pin(run_fusion_worker(FusionWorkerArgs {
            prepared: Some(prepared),
            conversation_id,
            sink,
            status_sink,
            workers,
            output_manager,
            worker_spool_path,
            worker_task_id,
            activation_rx,
            // Fires unconditionally when this future is done — naturally, or
            // forced by `runtime.cancel`'s hard abort — so `kill`'s bounded
            // wait can tell the two apart (F012).
            completion_signal: WorkerCompletionSignal::new(completion_tx),
        }));

        let mut workers = self.workers.lock().await;
        let bg_handle = ctx
            .runtime
            .spawn(&format!("{HANDLER_NAME}:{task_id}"), worker)
            .await
            .map_err(|e| TaskError::Internal(e.to_string()))?;
        workers.insert(
            task_id.clone(),
            WorkerCancel {
                handle: bg_handle,
                runtime: ctx.runtime.clone(),
                control: prepared_control,
                completion_rx: StdMutex::new(Some(completion_rx)),
            },
        );
        drop(workers);

        let cleanup_pending = self.pending_kill.clone();
        let cleanup_task_id = task_id.clone();
        let cleanup: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            cleanup_pending
                .lock()
                .unwrap()
                .push(cleanup_task_id.clone());
        });

        let handle =
            TaskHandle::new(task_id, Some(cleanup)).with_fusion_prepared_summary(prepared_summary);
        Ok(match activation_tx {
            Some(activation_tx) => handle.with_fusion_activation(move |activation| {
                let _ = activation_tx.send(activation);
            }),
            None => handle,
        })
    }

    async fn kill(&self, task_id: &str, _ctx: TaskContext) -> Result<(), TaskError> {
        if self.status_sink.is_terminal(task_id).await {
            return Ok(());
        }
        let rec = {
            let mut workers = self.workers.lock().await;
            let Some(rec) = workers.get(task_id) else {
                return Ok(());
            };
            // Claim cancellation atomically with the lifecycle phase check.
            // A separate is_finalizing()/is_terminal() read lets natural
            // finalization win immediately afterward while kill still removes
            // the worker record. Publication is suppressed by the recorder,
            // which builds an outbox only for a successful result -- this path
            // skips the legacy sink but does not by itself stop a publication.
            if !rec.control.request_cancel() {
                return Ok(());
            }
            workers
                .remove(task_id)
                .expect("worker record was present for the cancellation claim")
        };
        cancel_fusion_worker(rec).await?;
        if !self.status_sink.is_terminal(task_id).await {
            self.status_sink
                .set_status(task_id, TaskStatus::Killed)
                .await;
        }
        Ok(())
    }

    async fn drain_shutdown(&self) -> Result<(), TaskError> {
        let deadline = tokio::time::Instant::now() + FUSION_DRAIN_BUDGET;
        let completions = {
            let workers = self.workers.lock().await;
            workers
                .values()
                .filter_map(|worker| {
                    worker
                        .completion_rx
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take()
                })
                .collect::<Vec<_>>()
        };
        let mut timed_out = 0_usize;
        for completion in completions {
            if tokio::time::timeout_at(deadline, completion).await.is_err() {
                timed_out += 1;
            }
        }
        if timed_out > 0 {
            return Err(TaskError::Internal(format!(
                "{timed_out} Fusion worker(s) did not finish within the shutdown budget"
            )));
        }
        Ok(())
    }
}

/// XML-escape `&`, `<`, `>`, `"`, `'`.
#[must_use]
pub fn escape_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// Compact, user-facing fusion-result envelope. It is appended to the
/// session excluded from model context: the parent model receives the full
/// material through the task notification and writes the answer itself, so
/// this only tells the user what ran. No panel text, no analyst payload, no
/// model names (profile ids only, via `<egress-profiles>`).
#[must_use]
pub fn fusion_result_xml(result: &FusionResult) -> String {
    let status = match result.status {
        FusionStatus::Analyzed => "analyzed",
        FusionStatus::Unanalyzed => "unanalyzed",
    };
    let analysis_failure_section = result
        .analysis_failure
        .as_deref()
        .map(|reason| {
            format!(
                "\n  <analysis-failure>{}</analysis-failure>",
                escape_xml(reason)
            )
        })
        .unwrap_or_default();
    let panels = result
        .panels
        .iter()
        .map(|panel| {
            let status = match panel.status {
                platform_api::PanelRunStatus::Completed => "completed",
                platform_api::PanelRunStatus::Failed => "failed",
                platform_api::PanelRunStatus::TimedOut => "timed_out",
                platform_api::PanelRunStatus::Cancelled => "cancelled",
            };
            format!("{}:{status}", panel.panel_id)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let egress_section = if result.egress_profiles.is_empty() {
        String::new()
    } else {
        format!(
            "\n  <egress-profiles>{}</egress-profiles>",
            escape_xml(&result.egress_profiles.join(", "))
        )
    };
    let usage_section = format!(
        "\n  <usage><input-tokens>{}</input-tokens><output-tokens>{}</output-tokens><provider-requests>{}</provider-requests><estimated>{}</estimated></usage>",
        result.usage.input_tokens,
        result.usage.output_tokens,
        result.usage.provider_requests,
        result.usage.estimated,
    );
    format!(
        "<fusion-result>\n  <run-id>{}</run-id>\n  <status>{status}</status>{analysis_failure_section}\n  <panels>{}</panels>{egress_section}{usage_section}\n</fusion-result>",
        escape_xml(&result.run_id),
        escape_xml(&panels),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::{FusionAnalysis, FusionTiming, FusionUsage, PanelMaterial, PanelReport};

    fn result(analysis: Option<FusionAnalysis>) -> FusionResult {
        let report = PanelReport {
            schema_version: 2,
            summary: "PANEL_SENTINEL_summary".into(),
            candidate_answer: "PANEL_SENTINEL_answer".into(),
            claims: vec![],
            evidence: vec![],
            assumptions: vec![],
            risks: vec![],
            unresolved_questions: vec![],
        };
        FusionResult {
            schema_version: 2,
            run_id: "fu_<x>".into(),
            status: if analysis.is_some() {
                FusionStatus::Analyzed
            } else {
                FusionStatus::Unanalyzed
            },
            analysis_failure: analysis.is_none().then(|| "timeout".to_string()),
            analysis,
            responses: vec![PanelMaterial::from_report("P1", &report, &[])],
            panels: vec![platform_api::PanelOutcome {
                panel_id: "P1".into(),
                status: platform_api::PanelRunStatus::Completed,
                duration_ms: 1,
                error_category: None,
                error_detail: None,
                usage: None,
            }],
            usage: FusionUsage {
                input_tokens: 111,
                output_tokens: 222,
                provider_requests: 3,
                estimated: true,
                ..FusionUsage::default()
            },
            timing: FusionTiming::default(),
            egress_profiles: vec!["openai".into()],
            mode: Default::default(),
        }
    }

    fn sentinel_analysis() -> FusionAnalysis {
        FusionAnalysis {
            schema_version: 2,
            consensus: vec![platform_api::SupportedPoint {
                point: "ANALYSIS_SENTINEL_consensus".into(),
                panel_ids: vec!["P1".into()],
            }],
            contradictions: vec![],
            partial_coverage: vec![],
            unique_insights: vec![],
            blind_spots: vec!["ANALYSIS_SENTINEL_gap".into()],
            scores: std::collections::BTreeMap::new(),
        }
    }

    /// The user-facing envelope only says what ran: the material itself goes
    /// to the parent model through the task notification, so neither panel
    /// text nor the analyst payload may appear here.
    #[test]
    fn xml_is_a_compact_summary_without_panel_or_analyst_text() {
        let xml = fusion_result_xml(&result(Some(sentinel_analysis())));
        assert!(xml.contains("<run-id>fu_&lt;x&gt;</run-id>"));
        assert!(xml.contains("<status>analyzed</status>"));
        assert!(xml.contains("<panels>P1:completed</panels>"));
        assert!(xml.contains("<egress-profiles>openai</egress-profiles>"));
        assert!(!xml.contains("SENTINEL"), "material leaked: {xml}");
        assert!(!xml.contains("<analysis"), "got: {xml}");
        assert!(!xml.contains("<analysis-failure>"), "got: {xml}");
    }

    #[test]
    fn xml_names_the_analysis_failure_when_the_analyst_did_not_run() {
        let xml = fusion_result_xml(&result(None));
        assert!(xml.contains("<status>unanalyzed</status>"));
        assert!(xml.contains("<analysis-failure>timeout</analysis-failure>"));
    }

    #[test]
    fn xml_usage_section_carries_tokens_and_estimated_flag_but_no_model_names() {
        let xml = fusion_result_xml(&result(None));
        assert!(
            xml.contains(
                "<usage><input-tokens>111</input-tokens><output-tokens>222</output-tokens><provider-requests>3</provider-requests><estimated>true</estimated></usage>"
            ),
            "got: {xml}"
        );
    }
}

#[cfg(test)]
#[path = "local_fusion_test.rs"]
mod local_fusion_test;
