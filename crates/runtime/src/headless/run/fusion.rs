use crate::headless::exit_codes;
use crate::headless::output::OutputSink;
use lingxi_core::host::FusionPublicationStatus;

/// Extract a `local_fusion` task id from `/fusion`'s `Handled` display text
/// (`"{task_id}  {preset}  {scope}"`, `harness-runtime/src/desktop/
/// fusion_command.rs`), if the display looks like one. `local_fusion` ids
/// are `'f'` + 8 lowercase-base36 chars (`tasks::id::TaskType::id_prefix`) —
/// unique among every other task-id prefix in the workspace.
///
/// Shape-only: does NOT know which command produced `display`. Review
/// finding #26 — some other `Handled` command's display can collide with
/// this shape (e.g. a `/btw`/`/recap` answer beginning with an abbreviated
/// git SHA, or a bare word like "following"). Callers must gate on the
/// dispatched command actually being `/fusion` first —
/// [`local_fusion_task_id_to_await`] is that combined, safe-to-call form;
/// this function stays a private shape-matcher only.
pub(super) fn pending_local_fusion_task_id(display: &str) -> Option<&str> {
    let first = display.split_whitespace().next()?;
    let suffix = first.strip_prefix('f')?;
    (suffix.len() == 8
        && suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()))
    .then_some(first)
}

/// Whether `input` dispatches to the `/fusion` command by name, per the same
/// name extraction [`SlashCommandDispatcher::dispatch`] itself uses
/// (`command_api::parser::parse_slash_command`).
pub(super) fn dispatches_to_fusion_command(input: &str) -> bool {
    command_api::parser::parse_slash_command(input).is_some_and(|parsed| parsed.name == "fusion")
}

/// The `local_fusion` task id (if any) `run_slash_command_with_budget`
/// should await for a `Handled` result — [Finding 26]: `None` for every
/// command except `/fusion` itself, regardless of whether its `display`
/// happens to have the `f`+8 shape [`pending_local_fusion_task_id`] looks
/// for. Without this gate, ANY `Handled` command whose display collided
/// with that shape triggered a registry lookup for a task that never
/// existed, silently exiting print mode non-zero for a command that
/// actually succeeded.
pub(super) fn local_fusion_task_id_to_await<'a>(input: &str, display: &'a str) -> Option<&'a str> {
    dispatches_to_fusion_command(input)
        .then(|| pending_local_fusion_task_id(display))
        .flatten()
}

/// Exit code for a dispatched `/fusion` that produced a `Handled` display
/// with no task id to await ([`local_fusion_task_id_to_await`] returned
/// `None`) — i.e. `/fusion` never actually started a `local_fusion` task.
///
/// Review finding #11: `TaskRegistry::spawn` failing (`fusion_command.rs`'s
/// `Err(err) => Done { display: Some(format!("fusion failed to start:
/// {err}")) }`) used to fall through to `exit_codes::SUCCESS` like every
/// other `Handled` display, silently contradicting the very contract
/// [`fusion_result_exit_code`] exists to provide: a script gating on `$?`
/// could not tell a run that truly never started from one that produced an
/// answer. A flag/usage rejection (`FUSION_SLASH_USAGE`, `unknown flag
/// ...`) is left at `SUCCESS` deliberately — that is the pre-existing,
/// CLI-wide convention for every `Handled` command's argument errors
/// (`CommandResult::Done` carries no error channel), and special-casing
/// `/fusion` alone there would be a new inconsistency, not a fix.
pub(super) fn fusion_spawn_failure_exit_code(input: &str, display: &str) -> i32 {
    if dispatches_to_fusion_command(input)
        && (display.starts_with("fusion failed to start: ")
            || display.starts_with("fusion publication retry failed: "))
    {
        exit_codes::RUNTIME_ERROR
    } else {
        exit_codes::SUCCESS
    }
}

/// The minimal `local_fusion`-state lookup [`await_local_fusion_result_with_budget`]
/// needs — implemented by the real registry, and by a scripted double in
/// tests, so the polling/formatting logic doesn't require standing up a full
/// composition root to exercise.
#[async_trait::async_trait]
pub(super) trait FusionTaskLookup: Send + Sync {
    async fn get(&self, task_id: &str) -> Option<tasks::state::TaskState>;
}

