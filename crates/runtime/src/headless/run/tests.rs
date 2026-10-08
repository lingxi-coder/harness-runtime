use std::path::PathBuf;
use super::*;
use super::{fusion::*, lifecycle::*};
use std::time::{Duration, SystemTime};
#[test]
fn stream_compact_recognition_preserves_focus_without_matching_other_commands() {
    assert_eq!(compact_command_instructions("/compact"), Some(""));
    assert_eq!(
        compact_command_instructions(" /compact  retain API\nchanges "),
        Some("retain API\nchanges")
    );
    assert_eq!(compact_command_instructions("/compactor"), None);
    assert_eq!(compact_command_instructions("explain /compact"), None);
}

#[test]
fn stream_json_terminal_limits_keep_their_specific_error_subtypes() {
    assert_eq!(
        stream_json_error_subtype(&orchestrator::OrchestratorError::MaxTurnsReached {
            max_turns: 3,
        }),
        "error_max_turns"
    );
    assert_eq!(
        stream_json_error_subtype(&orchestrator::OrchestratorError::MaxBudgetReached {
            budget_nano_usd: 1_500_000_000,
        }),
        "error_max_budget_usd"
    );
    assert_eq!(
        stream_json_error_subtype(&orchestrator::OrchestratorError::Internal("boom".into())),
        "error_during_execution"
    );
}

#[test]
fn budget_halt_notice_matches_claude_bytes() {
    assert_eq!(
        budget_halt_notice(1.75, 5.0),
        "Budget limit reached ($1.75 of $5); stopping background agents."
    );
    assert_eq!(
        budget_halt_notice(1.505, 1.5),
        "Budget limit reached ($1.50 of $1.5); stopping background agents."
    );
    assert_eq!(
        budget_halt_notice(1.125, 2.0),
        "Budget limit reached ($1.13 of $2); stopping background agents."
    );
    assert_eq!(
        budget_halt_notice(2.675, 3.0),
        "Budget limit reached ($2.67 of $3); stopping background agents."
    );
}

#[test]
fn budget_reached_matches_claude_print_loop_boundary() {
    assert!(!budget_reached(1.5, 1_499_999_999));
    assert!(budget_reached(1.5, 1_500_000_000));
    assert!(budget_reached(1.5, 1_500_000_001));
}

fn outbound_line(msg: crate::headless::stream_json::OutboundMsg) -> String {
    match msg {
        crate::headless::stream_json::OutboundMsg::Line(line) => line,
        crate::headless::stream_json::OutboundMsg::StreamEvent(line) => line,
        crate::headless::stream_json::OutboundMsg::Heartbeats(_) => {
            panic!("unexpected heartbeat message")
        }
        _ => panic!("unexpected output fence"),
    }
}

#[test]
fn turn_terminal_lifecycle_matches_njo_call_site() {
    use crate::headless::queued_commands::QueueLifecycle;
    use orchestrator::{OrchestratorError, TurnOutcome};

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let lifecycle = QueueLifecycle::new(std::sync::Arc::new(tx), "sess-term".to_string());
    let next = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<
        crate::headless::stream_json::OutboundMsg,
    >| {
        serde_json::from_str::<serde_json::Value>(&outbound_line(
            rx.try_recv().expect("lifecycle frame"),
        ))
        .expect("valid command_lifecycle json")
    };

    // Clean turn → `completed` (reason "completed", not aborted).
    emit_turn_terminal_lifecycle(&lifecycle, Some("u-ok"), &Ok(TurnOutcome::EndTurn), false);
    let f = next(&mut rx);
    assert_eq!(f["type"], "command_lifecycle");
    assert_eq!(f["command_uuid"], "u-ok");
    assert_eq!(f["state"], "completed");

    // `max_turns` is one of `Bxs`'s `return!1` arms — still `completed`.
    emit_turn_terminal_lifecycle(&lifecycle, Some("u-max"), &Ok(TurnOutcome::MaxTurns), false);
    assert_eq!(next(&mut rx)["state"], "completed");

    // Interrupted turn: `aborted_streaming` (Wpt) AND the abort flag.
    emit_turn_terminal_lifecycle(&lifecycle, Some("u-int"), &Ok(TurnOutcome::Cancelled), true);
    assert_eq!(next(&mut rx)["state"], "cancelled");

    // Abort flag alone (`Njo`'s `t||…`) forces `cancelled` even on a turn
    // that otherwise ended naturally.
    emit_turn_terminal_lifecycle(&lifecycle, Some("u-ab"), &Ok(TurnOutcome::EndTurn), true);
    assert_eq!(next(&mut rx)["state"], "cancelled");

    // Hard failure — the `rn!==null?"cancelled"` arm. STREAM-1: this
    // reported `completed` while the same run's result frame said
    // `is_error:true`.
    for err in [
        OrchestratorError::MaxBudgetReached {
            budget_nano_usd: 100,
        },
        OrchestratorError::Internal("boom".to_string()),
    ] {
        emit_turn_terminal_lifecycle(&lifecycle, Some("u-err"), &Err(err), false);
        let f = next(&mut rx);
        assert_eq!(f["command_uuid"], "u-err");
        assert_eq!(f["state"], "cancelled");
    }

    // An unstamped frame is not lifecycle-tracked — no frame at all.
    emit_turn_terminal_lifecycle(&lifecycle, None, &Ok(TurnOutcome::EndTurn), false);
    assert!(rx.try_recv().is_err(), "no uuid ⇒ no lifecycle frame");
}

