//! `tengu_goal_cleared` must name the reason the goal actually died of.
//!
//! Upstream 2.1.267 emits the event through one helper whose reason is an
//! argument (`src_161508826.js`):
//!
//! ```js
//! function kB(e,t){i("tengu_goal_cleared",{reason:u(t),iterations:e.iterations,
//!   durationMs:Date.now()-e.setAt,origin:we(e.origin)})}
//! ```
//!
//! and it is reached with six different values. This port used to hardcode
//! `"user_clear"` at the single emission site, so a goal torn down by a context
//! overflow or a provider error was indistinguishable in the metric from one the
//! user cleared by hand — and a goal replaced by a newer one emitted nothing at
//! all.

use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::turn_loop::{clear_goal_after_unrecoverable_error, GoalClearReason};
use crate::ConversationOrchestrator;
use crate::OrchestratorConfig;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// Like [`orch`], but keeps the output stream so a test can read what the user
/// was actually shown.
fn orch_with_output(
    bus: Arc<telemetry::AnalyticsBus>,
) -> (ConversationOrchestrator, MockOutputStream) {
    let output = MockOutputStream::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig {
            interactive_session: true,
            ..Default::default()
        },
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(output.clone()),
        Arc::new(StaticMemoryProvider::with_files(vec![])),
        PathBuf::from("/work/repo"),
    )
    .with_analytics_bus(bus);
    (orch, output)
}

async fn system_notices(output: &MockOutputStream) -> Vec<String> {
    output
        .snapshot()
        .await
        .into_iter()
        .filter_map(|event| match event {
            platform_api::OutputEvent::SystemNotice { body, .. } => Some(body),
            _ => None,
        })
        .collect()
}

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

async fn set_goal(orch: &ConversationOrchestrator, condition: &str) {
    let mut s = orch.session.lock().await;
    s.active_goal = Some(lingxi_core::session::ActiveGoalState {
        condition: condition.into(),
        set_at: std::time::SystemTime::now(),
        last_reason: None,
        iterations: 0,
        tokens_at_start: 0,
        origin: lingxi_core::session::GoalOrigin::User,
    });
}

/// Every `tengu_goal_cleared` reason seen on `sink`, in emission order.
async fn cleared_reasons(sink: &telemetry::InMemorySink) -> Vec<String> {
    sink.events()
        .await
        .iter()
        .filter(|event| event.name == "tengu_goal_cleared")
        .map(|event| match event.metadata.get("reason") {
            Some(telemetry::AnalyticsValue::String(reason)) => reason.clone(),
            other => panic!("tengu_goal_cleared without a string reason: {other:?}"),
        })
        .collect()
}

async fn bus_and_sink() -> (Arc<telemetry::AnalyticsBus>, Arc<telemetry::InMemorySink>) {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let sink = Arc::new(telemetry::InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    (bus, sink)
}

#[tokio::test]
async fn a_context_limit_teardown_is_not_reported_as_a_user_clear() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, "ship the release").await;

    clear_goal_after_unrecoverable_error(&orch, GoalClearReason::ContextLimit).await;

    assert_eq!(
        cleared_reasons(&sink).await,
        vec!["context_limit".to_string()],
        "`kB(e, d===\"context_limit\" ? \"context_limit\" : \"api_error\")`"
    );
}

#[tokio::test]
async fn a_provider_error_teardown_is_reported_as_an_api_error() {
    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, "ship the release").await;

    // `billing_error` is a non-transient arm, so it clears (bucket `Billing`),
    // and every non-`ContextLimit` bucket is `api_error` upstream.
    clear_goal_after_unrecoverable_error(
        &orch,
        GoalClearReason::ApiError {
            error_kind: Some("billing_error"),
            is_transient: false,
        },
    )
    .await;

    assert_eq!(
        cleared_reasons(&sink).await,
        vec!["api_error".to_string()],
        "a billing failure is an api_error, not a user_clear"
    );
}