#[async_trait::async_trait]
impl FusionTaskLookup for tasks::registry::TaskRegistry {
    async fn get(&self, task_id: &str) -> Option<tasks::state::TaskState> {
        tasks::registry::TaskRegistry::get(self, task_id).await
    }
}

/// What to tell the user once an awaited `local_fusion` task reaches a
/// terminal status.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FusionPrintOutcome {
    /// `Completed` — the run's sanitized final text.
    FinalText(String),
    /// The answer is durable in an outbox and can be returned successfully,
    /// with a warning that transcript delivery remains pending.
    Queued(String),
    /// The computation produced an answer, but publication failed. Keep the
    /// answer visible while returning a non-zero process result.
    PublicationFailed { answer: String, reason: String },
    /// Computation succeeded but reliable accounting failed. Publication may
    /// still have succeeded; keep both the answer and non-zero exit status.
    FailedWithAnswer { answer: String, reason: String },
    /// `Failed` — the recorded failure reason (falls back to a generic
    /// message when the reason was never recorded).
    Failed(String),
    /// Any other terminal status (`Killed`, or anything future statuses add)
    /// — debug-formatted, since this path has no dedicated copy for it.
    Other(String),
}

/// Map a terminal (or absent — evicted, or the print-mode wait timed out)
/// `local_fusion` outcome to print mode's process exit code. A directly
/// `Published` answer and a durably `Queued` answer succeed. A publication
/// failure may still carry the computed answer, but must be non-zero alongside
/// compute failures, kills, eviction, and timeout so scripts can distinguish a
/// durable result from an answer that exists only in this process.
pub(super) fn fusion_result_exit_code(outcome: Option<&FusionPrintOutcome>) -> i32 {
    match outcome {
        Some(FusionPrintOutcome::FinalText(_) | FusionPrintOutcome::Queued(_)) => {
            exit_codes::SUCCESS
        }
        Some(
            FusionPrintOutcome::PublicationFailed { .. }
            | FusionPrintOutcome::FailedWithAnswer { .. }
            | FusionPrintOutcome::Failed(_)
            | FusionPrintOutcome::Other(_),
        )
        | None => exit_codes::RUNTIME_ERROR,
    }
}

/// Whether `state` is DONE for print mode's purposes — terminal, AND (for a
/// `Completed` run specifically) its publication attempt has a terminal typed
/// receipt. `finish_fusion_terminal` flips the task to a terminal state before the
/// handler awaits `FusionCompletionSink::publish`, so a waiter must remain on
/// `Pending`; `Published`, durable `Queued`, and explicit publication failures
/// are all ready to report with their distinct exit semantics. A failed run
/// with a retained answer also waits; computation failures and kills do not.
pub(super) fn fusion_result_ready(state: &tasks::state::TaskState) -> bool {
    let tasks::state::TaskState::LocalFusion(fusion) = state else {
        return state.base().status.is_terminal();
    };
    if !fusion.base.status.is_terminal() {
        return false;
    }
    if fusion.base.status != tasks::state::TaskStatus::Completed
        && !(fusion.base.status == tasks::state::TaskStatus::Failed && fusion.final_text.is_some())
    {
        // Computation failures/kills have no answer to publish. Accounting
        // failures do retain an answer and must await its publication tail.
        return true;
    }
    // Every publication state other than Pending is terminal for a waiter:
    // the append either landed, is durably queued, or definitively failed.
    fusion.publication_status.is_terminal()
}