/// Every lifecycle-tracked uuid reaches EXACTLY ONE terminal. The resume
/// dedup path retires the uuid from the shadow registry (`on_dequeued`)
/// before skipping the turn, so teardown's `discarded` sweep can no longer
/// cover it — the skip must emit its own terminal (binary @246492139).
#[test]
fn resume_dedup_skip_emits_its_own_terminal() {
    use crate::headless::queued_commands::QueueLifecycle;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let lifecycle = QueueLifecycle::new(std::sync::Arc::new(tx), "sess-dedup".to_string());

    // Router: uuid enters the queue.
    lifecycle.command_queued("u-dup");
    let queued: serde_json::Value =
        serde_json::from_str(&outbound_line(rx.try_recv().expect("queued"))).unwrap();
    assert_eq!(queued["state"], "queued");

    // Turn loop: dequeue (not cancel-pending), then the dedup skip.
    assert!(lifecycle.queued.on_dequeued("u-dup"));
    emit_dedup_skip_terminal(&lifecycle, "u-dup");
    let terminal: serde_json::Value =
        serde_json::from_str(&outbound_line(rx.try_recv().expect("terminal"))).unwrap();
    assert_eq!(terminal["command_uuid"], "u-dup");
    assert_eq!(terminal["state"], "completed");

    // Teardown cannot make up for a missing terminal: the uuid is gone.
    // A uuid that never reached the turn loop IS still reachable, and gets
    // `discarded` (binary `Hkm`) — the contrast that makes the skip's own
    // terminal load-bearing.
    lifecycle.command_queued("u-resident");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&outbound_line(rx.try_recv().expect("queued")))
            .unwrap()["state"],
        "queued"
    );
    let survivors = lifecycle.queued.drain_for_discard();
    assert_eq!(
        survivors,
        vec!["u-resident"],
        "a dequeued uuid is unreachable from the teardown discard sweep"
    );
    for uuid in &survivors {
        lifecycle.emit(uuid, crate::headless::queued_commands::LIFECYCLE_DISCARDED);
    }
    let discarded: serde_json::Value =
        serde_json::from_str(&outbound_line(rx.try_recv().expect("discarded"))).unwrap();
    assert_eq!(discarded["command_uuid"], "u-resident");
    assert_eq!(discarded["state"], "discarded");
    assert!(rx.try_recv().is_err(), "exactly one terminal per command");
}

/// `cK(mt,ce.fastMode)` (@227895153): the state rides the SAME inputs as
/// `JW()`, so the `-p` surface never emits `off` with no reason.

