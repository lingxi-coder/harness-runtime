//! `tengu_goal_evaluated` — the outcome of every stop-time goal evaluation.
//!
//! Upstream 2.1.267 emits it from the query generator's `finally`
//! (`src_163219561.js` @4343091), so it fires once per Stop dispatch whenever a
//! goal was active when the dispatch began — including the dispatches where the
//! goal was never evaluated at all:
//!
//! ```js
//! if (Fe) { let Qe = p.abortController.signal.aborted,
//!           Je = We==="met"||We==="not_met"||We==="impossible";
//!   i("tengu_goal_evaluated",{ outcome:u(We ?? (_e?"error":Qe?"cancelled":"absent")),
//!     durationMs:Date.now()-D, iterations:Fe.iterations+(Je?1:0),
//!     parentAborted:Qe, origin:we(Fe.origin), ...Xe }) }
//! ```
//!
//! Three details make this event easy to port wrong, and each has a test below:
//! `D` is the Stop-handler start (2.1.270), not the model query's; `...Xe` is set in exactly one
//! branch; and `iterations` is bumped only by the three verdict outcomes, so a
//! deferred or cancelled dispatch must report the count unchanged.

use crate::conversation::hooks_impl::GoalEvalOutcome;
use crate::conversation::MessageId;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orch(bus: Arc<telemetry::AnalyticsBus>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
    .with_analytics_bus(bus)
}

async fn bus_and_sink() -> (Arc<telemetry::AnalyticsBus>, Arc<telemetry::InMemorySink>) {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(telemetry::InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    (bus, sink)
}

async fn set_goal_with_origin(
    orch: &ConversationOrchestrator,
    iterations: u64,
    origin: lingxi_core::session::GoalOrigin,
) {
    set_goal(orch, iterations).await;
    orch.session
        .lock()
        .await
        .active_goal
        .as_mut()
        .unwrap()
        .origin = origin;
}

async fn set_goal(orch: &ConversationOrchestrator, iterations: u64) {
    let mut s = orch.session.lock().await;
    s.active_goal = Some(lingxi_core::session::ActiveGoalState {
        condition: "ship the release".into(),
        set_at: std::time::SystemTime::now(),
        last_reason: None,
        iterations,
        tokens_at_start: 0,
        origin: lingxi_core::session::GoalOrigin::User,
    });
}

fn task(id: &str, kind: &str) -> hooks::HookBackgroundTask {
    hooks::HookBackgroundTask {
        is_idle: false,
        id: id.into(),
        r#type: kind.into(),
        status: "running".into(),
        description: "doing things".into(),
        command: None,
        agent_type: None,
        server: None,
        tool: None,
        name: None,
    }
}

/// Feeds the Stop payload's background-task snapshot, which is what decides
/// whether the goal evaluation defers.
struct Tasks(Vec<hooks::HookBackgroundTask>);

#[async_trait::async_trait]
impl crate::stop_hook_snapshot::StopHookSnapshotProvider for Tasks {
    async fn background_tasks(&self) -> Vec<hooks::HookBackgroundTask> {
        self.0.clone()
    }

    async fn session_crons(&self) -> Vec<hooks::HookSessionCron> {
        Vec::new()
    }
}

/// Run one Stop dispatch through the driver-facing entry point.
async fn stop_dispatch(orch: &ConversationOrchestrator, parent_aborted: bool) {
    let mut active = false;
    let mut blocking = 0u32;
    let _ = orch
        .handle_stop_at_end(
            "end_turn",
            &mut active,
            &mut blocking,
            1,
            MessageId::new(),
            parent_aborted,
        )
        .await;
}

/// The single `tengu_goal_evaluated` on `sink`, or a panic naming what was seen.
async fn the_event(sink: &telemetry::InMemorySink) -> telemetry::LogEventMetadata {
    let events = sink.events().await;
    let mut found: Vec<_> = events
        .iter()
        .filter(|e| e.name == "tengu_goal_evaluated")
        .collect();
    assert_eq!(
        found.len(),
        1,
        "expected exactly one tengu_goal_evaluated; saw {:?}",
        events.iter().map(|e| &e.name).collect::<Vec<_>>()
    );
    found.pop().unwrap().metadata.clone()
}

fn str_field(md: &telemetry::LogEventMetadata, key: &str) -> String {
    match md.get(key) {
        Some(telemetry::AnalyticsValue::String(v)) => v.clone(),
        other => panic!("{key}: expected a string, got {other:?}"),
    }
}

fn int_field(md: &telemetry::LogEventMetadata, key: &str) -> i64 {
    match md.get(key) {
        Some(telemetry::AnalyticsValue::Int(v)) => *v,
        other => panic!("{key}: expected an int, got {other:?}"),
    }
}

fn bool_field(md: &telemetry::LogEventMetadata, key: &str) -> bool {
    match md.get(key) {
        Some(telemetry::AnalyticsValue::Bool(v)) => *v,
        other => panic!("{key}: expected a bool, got {other:?}"),
    }
}

/// The wiring proof: a Stop dispatch with a goal active emits the event at all.
/// With no goal Stop hook registered there is no verdict, so `We` is undefined
/// and the `??` fallback lands on `"absent"`.
#[tokio::test]
async fn a_stop_with_an_active_goal_but_no_verdict_reports_absent() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, 4).await;

    stop_dispatch(&orch, false).await;

    let md = the_event(&sink).await;
    assert_eq!(str_field(&md, "outcome"), "absent");
    assert!(!bool_field(&md, "parentAborted"));
    assert_eq!(
        int_field(&md, "iterations"),
        4,
        "`Je` is false for `absent`, so `Fe.iterations + 0`"
    );
    assert_eq!(str_field(&md, "origin"), "user");
}