#[tokio::test]
async fn an_explicit_clear_still_reports_user_clear() {
    use platform_api::OrchestratorHandle as _;

    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);
    set_goal(&orch, "ship the release").await;

    orch.clear_active_goal().await.expect("a goal was active");

    assert_eq!(
        cleared_reasons(&sink).await,
        vec!["user_clear".to_string()],
        "`kB(n,\"user_clear\")` — the arm that was always right"
    );
}

#[tokio::test]
async fn replacing_a_live_goal_reports_superseded() {
    use platform_api::OrchestratorHandle as _;

    let (bus, sink) = bus_and_sink().await;
    let orch = orch(bus);

    // The first `/goal` has nothing to supersede.
    orch.set_active_goal("ship the release").await;
    assert!(
        cleared_reasons(&sink).await.is_empty(),
        "setting the first goal tears nothing down"
    );

    // `let l = t.getAppState().activeGoal; if (l !== void 0) kB(l,"superseded")`
    orch.set_active_goal("cut the branch instead").await;

    assert_eq!(
        cleared_reasons(&sink).await,
        vec!["superseded".to_string()],
        "a goal replaced by a newer one is torn down and must say so"
    );
}

/// OR-4 (2.1.269) — a turn that failed WITHOUT qualifying for a clear used to
/// leave the goal silently sitting there. That is the reported stall: the goal
/// evaluation runs off the Stop hook, a failed turn yields no disposition, and
/// nothing told the user. The goal now says why it stopped driving.
#[tokio::test]
async fn a_rejected_request_pauses_the_goal_out_loud() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let (orch, output) = orch_with_output(bus);
    set_goal(&orch, "ship the port").await;

    clear_goal_after_unrecoverable_error(
        &orch,
        GoalClearReason::ApiError {
            error_kind: Some("invalid_request"),
            is_transient: false,
        },
    )
    .await;

    assert_eq!(
        system_notices(&output).await,
        vec![
            "Goal paused · the API rejected the last request · send a message to continue, or run /goal clear"
                .to_string()
        ]
    );
    // PAUSED, not cleared: the goal stays set so a message resumes it.
    assert!(
        orch.session.lock().await.active_goal.is_some(),
        "a pause must not clear the goal"
    );
}

/// A rate limit is its own sentence, and likewise does not clear the goal.
#[tokio::test]
async fn a_rate_limit_pauses_with_its_own_sentence() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let (orch, output) = orch_with_output(bus);
    set_goal(&orch, "ship the port").await;

    clear_goal_after_unrecoverable_error(
        &orch,
        GoalClearReason::ApiError {
            error_kind: Some("rate_limit"),
            is_transient: false,
        },
    )
    .await;

    assert_eq!(
        system_notices(&output).await,
        vec!["Goal paused · the request was rate limited · send a message to retry".to_string()]
    );
    assert!(orch.session.lock().await.active_goal.is_some());
}

/// The CLEAR tier is untouched: it still clears, and it still shows its own
/// warning rather than a pause sentence.
#[tokio::test]
async fn the_clear_tier_still_clears_and_says_so() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let (orch, output) = orch_with_output(bus);
    set_goal(&orch, "ship the port").await;

    clear_goal_after_unrecoverable_error(
        &orch,
        GoalClearReason::ApiError {
            error_kind: Some("billing_error"),
            is_transient: false,
        },
    )
    .await;

    let notices = system_notices(&output).await;
    assert_eq!(notices.len(), 1);
    assert!(
        notices[0]
            .starts_with("Goal cleared after an unrecoverable error (credit balance too low)"),
        "got {:?}",
        notices[0]
    );
    assert!(
        orch.session.lock().await.active_goal.is_none(),
        "the clear tier must still clear"
    );
}

/// With NO goal set, a failed turn says nothing at all — the pause tier must not
/// start narrating errors to users who never ran `/goal`.
#[tokio::test]
async fn without_an_active_goal_nothing_is_announced() {
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let (orch, output) = orch_with_output(bus);

    clear_goal_after_unrecoverable_error(
        &orch,
        GoalClearReason::ApiError {
            error_kind: Some("invalid_request"),
            is_transient: false,
        },
    )
    .await;

    assert!(system_notices(&output).await.is_empty());
}