#[derive(Default)]
struct RecordingFusionSink {
    outputs: tokio::sync::Mutex<Vec<(String, String)>>,
    errors: tokio::sync::Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl OutputSink for RecordingFusionSink {
    async fn text(&self, _s: &str) {}
    async fn turn_start(&self) {}
    async fn turn_end(&self, _r: &str, _u: f64, _i: u64, _o: u64) {}
    async fn tool_call(&self, _tool: &str, _input: &serde_json::Value) {}
    async fn tool_result(&self, _tool: &str, _result: &serde_json::Value) {}
    async fn tool_heartbeat(&self, _id: &str, _tool: &str, _elapsed_ms: u64) {}
    async fn command_output(&self, name: &str, display: &str) {
        self.outputs
            .lock()
            .await
            .push((name.to_string(), display.to_string()));
    }
    async fn mod_log(&self, _plugin: &str, _text: &str) {}
    async fn error(&self, code: &str, message: &str) {
        self.errors
            .lock()
            .await
            .push((code.to_string(), message.to_string()));
    }
}

/// Canned `local_fusion` states returned in order (repeating the last
/// one once exhausted), so a test can script "pending N times, then
/// terminal" without a live [`tasks::registry::TaskRegistry`].
struct ScriptedLookup {
    states: Vec<Option<tasks::state::TaskState>>,
    calls: std::sync::atomic::AtomicUsize,
}

impl ScriptedLookup {
    fn new(states: Vec<Option<tasks::state::TaskState>>) -> Self {
        Self {
            states,
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl FusionTaskLookup for ScriptedLookup {
    async fn get(&self, _task_id: &str) -> Option<tasks::state::TaskState> {
        let i = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let idx = i.min(self.states.len().saturating_sub(1));
        self.states.get(idx).cloned().flatten()
    }
}

fn fusion_state(
    status: tasks::state::TaskStatus,
    final_text: Option<&str>,
    error: Option<&str>,
) -> tasks::state::TaskState {
    tasks::state::TaskState::LocalFusion(tasks::state::LocalFusionTaskState {
        base: tasks::state::TaskStateBase {
            id: "ftest0001".to_string(),
            task_type: tasks::id::TaskType::LocalFusion,
            status,
            description: "Fusion quality same: review".to_string(),
            tool_use_id: None,
            start_time: std::time::SystemTime::now(),
            end_time: None,
            total_paused_ms: 0,
            output_file: PathBuf::from("/tmp/ftest0001"),
            evict_after: None,
            output_offset: 0,
            notified: false,
            creator_teammate_name: None,
            creator_team_name: None,
            creator_agent_id: None,
        },
        conversation_id: "conv".to_string(),
        prompt: "review this".to_string(),
        run_id: Some("fu_test".to_string()),
        preset: "quality".to_string(),
        cross_provider: false,
        final_text: final_text.map(str::to_string),
        error: error.map(str::to_string),
        egress_profiles: Vec::new(),
        usage: None,
        stage: None,
        effective_timeout_ms: None,
        planned_panels: None,
        fusion_activation_deadline: None,
        publication_status: lingxi_core::host::FusionPublicationStatus::Published,
        publication_error: None,
    })
}

/// Like [`fusion_state`] with `status: Completed`, but returns the inner
/// [`tasks::state::LocalFusionTaskState`] (not the wrapping enum) so a
/// caller can override `publication_status` with struct-update syntax.
fn completed_fusion_state(final_text: &str) -> tasks::state::LocalFusionTaskState {
    let tasks::state::TaskState::LocalFusion(fusion) =
        fusion_state(tasks::state::TaskStatus::Completed, Some(final_text), None)
    else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    fusion
}

#[test]
fn pending_local_fusion_task_id_recognizes_the_fusion_done_display() {
    assert_eq!(
        pending_local_fusion_task_id("f1a2b3c4d  quality  same-provider"),
        Some("f1a2b3c4d")
    );
    // Wrong length / non-fusion prefix / plain error text: no match.
    assert_eq!(pending_local_fusion_task_id("f1a2b3c  quality  x"), None);
    assert_eq!(
        pending_local_fusion_task_id("b1a2b3c4d  running"),
        None,
        "'b' is the local_bash prefix, not local_fusion's 'f'"
    );
    assert_eq!(
        pending_local_fusion_task_id("fusion failed to start: too few models"),
        None,
        "the Err(_) Done display must not be mistaken for a task id"
    );
    assert_eq!(pending_local_fusion_task_id(""), None);
}

/// [Finding 26]: `pending_local_fusion_task_id`'s shape-only detector
/// must never be applied to a command other than `/fusion`, even when
/// that command's `Handled` display happens to collide with the shape
/// (`f` + 8 lowercase-alnum chars) — e.g. a `/btw`/`/recap` answer
/// beginning with an abbreviated git SHA, or a bare word like
/// "following"/"formatted".
#[test]
fn local_fusion_task_id_to_await_requires_the_dispatched_command_to_be_fusion() {
    // The real /fusion shape: recovered.
    assert_eq!(
        local_fusion_task_id_to_await("/fusion review this", "f1a2b3c4d  quality  same-provider"),
        Some("f1a2b3c4d")
    );
    // A DIFFERENT command's Handled display that collides with the
    // f+8 shape must NOT be mistaken for a fusion task id.
    assert_eq!(
        local_fusion_task_id_to_await(
            "/btw which commit fixed the panel cap?",
            "f3ac93bac fixed it in the panel bar"
        ),
        None,
        "a non-fusion command's display must never be parsed as a \
         fusion task id, even when it happens to have the f+8 shape"
    );
    assert_eq!(
        local_fusion_task_id_to_await("/recap", "following up on yesterday's investigation"),
        None
    );
    // A command whose NAME merely starts with "fusion" must not match
    // either — only the exact command name counts.
    assert_eq!(
        local_fusion_task_id_to_await("/fusionx review this", "f1a2b3c4d  quality  same-provider"),
        None
    );
}

/// Review finding #11: `/fusion` whose `TaskRegistry::spawn` failed
/// (`fusion_command.rs`'s `"fusion failed to start: {err}"` display) must
/// exit non-zero in print mode — it never started a `local_fusion` task,
/// so `local_fusion_task_id_to_await` correctly returns `None`, but the
/// `None` arm used to hardcode `exit_codes::SUCCESS` for every such
/// display, indistinguishable from a `/fusion` that actually ran and
/// answered. A plain flag/usage rejection is deliberately left at
/// `SUCCESS`: that is the pre-existing, CLI-wide convention for every
/// `Handled` command's argument errors, not something this fix touches.
#[test]
fn fusion_spawn_failure_exits_non_zero_but_usage_rejection_does_not() {
    assert_eq!(
        fusion_spawn_failure_exit_code(
            "/fusion review this",
            "fusion failed to start: too few models"
        ),
        exit_codes::RUNTIME_ERROR,
        "a /fusion that never started a task must not exit 0"
    );
    assert_eq!(
        fusion_spawn_failure_exit_code("/fusion", "Usage: /fusion [--quality|--fast] ... PROMPT"),
        exit_codes::SUCCESS,
        "a usage/flag rejection is the pre-existing CLI-wide convention, not this fix's concern"
    );
    assert_eq!(
        fusion_spawn_failure_exit_code(
            "/fusion --retry-publication fu_0123456789abcdef0123456789abcdef",
            "fusion publication retry failed: durable outbox write failed"
        ),
        exit_codes::RUNTIME_ERROR,
        "a failed explicit durable retry must not exit 0"
    );
    // Non-fusion commands never take the RUNTIME_ERROR arm, even if
    // their display happens to start with the same text.
    assert_eq!(
        fusion_spawn_failure_exit_code(
            "/btw did fusion fail to start?",
            "fusion failed to start: unrelated collision"
        ),
        exit_codes::SUCCESS
    );
}

#[test]
fn fusion_print_outcome_maps_each_terminal_status() {
    assert_eq!(
        fusion_print_outcome(&fusion_state(
            tasks::state::TaskStatus::Completed,
            Some("the answer"),
            None
        )),
        Some(FusionPrintOutcome::FinalText("the answer".to_string()))
    );
    assert_eq!(
        fusion_print_outcome(&fusion_state(
            tasks::state::TaskStatus::Failed,
            None,
            Some("too few fusion models")
        )),
        Some(FusionPrintOutcome::Failed(
            "too few fusion models".to_string()
        ))
    );
    // No recorded error text still yields a diagnostic, not a panic/None.
    assert_eq!(
        fusion_print_outcome(&fusion_state(tasks::state::TaskStatus::Failed, None, None)),
        Some(FusionPrintOutcome::Failed("fusion run failed".to_string()))
    );
    assert_eq!(
        fusion_print_outcome(&fusion_state(tasks::state::TaskStatus::Killed, None, None)),
        Some(FusionPrintOutcome::Other("Killed".to_string()))
    );
}

#[tokio::test]
async fn fusion_accounting_failure_keeps_answer_and_waits_for_publication() {
    let mut pending = fusion_state(
        tasks::state::TaskStatus::Failed,
        Some("computed despite accounting failure"),
        Some("Fusion accounting failed: durable receipt rejected"),
    );
    let tasks::state::TaskState::LocalFusion(fusion) = &mut pending else {
        unreachable!();
    };
    fusion.publication_status = lingxi_core::host::FusionPublicationStatus::Pending;
    assert!(!fusion_result_ready(&pending));
    let mut published = pending.clone();
    let tasks::state::TaskState::LocalFusion(fusion) = &mut published else {
        unreachable!();
    };
    fusion.publication_status = lingxi_core::host::FusionPublicationStatus::Published;
    assert!(fusion_result_ready(&published));
    let lookup = ScriptedLookup::new(vec![Some(pending), Some(published)]);
    let sink = RecordingFusionSink::default();
    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR
    );
    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[(
            "fusion".to_string(),
            "computed despite accounting failure".to_string()
        )]
    );
    assert_eq!(
        sink.errors.lock().await.as_slice(),
        &[(
            "fusion".to_string(),
            "Fusion accounting failed: durable receipt rejected".to_string()
        )]
    );
}

#[test]
fn unsupported_publication_is_ready_but_fails_with_the_answer_retained() {
    let mut state = fusion_state(
        tasks::state::TaskStatus::Completed,
        Some("computed answer"),
        None,
    );
    let tasks::state::TaskState::LocalFusion(fusion) = &mut state else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    fusion.publication_status = lingxi_core::host::FusionPublicationStatus::NotRequired;

    assert!(fusion_result_ready(&state));
    let outcome = fusion_print_outcome(&state);
    assert!(matches!(
        outcome,
        Some(FusionPrintOutcome::PublicationFailed { ref answer, .. })
            if answer == "computed answer"
    ));
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR
    );
}

#[tokio::test]
async fn await_local_fusion_result_prints_final_text_when_already_completed() {
    let lookup = ScriptedLookup::new(vec![Some(fusion_state(
        tasks::state::TaskStatus::Completed,
        Some("the sanitized answer"),
        None,
    ))]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[("fusion".to_string(), "the sanitized answer".to_string())]
    );
    assert!(sink.errors.lock().await.is_empty());
    assert_eq!(lookup.call_count(), 1);
    assert_eq!(
        outcome,
        Some(FusionPrintOutcome::FinalText(
            "the sanitized answer".to_string()
        ))
    );
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::SUCCESS,
        "a produced answer must exit 0"
    );
}