/// `We ?? (_e ? "error" : Qe ? "cancelled" : "absent")` — the same
/// no-verdict dispatch classifies as `cancelled` once the turn was aborted.
/// Without the `parentAborted` thread this test cannot tell the two apart.
#[tokio::test]
async fn a_user_cancelled_dispatch_reports_cancelled_rather_than_absent() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, 0).await;

    stop_dispatch(&orch, true).await;

    let md = the_event(&sink).await;
    assert_eq!(str_field(&md, "outcome"), "cancelled");
    assert!(bool_field(&md, "parentAborted"));
}

/// `if (Fe)` — no goal when the dispatch began means no event. The control for
/// every test above: they would all still pass if the emitter fired
/// unconditionally.
#[tokio::test]
async fn a_stop_with_no_active_goal_emits_nothing() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);

    stop_dispatch(&orch, false).await;

    assert!(
        !sink
            .events()
            .await
            .iter()
            .any(|e| e.name == "tengu_goal_evaluated"),
        "a session with no goal must not report a goal evaluation"
    );
}

/// `Xe = {activeAgents: j(An, A9t), activeShells: j(An, v9t)}`, set only where
/// `We = "deferred"`. `A9t` covers the four agent-ish task types and `v9t` the
/// single shell type, so the two counts are independent — not a total and a
/// remainder.
#[tokio::test]
async fn a_deferred_evaluation_splits_agents_from_shells() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus).with_stop_hook_snapshot(Arc::new(Tasks(vec![
        task("b1", "shell"),
        task("a1", "subagent"),
        task("a2", "subagent"),
        task("t1", "teammate"),
        task("w1", "workflow"),
        task("c1", "cloud session"),
    ])));
    set_goal(&orch, 7).await;

    stop_dispatch(&orch, false).await;

    let md = the_event(&sink).await;
    assert_eq!(str_field(&md, "outcome"), "deferred");
    assert_eq!(int_field(&md, "activeShells"), 1);
    assert_eq!(
        int_field(&md, "activeAgents"),
        5,
        "subagent + subagent + teammate + workflow + cloud session"
    );
    assert_eq!(
        int_field(&md, "iterations"),
        7,
        "a deferred dispatch never evaluated the goal, so `Je` is false"
    );
}