pub(super) fn fusion_print_outcome(state: &tasks::state::TaskState) -> Option<FusionPrintOutcome> {
    let tasks::state::TaskState::LocalFusion(fusion) = state else {
        return None;
    };
    Some(match fusion.base.status {
        tasks::state::TaskStatus::Completed => {
            let answer = fusion.final_text.clone().unwrap_or_default();
            match fusion.publication_status {
                FusionPublicationStatus::Published => FusionPrintOutcome::FinalText(answer),
                FusionPublicationStatus::Queued => FusionPrintOutcome::Queued(answer),
                FusionPublicationStatus::NotRequired => FusionPrintOutcome::PublicationFailed {
                    answer,
                    reason: fusion
                        .publication_error
                        .clone()
                        .unwrap_or_else(|| {
                            "fusion result publication was not required; no durable transcript was recorded"
                                .to_string()
                        }),
                },
                FusionPublicationStatus::OutboxFailed
                | FusionPublicationStatus::StorageFailure => {
                    FusionPrintOutcome::PublicationFailed {
                        answer,
                        reason: fusion
                            .publication_error
                            .clone()
                            .unwrap_or_else(|| "fusion result could not be published".to_string()),
                    }
                }
                FusionPublicationStatus::Pending => {
                    FusionPrintOutcome::Other("publication pending".to_string())
                }
            }
        }
        tasks::state::TaskStatus::Failed => {
            let reason = fusion
                .error
                .clone()
                .unwrap_or_else(|| "fusion run failed".to_string());
            match &fusion.final_text {
                Some(answer) => FusionPrintOutcome::FailedWithAnswer {
                    answer: answer.clone(),
                    reason,
                },
                None => FusionPrintOutcome::Failed(reason),
            }
        }
        other => FusionPrintOutcome::Other(format!("{other:?}")),
    })
}

/// Poll interval while print mode waits for a background `/fusion` run.
pub(super) const FUSION_PRINT_POLL_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(200);

/// Headroom past a run's captured `total_timeout_ms` for the finalize tail
/// print mode must also wait through: result assembly, `lease.commit`,
/// telemetry, task-spool/registry writes, and the durable
/// `<fusion-result>` session append. This is deliberately only a tail margin;
/// the end-to-end timeout itself comes from the per-run snapshot stored on the
/// LocalFusion task, not a duplicated Fusion default.
pub(super) const FUSION_PRINT_FINALIZE_MARGIN_MS: u64 = 120_000;

pub(super) fn checked_fusion_print_deadline(
    monotonic_now: tokio::time::Instant,
    wait_budget: std::time::Duration,
) -> Option<tokio::time::Instant> {
    monotonic_now.checked_add(wait_budget)
}

/// Translate the effective timeout captured when a task was activated into a
/// monotonic print-mode deadline. The registry publishes `start_time` at the
/// same activation boundary as the prepared Fusion control, so TaskCreated
/// hook time is excluded while queue/scheduling time after activation counts.
/// A missing snapshot is intentionally unbounded here: production Fusion
/// handlers always capture one, while legacy/external task producers should be
/// allowed to finish rather than be cut off by a second stale default.
pub(super) fn fusion_print_deadline(
    state: &tasks::state::TaskState,
    monotonic_now: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    let tasks::state::TaskState::LocalFusion(fusion) = state else {
        return None;
    };
    if let Some(activation_deadline) = fusion.fusion_activation_deadline {
        return activation_deadline.checked_add(std::time::Duration::from_millis(
            FUSION_PRINT_FINALIZE_MARGIN_MS,
        ));
    }
    let elapsed = fusion.base.start_time.elapsed().unwrap_or_default();
    fusion_print_deadline_with_elapsed(state, monotonic_now, elapsed)
}

/// Deterministic core of [`fusion_print_deadline`]. Keeping elapsed time as an
/// explicit input lets tests model a post-activation scheduling delay without
/// sleeping or restarting the run's deadline at the first poll.
pub(super) fn fusion_print_deadline_with_elapsed(
    state: &tasks::state::TaskState,
    monotonic_now: tokio::time::Instant,
    elapsed: std::time::Duration,
) -> Option<tokio::time::Instant> {
    let tasks::state::TaskState::LocalFusion(fusion) = state else {
        return None;
    };
    let timeout_ms = fusion.effective_timeout_ms?;
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let wait_budget = timeout
        .saturating_add(std::time::Duration::from_millis(
            FUSION_PRINT_FINALIZE_MARGIN_MS,
        ))
        .saturating_sub(elapsed);
    checked_fusion_print_deadline(monotonic_now, wait_budget)
}