#[tokio::test]
async fn await_local_fusion_result_reports_durable_queue_without_failing() {
    let mut queued = fusion_state(
        tasks::state::TaskStatus::Completed,
        Some("queued answer"),
        None,
    );
    let tasks::state::TaskState::LocalFusion(fusion) = &mut queued else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    fusion.publication_status = lingxi_core::host::FusionPublicationStatus::Queued;
    let lookup = ScriptedLookup::new(vec![Some(queued)]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        outcome,
        Some(FusionPrintOutcome::Queued("queued answer".to_string()))
    );
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::SUCCESS
    );
    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[("fusion".to_string(), "queued answer".to_string())]
    );
    assert!(sink
        .errors
        .lock()
        .await
        .iter()
        .any(|(_, message)| { message.contains("durably queued") }));
}

#[tokio::test]
async fn await_local_fusion_result_keeps_answer_but_fails_on_storage_error() {
    let mut failed_publication = fusion_state(
        tasks::state::TaskStatus::Completed,
        Some("answer despite append failure"),
        None,
    );
    let tasks::state::TaskState::LocalFusion(fusion) = &mut failed_publication else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    fusion.publication_status = lingxi_core::host::FusionPublicationStatus::StorageFailure;
    fusion.publication_error = Some("append failed".to_string());
    let lookup = ScriptedLookup::new(vec![Some(failed_publication)]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;

    assert_eq!(
        outcome,
        Some(FusionPrintOutcome::PublicationFailed {
            answer: "answer despite append failure".to_string(),
            reason: "append failed".to_string(),
        })
    );
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR
    );
    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[(
            "fusion".to_string(),
            "answer despite append failure".to_string()
        )]
    );
    assert_eq!(
        sink.errors.lock().await.as_slice(),
        &[("fusion".to_string(), "append failed".to_string())]
    );
}