/// `...Xe` contributes nothing outside the deferred branch. A spread applied
/// unconditionally would put two zeroes on every other outcome.
#[tokio::test]
async fn a_non_deferred_evaluation_carries_no_task_counts() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, 0).await;

    stop_dispatch(&orch, false).await;

    let md = the_event(&sink).await;
    assert_eq!(str_field(&md, "outcome"), "absent");
    assert!(
        md.get("activeAgents").is_none() && md.get("activeShells").is_none(),
        "`Xe` is assigned in the deferred branch only: {md:?}"
    );
}

/// 2.1.270 starts D at the Stop-handler entry, excluding the model query.
#[tokio::test]
async fn the_duration_excludes_the_model_query() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, 0).await;
    *orch
        .compaction_runtime
        .query_started_at
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        std::time::Instant::now() - std::time::Duration::from_millis(750);

    stop_dispatch(&orch, false).await;

    assert!(
        int_field(&the_event(&sink).await, "durationMs") < 700,
        "Stop evaluation must not include the earlier model query"
    );
}

/// `Je = We==="met"||We==="not_met"||We==="impossible"` — the three verdicts
/// bump `iterations`, and nothing else does. Driven directly because the
/// verdict outcomes need a goal Stop hook to actually execute.
#[tokio::test]
async fn only_a_real_verdict_bumps_the_iteration_count() {
    for (outcome, expected) in [
        (GoalEvalOutcome::Met, 6),
        (GoalEvalOutcome::NotMet, 6),
        (GoalEvalOutcome::Impossible, 6),
        (GoalEvalOutcome::Deferred, 5),
        (GoalEvalOutcome::Error, 5),
        (GoalEvalOutcome::Cancelled, 5),
        (GoalEvalOutcome::Absent, 5),
    ] {
        let (bus, sink) = bus_and_sink().await;
        let orch = orch(bus);
        let goal = lingxi_core::session::ActiveGoalState {
            condition: "ship the release".into(),
            set_at: std::time::SystemTime::now(),
            last_reason: None,
            iterations: 5,
            tokens_at_start: 0,
            origin: lingxi_core::session::GoalOrigin::User,
        };

        orch.fire_goal_evaluated(&goal, outcome, false, &[], std::time::Duration::ZERO)
            .await;

        let md = the_event(&sink).await;
        assert_eq!(
            int_field(&md, "iterations"),
            expected,
            "{outcome:?} counts_as_iteration = {}",
            outcome.counts_as_iteration()
        );
    }
}

/// The seven `outcome` spellings, which are the strings the metric is grouped
/// by downstream. A renamed variant that still compiles would silently split
/// every dashboard.
#[tokio::test]
async fn the_outcome_strings_match_the_oracle_vocabulary() {
    assert_eq!(
        [
            GoalEvalOutcome::Met,
            GoalEvalOutcome::NotMet,
            GoalEvalOutcome::Impossible,
            GoalEvalOutcome::Deferred,
            GoalEvalOutcome::Error,
            GoalEvalOutcome::Cancelled,
            GoalEvalOutcome::Absent,
        ]
        .map(GoalEvalOutcome::as_str),
        [
            "met",
            "not_met",
            "impossible",
            "deferred",
            "error",
            "cancelled",
            "absent"
        ]
    );
}

/// `origin: we(Fe.origin)`. Upstream's `y()` defaults the field to `"user"`,
/// but `mon` stamps `"restored"` on every goal a resume recovers — so a goal
/// that came back with the session must not be reported as one the user just
/// set. This port hardcoded `"user"` at all three `tengu_goal_*` emitters.
#[tokio::test]
async fn a_resumed_goal_reports_its_origin_as_restored() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal_with_origin(&orch, 2, lingxi_core::session::GoalOrigin::Restored).await;

    stop_dispatch(&orch, false).await;

    assert_eq!(str_field(&the_event(&sink).await, "origin"), "restored");
}

/// The control: an ordinary `/goal` still reports `y()`'s `"user"` fallback.
#[tokio::test]
async fn a_goal_set_in_this_session_reports_its_origin_as_user() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal_with_origin(&orch, 2, lingxi_core::session::GoalOrigin::User).await;

    stop_dispatch(&orch, false).await;

    assert_eq!(str_field(&the_event(&sink).await, "origin"), "user");
}