/// G002: await one `local_fusion` task to a terminal status and print its
/// result, instead of leaving print mode's only trace of the run as a bare
/// task id for work that dies with the process.
pub(super) async fn await_local_fusion_result(
    task_id: &str,
    task_registry: &tasks::registry::TaskRegistry,
    sink: &dyn OutputSink,
) -> Option<FusionPrintOutcome> {
    await_local_fusion_result_with_budget(
        task_id,
        task_registry,
        sink,
        FUSION_PRINT_POLL_INTERVAL,
        None,
    )
    .await
}

/// Returns the terminal outcome so the caller can set the process exit code
/// (§13: a `Failed`/`Other`/evicted/timed-out run must not exit 0) — `None`
/// covers every path that has no `FusionPrintOutcome` to report: the task
/// evicted or never created, and the print-mode wait timing out.
#[cfg(test)]
pub(super) async fn await_local_fusion_result_bounded<L>(
    task_id: &str,
    lookup: &L,
    sink: &dyn OutputSink,
    poll_interval: std::time::Duration,
    max_wait: std::time::Duration,
) -> Option<FusionPrintOutcome>
where
    L: FusionTaskLookup + ?Sized,
{
    await_local_fusion_result_with_budget(task_id, lookup, sink, poll_interval, Some(max_wait))
        .await
}

/// Shared print waiter. `Some(wait_budget)` is the deterministic test seam;
/// `None` derives the deadline once from the task's captured effective
/// timeout. The latter never consults mutable settings after task creation.
pub(super) async fn await_local_fusion_result_with_budget<L>(
    task_id: &str,
    lookup: &L,
    sink: &dyn OutputSink,
    poll_interval: std::time::Duration,
    wait_budget: Option<std::time::Duration>,
) -> Option<FusionPrintOutcome>
where
    L: FusionTaskLookup + ?Sized,
{
    let mut deadline = wait_budget.map(|budget| tokio::time::Instant::now() + budget);
    loop {
        let Some(state) = lookup.get(task_id).await else {
            // Evicted or never created — nothing left to report. [Finding
            // 26] Say so before returning `None`: this branch used to exit
            // print mode non-zero with nothing on stderr, indistinguishable
            // from every other silent failure.
            sink.error(
                "fusion",
                &format!("fusion task {task_id} was not found (evicted, or never created)"),
            )
            .await;
            return None;
        };
        if fusion_result_ready(&state) {
            let outcome = fusion_print_outcome(&state);
            match &outcome {
                Some(FusionPrintOutcome::FinalText(text)) => {
                    sink.command_output("fusion", text).await;
                }
                Some(FusionPrintOutcome::Queued(text)) => {
                    sink.command_output("fusion", text).await;
                    sink.error(
                        "fusion",
                        "fusion answer was durably queued; transcript delivery is still pending",
                    )
                    .await;
                }
                Some(
                    FusionPrintOutcome::PublicationFailed { answer, reason }
                    | FusionPrintOutcome::FailedWithAnswer { answer, reason },
                ) => {
                    // Preserve the computational answer for the caller even
                    // though publication or accounting failed independently.
                    sink.command_output("fusion", answer).await;
                    sink.error("fusion", reason).await;
                }
                Some(FusionPrintOutcome::Failed(reason)) => {
                    sink.error("fusion", reason).await;
                }
                Some(FusionPrintOutcome::Other(status)) => {
                    sink.error(
                        "fusion",
                        &format!("fusion run ended as {status} with no result"),
                    )
                    .await;
                }
                None => {}
            }
            return outcome;
        }
        if deadline.is_none() {
            deadline = fusion_print_deadline(&state, tokio::time::Instant::now());
        }
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            // Review finding #6: this process is print mode's ONLY caller of
            // this function (confirmed: no other in-repo caller of the
            // public `run_slash_command` wrapper), and it exits with this
            // function's return value moments after this fires — dropping
            // the in-process `local_fusion` worker future along with it.
            // "it may still be running" was therefore never true on this
            // path; say what actually happens instead.
            sink.error(
                "fusion",
                &format!(
                    "fusion run {task_id} did not finish before print mode's wait timed \
                     out; this process is exiting, which aborts the run's in-process \
                     worker along with it — it will not keep running in the background"
                ),
            )
            .await;
            return None;
        }
        tokio::time::sleep(poll_interval).await;
    }
}