#[tokio::test]
async fn await_local_fusion_result_polls_until_terminal_then_prints() {
    let lookup = ScriptedLookup::new(vec![
        Some(fusion_state(tasks::state::TaskStatus::Running, None, None)),
        Some(fusion_state(tasks::state::TaskStatus::Running, None, None)),
        Some(fusion_state(
            tasks::state::TaskStatus::Completed,
            Some("finished after polling"),
            None,
        )),
    ]);
    let sink = RecordingFusionSink::default();

    await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(2),
    )
    .await;

    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[("fusion".to_string(), "finished after polling".to_string())]
    );
    assert_eq!(
        lookup.call_count(),
        3,
        "must have actually polled across the two Running states"
    );
}

/// Review finding #17: `finish_fusion_terminal` flips the task to
/// `Completed` BEFORE the handler's worker has awaited
/// `FusionCompletionSink::publish` (the durable `<fusion-result>`
/// session append) — that ordering is required by the registry's own
/// terminal-status-gated notification drain and cannot flip. A waiter
/// that returns the instant it observes `Completed` can therefore race
/// the still-in-flight append: in print mode the process exits right
/// after this function returns, so the append is aborted mid-flight and
/// the session never gets its `<fusion-result>` row. This pins that the
/// waiter keeps polling a `Completed`-but-not-yet-published run instead
/// of returning immediately, and only reports the outcome once the
/// publication receipt leaves `Pending`.
#[tokio::test]
async fn await_local_fusion_result_waits_for_publish_before_reporting_completed() {
    let unpublished = tasks::state::TaskState::LocalFusion(tasks::state::LocalFusionTaskState {
        publication_status: lingxi_core::host::FusionPublicationStatus::Pending,
        ..completed_fusion_state("not yet on disk")
    });
    let published = tasks::state::TaskState::LocalFusion(tasks::state::LocalFusionTaskState {
        publication_status: lingxi_core::host::FusionPublicationStatus::Published,
        ..completed_fusion_state("not yet on disk")
    });
    let lookup = ScriptedLookup::new(vec![
        Some(unpublished.clone()),
        Some(unpublished),
        Some(published),
    ]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(2),
    )
    .await;

    assert_eq!(
        lookup.call_count(),
        3,
        "must keep polling past the first two Completed-but-unpublished \
         observations instead of returning on the first one"
    );
    assert_eq!(
        outcome,
        Some(FusionPrintOutcome::FinalText("not yet on disk".to_string())),
        "must still report the real outcome once publish lands"
    );
    assert_eq!(
        sink.outputs.lock().await.as_slice(),
        &[("fusion".to_string(), "not yet on disk".to_string())],
        "must print exactly once, after publish lands — never on an \
         unpublished Completed observation"
    );
}