// ── batched cancellation and Stop dispatch ───────────────────────────────────
//
// A cancellation during the model step now ends the cancelable driver before
// Stop dispatch, even if the API also returns a successful response. The old
// fixture relied on a select race allowing that response to reach Stop with an
// already-cancelled token. Keep public cancellation and successful Stop
// dispatch as separate contracts; the direct Stop tests above cover an aborted
// parent's outcome classification when Stop dispatch actually occurs.

/// Cancels while producing a model response, then returns that response.
struct CancelsThenAnswers(tokio_util::sync::CancellationToken);

#[async_trait::async_trait]
impl crate::OrchestratorApiClient for CancelsThenAnswers {
    async fn messages_create(
        &self,
        request: crate::OrchestratorApiRequest,
    ) -> Result<llm_runtime::HistoryResponse, llm_runtime::LlmError> {
        let (request_model, request_profile, request_system, _msgs, _tools) = match request {
            crate::OrchestratorApiRequest::Main(request) => (
                request.model,
                request.profile,
                request.system.map(|system| system.display_text()),
                request.messages,
                request.tools,
            ),
            crate::OrchestratorApiRequest::HookPrompt(request) => (
                request.model,
                request.profile,
                Some(request.system),
                request.messages,
                Vec::new(),
            ),
        };
        let _model = request_model.as_str();
        let _profile = request_profile.as_deref();
        let _system = request_system.as_deref();

        self.0.cancel();
        Ok(crate::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "done".into(),
                cache_control: None, citations: None,
            }],
            Some("end_turn"),
        ))
    }
}

fn orch_with_api(
    bus: Arc<telemetry::AnalyticsBus>,
    api: Arc<dyn crate::OrchestratorApiClient>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
    .with_analytics_bus(bus)
}

#[tokio::test]
async fn a_cancelled_model_response_does_not_dispatch_stop_goal_analytics() {
    let (bus, sink) = bus_and_sink().await;
    let token = tokio_util::sync::CancellationToken::new();
    let cancelable = orch_with_api(bus, Arc::new(CancelsThenAnswers(token.clone())));
    set_goal(&cancelable, 0).await;
    let outcome = cancelable
        .run_turn_with_cancel("hi", token.clone())
        .await
        .expect("turn");
    assert!(
        token.is_cancelled(),
        "the fixture must actually have cancelled the token, or this proves nothing"
    );
    assert_eq!(
        outcome,
        crate::TurnOutcome::Cancelled,
        "cancellation during the model step must end the public driver before Stop"
    );
    assert!(
        !sink
            .events()
            .await
            .iter()
            .any(|event| event.name == "tengu_goal_evaluated"),
        "a model-step cancellation must not emit analytics for an unrun Stop dispatch"
    );
}

#[tokio::test]
async fn successful_batched_entries_dispatch_goal_analytics_without_parent_abort() {
    for cancelable in [false, true] {
        let (bus, sink) = bus_and_sink().await;
        let orch = orch_with_api(
            bus,
            Arc::new(MockApiClient::new(vec![
                crate::test_support::mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "done".into(),
                        cache_control: None,
                        citations: None,
                    }],
                    Some("end_turn"),
                ),
            ])),
        );
        set_goal(&orch, 0).await;
        if cancelable {
            assert_eq!(
                orch.run_turn_with_cancel("hi", tokio_util::sync::CancellationToken::new())
                    .await
                    .expect("cancelable turn"),
                crate::TurnOutcome::EndTurn,
            );
        } else {
            assert!(matches!(
                orch.run_turn("hi").await.expect("plain turn"),
                crate::ConversationOutcome::EndTurn { turn_count: 1, .. }
            ));
        }
        let md = the_event(&sink).await;
        assert!(
            !bool_field(&md, "parentAborted"),
            "a successful batched turn has no parent abort: cancelable={cancelable}"
        );
        assert_eq!(str_field(&md, "outcome"), "absent");
    }
}