#[tokio::test]
async fn await_local_fusion_result_reports_the_failure_reason() {
    let lookup = ScriptedLookup::new(vec![Some(fusion_state(
        tasks::state::TaskStatus::Failed,
        None,
        Some("TooFewModels"),
    ))]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;

    assert!(sink.outputs.lock().await.is_empty());
    assert_eq!(
        sink.errors.lock().await.as_slice(),
        &[("fusion".to_string(), "TooFewModels".to_string())]
    );
    // §13: a Failed run must not exit 0 — a CI script gating on `$?`
    // must be able to tell a deliberation that produced no answer apart
    // from one that succeeded.
    assert_eq!(
        outcome,
        Some(FusionPrintOutcome::Failed("TooFewModels".to_string()))
    );
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR,
        "a Failed fusion run must exit non-zero"
    );
}

#[tokio::test]
async fn await_local_fusion_result_times_out_with_a_named_diagnostic_instead_of_hanging() {
    let lookup = ScriptedLookup::new(vec![Some(fusion_state(
        tasks::state::TaskStatus::Running,
        None,
        None,
    ))]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_millis(5),
    )
    .await;

    assert!(sink.outputs.lock().await.is_empty());
    let errors = sink.errors.lock().await;
    assert_eq!(
        errors.len(),
        1,
        "exactly one timeout diagnostic: {errors:?}"
    );
    assert_eq!(errors[0].0, "fusion");
    assert!(
        errors[0].1.contains("ftest0001"),
        "diagnostic must name the task id: {errors:?}"
    );
    // Review finding #6: print mode is confirmed the only caller of this
    // function and exits with this outcome moments after it returns,
    // aborting the in-process worker — the diagnostic must say so
    // instead of the old, false "it may still be running".
    assert!(
        errors[0].1.contains("aborts the run's in-process worker"),
        "{errors:?}"
    );
    assert!(
        !errors[0].1.contains("may still be running"),
        "print mode is this function's only caller and exits right \
         after — nothing survives to still be running: {errors:?}"
    );
    // §13: the print-mode timeout must also exit non-zero — THIS
    // process printed no answer, and (finding #6) its worker is gone
    // too, not merely unreported.
    assert_eq!(outcome, None);
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR,
        "a print-mode timeout must exit non-zero"
    );
}

#[tokio::test]
async fn await_local_fusion_result_evicted_task_also_exits_non_zero() {
    // §13 (companion gap named by the review's second refuter): a task
    // that vanished mid-wait (evicted, or never created) prints nothing
    // at all — that must ALSO exit non-zero, not silently succeed.
    let lookup = ScriptedLookup::new(vec![None]);
    let sink = RecordingFusionSink::default();

    let outcome = await_local_fusion_result_bounded(
        "ftest0001",
        &lookup,
        &sink,
        std::time::Duration::from_millis(1),
        std::time::Duration::from_secs(1),
    )
    .await;

    assert!(sink.outputs.lock().await.is_empty());
    // [Finding 26] The evicted/never-created branch must not exit
    // non-zero SILENTLY — it now reports a named diagnostic before
    // returning `None`, same as the timeout branch already did.
    let errors = sink.errors.lock().await.clone();
    assert_eq!(errors.len(), 1, "exactly one diagnostic: {errors:?}");
    assert_eq!(errors[0].0, "fusion");
    assert!(
        errors[0].1.contains("ftest0001") && errors[0].1.contains("not found"),
        "diagnostic must name the task id: {:?}",
        errors[0].1
    );
    assert_eq!(outcome, None);
    assert_eq!(
        fusion_result_exit_code(outcome.as_ref()),
        exit_codes::RUNTIME_ERROR,
        "an evicted/never-created fusion task must exit non-zero"
    );
}

#[test]
fn fusion_result_exit_code_only_final_text_is_success() {
    assert_eq!(
        fusion_result_exit_code(Some(&FusionPrintOutcome::FinalText("ok".to_string()))),
        exit_codes::SUCCESS
    );
    assert_eq!(
        fusion_result_exit_code(Some(&FusionPrintOutcome::Failed("no".to_string()))),
        exit_codes::RUNTIME_ERROR
    );
    assert_eq!(
        fusion_result_exit_code(Some(&FusionPrintOutcome::Other("Killed".to_string()))),
        exit_codes::RUNTIME_ERROR
    );
    assert_eq!(fusion_result_exit_code(None), exit_codes::RUNTIME_ERROR);
}

/// F011: print mode must calculate its wait from the effective timeout
/// captured on this task, including finalize-tail headroom. A later
/// settings edit must not change an already-running task's deadline, and
/// a custom timeout far beyond the old 20-minute literal must survive the
/// projection unchanged.
#[test]
fn fusion_print_deadline_uses_captured_custom_timeout_with_controlled_clock() {
    let mut state = fusion_state(tasks::state::TaskStatus::Running, None, None);
    let tasks::state::TaskState::LocalFusion(fusion) = &mut state else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    // Deliberately much longer than the old duplicated default. The
    // waiter must honor this per-run snapshot rather than a CLI literal.
    fusion.effective_timeout_ms = Some(3_600_000);

    let monotonic_now = tokio::time::Instant::now();
    let deadline =
        fusion_print_deadline_with_elapsed(&state, monotonic_now, std::time::Duration::ZERO)
            .expect("captured timeout must produce a deadline");
    assert_eq!(
        deadline
            .checked_duration_since(monotonic_now)
            .expect("deadline is in the future"),
        std::time::Duration::from_millis(3_600_000 + FUSION_PRINT_FINALIZE_MARGIN_MS)
    );
}

#[test]
fn fusion_print_deadline_excludes_hook_delay_but_charges_scheduling_delay() {
    let mut state = fusion_state(tasks::state::TaskStatus::Running, None, None);
    let tasks::state::TaskState::LocalFusion(fusion) = &mut state else {
        unreachable!("fusion_state always builds a LocalFusion state");
    };
    fusion.effective_timeout_ms = Some(1_000);
    let now = tokio::time::Instant::now();
    let full_budget = std::time::Duration::from_millis(1_000 + FUSION_PRINT_FINALIZE_MARGIN_MS);

    // TaskCreated hook time is before activation and therefore contributes
    // no elapsed budget at the activation boundary.
    let after_hook = fusion_print_deadline_with_elapsed(&state, now, std::time::Duration::ZERO)
        .expect("activation must produce a deadline")
        .checked_duration_since(now)
        .expect("deadline is in the future");
    assert_eq!(after_hook, full_budget);

    // A later queue/scheduling delay is after activation and must reduce
    // the remaining Fusion budget rather than restarting it at first poll.
    let scheduling_delay = std::time::Duration::from_millis(275);
    let after_queue = fusion_print_deadline_with_elapsed(&state, now, scheduling_delay)
        .expect("activation must produce a deadline")
        .checked_duration_since(now)
        .expect("deadline is in the future");
    assert_eq!(after_queue, full_budget - scheduling_delay);

    let expired = fusion_print_deadline_with_elapsed(
        &state,
        now,
        full_budget + std::time::Duration::from_millis(1),
    )
    .expect("an expired fallback deadline is still representable")
    .checked_duration_since(now)
    .expect("deadline equals now");
    assert_eq!(expired, std::time::Duration::ZERO);
}

#[test]
fn fusion_print_deadline_prefers_monotonic_activation_and_ignores_wall_rollback() {
    let mut state = fusion_state(tasks::state::TaskStatus::Running, None, None);
    let now = tokio::time::Instant::now();
    let activation_delay = std::time::Duration::from_millis(275);
    let activation = now
        .checked_sub(activation_delay)
        .expect("controlled activation instant is representable");
    {
        let tasks::state::TaskState::LocalFusion(fusion) = &mut state else {
            unreachable!("fusion_state always builds a LocalFusion state");
        };
        fusion.effective_timeout_ms = Some(1_000);
        fusion.fusion_activation_deadline =
            activation.checked_add(std::time::Duration::from_secs(1));
        fusion.base.start_time = std::time::SystemTime::now()
            .checked_add(std::time::Duration::from_secs(3_600))
            .expect("future wall-clock fixture is representable");
    }

    let deadline =
        fusion_print_deadline(&state, now).expect("monotonic activation deadline must be used");
    assert_eq!(
        deadline
            .checked_duration_since(now)
            .expect("finalize margin keeps the deadline in the future"),
        std::time::Duration::from_millis(FUSION_PRINT_FINALIZE_MARGIN_MS + 1_000)
            - activation_delay
    );

    {
        let tasks::state::TaskState::LocalFusion(fusion) = &mut state else {
            unreachable!("fusion_state always builds a LocalFusion state");
        };
        fusion.fusion_activation_deadline = None;
    }
    let fallback =
        fusion_print_deadline(&state, now).expect("legacy wall-clock fallback remains bounded");
    assert_eq!(
        fallback
            .checked_duration_since(now)
            .expect("wall-clock rollback falls back to the full captured budget"),
        std::time::Duration::from_millis(1_000 + FUSION_PRINT_FINALIZE_MARGIN_MS)
    );
}

#[test]
fn fusion_print_deadline_safely_unbounds_on_instant_overflow() {
    assert!(
        checked_fusion_print_deadline(tokio::time::Instant::now(), std::time::Duration::MAX,)
            .is_none()
    );
}
#[test]
fn print_winddown_waits_for_jobs_but_not_monitor_subscriptions_or_parked_agents() {
    let mut task = lingxi_core::host::task_registry::TaskRecord {
        task_type: "local_bash".into(),
        status: "running".into(),
        ..Default::default()
    };
    assert!(print_task_keeps_session_alive(&task));
    task.kind = Some("monitor".into());
    assert!(!print_task_keeps_session_alive(&task));
    task.task_type = "local_agent".into();
    task.kind = None;
    assert!(print_task_keeps_session_alive(&task));
    task.status = "completed".into();
    task.is_parked = true;
    assert!(!print_task_keeps_session_alive(&task));
}

#[derive(Default)]
pub(super) struct FixtureHost;
#[async_trait::async_trait]
impl crate::headless::host::HeadlessHostServices for FixtureHost {
    fn environment_variable(&self, _: &str) -> Option<String> {
        None
    }
    fn update_environment_variables(
        &self,
        _: std::collections::HashMap<String, String>,
    ) -> Result<(), String> {
        Ok(())
    }
}

pub(super) async fn fixture_runtime(
    stream: Arc<StreamJsonStream>,
    root: &std::path::Path,
) -> Runtime {
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    let mut config = crate::desktop::DesktopConfig {
        lingxi_home: root.join("home"),
        cwd: project,
        isolated_credential_storage: true,
        use_noop_permission_gate: true,
        composition: Some(crate::desktop::DesktopSessionComposition::HeadlessCli),
        ..Default::default()
    };
    let prepared =
        crate::headless::session::prepare(&mut config, &crate::headless::SessionStart::New)
            .await
            .unwrap();
    let inner = crate::headless::session::build_prepared(
        config,
        prepared,
        stream,
        Arc::new(crate::headless::HeadlessPermissionRequestSink),
    )
    .await
    .unwrap();
    Runtime {
        inner,
        services: Arc::new(FixtureHost),
        output: crate::headless::io::Output::new(tokio::io::sink()),
        stdout: crate::headless::io::Output::new(tokio::io::sink()),
        shutdown: tokio_util::sync::CancellationToken::new(),
        active_turn: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
        failures: std::sync::Mutex::new(Vec::new()),
        execution_errors: Arc::new(std::sync::Mutex::new(Vec::new())),
        max_turns: None,
        execution_code: std::sync::atomic::AtomicI32::new(0),
        execution_interrupted: std::sync::atomic::AtomicBool::new(false),
        shutdown_complete: std::sync::atomic::AtomicBool::new(false),
    }
}

#[tokio::test]
async fn print_branch_boundary_tears_down_tasks_on_error_and_budget_exit() {
    use lingxi_core::host::task_registry::{TaskCreateInput, TaskRegistryHandle};
    let root = tempfile::tempdir().unwrap();
    let stream = Arc::new(StreamJsonStream::new_placeholder(
        crate::headless::io::Output::new(tokio::io::sink()),
    ));
    let runtime = fixture_runtime(stream, root.path()).await;
    let registry = runtime.task_registry.as_ref();
    let sink = crate::headless::output::PlainSink::new(
        crate::headless::io::Output::new(tokio::io::sink()),
        crate::headless::io::Output::new(tokio::io::sink()),
    );
    for (code, budget) in [
        (exit_codes::RUNTIME_ERROR, None),
        (exit_codes::SUCCESS, Some(0.0)),
    ] {
        let task = TaskRegistryHandle::create(
            registry,
            TaskCreateInput {
                task_type: "local_bash".into(),
                description: "branch cleanup fixture".into(),
            },
        )
        .await
        .unwrap();
        let actual = finish_print_branch(&runtime, budget, &sink, code).await;
        assert_eq!(actual, code);
        let record = TaskRegistryHandle::get(registry, &task.task_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.status, "killed",
            "every print branch settles live registry work"
        );
    }
    assert!(
        runtime
            .session_lifecycle
            .shutdown_and_drain()
            .await
            .complete
    );
}
