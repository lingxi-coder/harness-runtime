//! Task 8 (llm-runtime future-work batch 3): the orchestrator forwards the
//! adapter's unified rate-limit header snapshot to
//! `OutputStream::emit_rate_limit` after each completed API call, emitting
//! ONLY when the snapshot differs from the last emitted value
//! (emit-on-change dedup).
//!
//! Mirrors the turn-driving pattern of `orchestrator_multi_turn_test.rs`:
//! `MockApiClient` (extended with `set_rate_limit_full`) + the recording
//! `MockOutputStream`.

use lingxi_core::host::OutputEvent;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::model::rate_limit::RateLimitInfo;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// A single-text `end_turn` response so each `run_turn` is exactly one API call.
fn end_turn_response(text: &str) -> llm_runtime::HistoryResponse {
    mock_message_response(
        vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
            citations: None,
        }],
        Some("end_turn"),
    )
}

/// A fully-populated internal snapshot (all nine post-T7 fields, the
/// B4-T2 `surpassed_threshold`, plus the four 2.1.206 overage fields —
/// internal-only: the nine-argument `emit_rate_limit` event surface is
/// unchanged).
fn sample_info() -> RateLimitInfo {
    RateLimitInfo {
        rate_limit_type: Some("five_hour".into()),
        overage_status: Some("allowed".into()),
        overage_disabled_reason: Some("out_of_credits".into()),
        status: Some("allowed_warning".into()),
        resets_at: Some(1_760_000_000),
        utilization: Some(0.85),
        claim_resets_at: Some(1_760_000_100),
        overage_resets_at: Some(1_760_000_200),
        fallback_available: Some(true),
        surpassed_threshold: Some(0.9),
        overage_in_use: true,
        upgrade_paths: Some(vec!["upgrade_plan".to_string(), "overage".to_string()]),
        overage_period_monthly_utilization: Some(0.42),
        overage_period_channel_utilization: Some(0.10),
        credits_required: false,
    }
}

fn build_orch(api: Arc<MockApiClient>, output: Arc<MockOutputStream>) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

/// Project the captured `RateLimit` events out of the full event stream.
fn rate_limit_events(events: &[OutputEvent]) -> Vec<OutputEvent> {
    events
        .iter()
        .filter(|e| matches!(e, OutputEvent::RateLimit { .. }))
        .cloned()
        .collect()
}

/// (a) First `Some` snapshot after a completed API call → exactly one
/// `RateLimit` event, with every field mapped 1:1 from the internal struct.
#[tokio::test]
async fn emits_rate_limit_on_first_snapshot() {
    let api = Arc::new(MockApiClient::new(vec![end_turn_response("hi")]));
    api.set_rate_limit_full(Some(sample_info()));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("hello").await.expect("turn 1");

    let events = rate_limit_events(&output.snapshot().await);
    assert_eq!(events.len(), 1, "exactly one RateLimit event: {events:?}");
    let OutputEvent::RateLimit {
        status,
        rate_limit_type,
        utilization,
        resets_at,
        claim_resets_at,
        overage_status,
        overage_resets_at,
        overage_disabled_reason,
        fallback_available,
        upgrade_paths,
        credits_required,
    } = &events[0]
    else {
        panic!("expected RateLimit event");
    };
    assert_eq!(status.as_deref(), Some("allowed_warning"));
    assert_eq!(rate_limit_type.as_deref(), Some("five_hour"));
    assert_eq!(*utilization, Some(0.85));
    assert_eq!(*resets_at, Some(1_760_000_000));
    assert_eq!(*claim_resets_at, Some(1_760_000_100));
    assert_eq!(overage_status.as_deref(), Some("allowed"));
    assert_eq!(*overage_resets_at, Some(1_760_000_200));
    assert_eq!(overage_disabled_reason.as_deref(), Some("out_of_credits"));
    assert_eq!(*fallback_available, Some(true));
    assert_eq!(
        upgrade_paths.as_deref(),
        Some(&["upgrade_plan".to_string(), "overage".to_string()][..])
    );
    assert!(!credits_required);
}

/// (b) An IDENTICAL snapshot across two turns must NOT be re-emitted —
/// emit-on-change means the second turn produces no second event.
#[tokio::test]
async fn no_duplicate_emit_for_identical_snapshot_across_turns() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("one"),
        end_turn_response("two"),
    ]));
    api.set_rate_limit_full(Some(sample_info()));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("first").await.expect("turn 1");
    orch.run_turn("second").await.expect("turn 2");

    let events = rate_limit_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        1,
        "identical snapshot must emit exactly once: {events:?}"
    );
}

/// (c) A CHANGED snapshot re-emits: two turns with different snapshots →
/// two events, the second carrying the new values.
#[tokio::test]
async fn re_emits_when_snapshot_changes() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("one"),
        end_turn_response("two"),
    ]));
    api.set_rate_limit_full(Some(sample_info()));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("first").await.expect("turn 1");

    let changed = RateLimitInfo {
        status: Some("rejected".into()),
        utilization: Some(1.0),
        ..sample_info()
    };
    api.set_rate_limit_full(Some(changed));
    orch.run_turn("second").await.expect("turn 2");

    let events = rate_limit_events(&output.snapshot().await);
    assert_eq!(events.len(), 2, "changed snapshot must re-emit: {events:?}");
    let OutputEvent::RateLimit {
        status,
        utilization,
        rate_limit_type,
        ..
    } = &events[1]
    else {
        panic!("expected RateLimit event");
    };
    assert_eq!(status.as_deref(), Some("rejected"));
    assert_eq!(*utilization, Some(1.0));
    assert_eq!(rate_limit_type.as_deref(), Some("five_hour"));
}

/// (Task 9 bonus) The STREAMING seam in `try_run_turn_streaming` also
/// forwards the snapshot: `run_turn_streaming` over a scripted SSE stream →
/// exactly one `RateLimit` event. (`emit_rate_limit_if_changed` reads
/// `self.api`, which is the same `ProviderApiAdapter` as `streaming_api` in
/// production — here the batched mock carries the snapshot.)
#[tokio::test]
async fn streaming_turn_emits_rate_limit() {
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    let stream = orchestrator::scripted![
        message_start("msg_01", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "hi"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    batched.set_rate_limit_full(Some(sample_info()));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        streaming,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    orch.run_turn_streaming("hello").await.expect("turn 1");

    let events = rate_limit_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        1,
        "streaming seam must emit exactly once: {events:?}"
    );
    assert!(matches!(
        &events[0],
        OutputEvent::RateLimit { status, .. } if status.as_deref() == Some("allowed_warning")
    ));
}

/// No snapshot (the default `last_rate_limit_full() == None`, e.g. a
/// provider that never sent unified headers) → no `RateLimit` event at all.
#[tokio::test]
async fn no_emit_when_no_snapshot() {
    let api = Arc::new(MockApiClient::new(vec![end_turn_response("hi")]));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("hello").await.expect("turn 1");

    let events = rate_limit_events(&output.snapshot().await);
    assert!(events.is_empty(), "no snapshot → no emit: {events:?}");
}

// ════════════════════════════════════════════════════════════════════════
// RawUtilization (llm-runtime future-work batch 5, Task 2): the orchestrator
// forwards the adapter's raw per-window utilization snapshot to
// `OutputStream::emit_raw_utilization` next to the `emit_rate_limit` seam,
// emitting only on change and never for the empty snapshot.
// ════════════════════════════════════════════════════════════════════════

use orchestrator::model::rate_limit::RawUtilization;

/// Header vec carrying BOTH windows (the unified per-window quartet).
fn both_window_headers(five_h_util: &str) -> Vec<(String, String)> {
    [
        ("anthropic-ratelimit-unified-5h-utilization", five_h_util),
        ("anthropic-ratelimit-unified-5h-reset", "1750000005"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.77"),
        ("anthropic-ratelimit-unified-7d-reset", "1750000007"),
    ]
    .iter()
    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
    .collect()
}

/// Project the captured `RawUtilization` events out of the full event stream.
fn raw_utilization_events(events: &[OutputEvent]) -> Vec<OutputEvent> {
    events
        .iter()
        .filter(|e| matches!(e, OutputEvent::RawUtilization { .. }))
        .cloned()
        .collect()
}

/// (a) Response with 5h+7d unified headers → exactly one `RawUtilization`
/// event with all four fields `Some` (each window emitted atomically).
#[tokio::test]
async fn emits_raw_utilization_on_first_snapshot() {
    let api = Arc::new(MockApiClient::new(vec![end_turn_response("hi")]));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("hello").await.expect("turn 1");

    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        1,
        "exactly one RawUtilization event: {events:?}"
    );
    let OutputEvent::RawUtilization {
        five_hour_utilization,
        five_hour_resets_at,
        seven_day_utilization,
        seven_day_resets_at,
    } = &events[0]
    else {
        panic!("expected RawUtilization event");
    };
    assert_eq!(*five_hour_utilization, Some(0.42));
    assert_eq!(*five_hour_resets_at, Some(1_750_000_005));
    assert_eq!(*seven_day_utilization, Some(0.77));
    assert_eq!(*seven_day_resets_at, Some(1_750_000_007));
}

/// (b) An IDENTICAL snapshot across two turns → no second emit (TS updates
/// `rawUtilization` unconditionally on every headers pass — our event
/// channel dedupes; documented divergence).
#[tokio::test]
async fn no_duplicate_raw_utilization_for_identical_snapshot_across_turns() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("one"),
        end_turn_response("two"),
    ]));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("first").await.expect("turn 1");
    orch.run_turn("second").await.expect("turn 2");

    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        1,
        "identical snapshot must emit exactly once: {events:?}"
    );
}

/// (c) A changed 5h utilization re-emits with the new value.
#[tokio::test]
async fn re_emits_raw_utilization_when_five_hour_utilization_changes() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("one"),
        end_turn_response("two"),
    ]));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("first").await.expect("turn 1");

    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.55",
    ))));
    orch.run_turn("second").await.expect("turn 2");

    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(events.len(), 2, "changed snapshot must re-emit: {events:?}");
    let OutputEvent::RawUtilization {
        five_hour_utilization,
        seven_day_utilization,
        ..
    } = &events[1]
    else {
        panic!("expected RawUtilization event");
    };
    assert_eq!(*five_hour_utilization, Some(0.55));
    assert_eq!(*seven_day_utilization, Some(0.77));
}

/// (d) No API snapshot at all emits nothing. A parsed-but-empty snapshot is
/// emitted once as an all-`None` clearing event.
#[tokio::test]
async fn empty_raw_utilization_snapshot_emits_clear_once() {
    // Default: no snapshot at all.
    let api = Arc::new(MockApiClient::new(vec![end_turn_response("hi")]));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());
    orch.run_turn("hello").await.expect("turn 1");
    let events = raw_utilization_events(&output.snapshot().await);
    assert!(events.is_empty(), "no snapshot → no emit: {events:?}");

    // Headers present but with NO per-window quartet → empty parse → one clear.
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("hi"),
        end_turn_response("again"),
    ]));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&[(
        "anthropic-ratelimit-unified-status".to_string(),
        "allowed".to_string(),
    )])));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());
    orch.run_turn("hello").await.expect("turn 1");
    orch.run_turn("again").await.expect("turn 2");
    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(
        events,
        vec![OutputEvent::RawUtilization {
            five_hour_utilization: None,
            five_hour_resets_at: None,
            seven_day_utilization: None,
            seven_day_resets_at: None,
        }],
        "empty snapshot must emit one deduplicated clear"
    );
}

/// (e) A non-empty snapshot followed by the empty snapshot emits a clearing
/// event so downstream status UI cannot retain stale windows.
#[tokio::test]
async fn empty_raw_utilization_clears_prior_snapshot() {
    let api = Arc::new(MockApiClient::new(vec![
        end_turn_response("one"),
        end_turn_response("two"),
    ]));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    orch.run_turn("first").await.expect("turn 1");
    api.set_raw_utilization(Some(RawUtilization::default()));
    orch.run_turn("second").await.expect("turn 2");

    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        2,
        "snapshot and clear must both emit: {events:?}"
    );
    assert_eq!(
        events[1],
        OutputEvent::RawUtilization {
            five_hour_utilization: None,
            five_hour_resets_at: None,
            seven_day_utilization: None,
            seven_day_resets_at: None,
        }
    );
}

/// (f) The STREAMING seam also forwards raw utilization: `run_turn_streaming`
/// over a scripted SSE stream → exactly one `RawUtilization` event (the raw
/// hook sits next to `emit_rate_limit_if_changed` in `try_run_turn_streaming`
/// and reads the same `self.api` adapter the batched seam reads).
#[tokio::test]
async fn streaming_turn_emits_raw_utilization() {
    use orchestrator::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };

    let stream = orchestrator::scripted![
        message_start("msg_01", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "hi"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    batched.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        streaming,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));

    orch.run_turn_streaming("hello").await.expect("turn 1");

    let events = raw_utilization_events(&output.snapshot().await);
    assert_eq!(
        events.len(),
        1,
        "streaming seam must emit exactly once: {events:?}"
    );
    assert!(matches!(
        &events[0],
        OutputEvent::RawUtilization { five_hour_utilization, .. }
            if *five_hour_utilization == Some(0.42)
    ));
}

// ════════════════════════════════════════════════════════════════════════
// B6-T1: terminal-429 status-change emit parity.
//
// claude-code's terminal catch handler `extractQuotaStatusFromError`
// (claudeAiLimits.ts:487) forces the limits to `status='rejected'` and runs
// `emitStatusChange` (ts:509-511) ALONGSIDE rendering the terminal error
// copy, so the TUI shows the rate-limit banner (+ the T5 overage notice)
// next to the assistant error message. The Rust seam fires
// `emit_rate_limit_if_changed` / `emit_raw_utilization_if_changed` at the
// terminal-error mapping sites (`run_turn` & co.), AFTER the drive fn
// promoted the pending 429 into `self.api`'s caches and BEFORE
// `enrich_api_error` builds the terminal copy.
//
// These are EMIT-SEAM tests: the mock stubs the snapshot directly
// (`set_rate_limit_full` / `set_raw_utilization`) and forces a terminal
// rate-limited error (`set_fail_with`), so they exercise the emit-on-change
// hooks at the terminal sites — NOT the adapter-level pending-slot promotion
// (covered in provider_adapter.rs).
// ════════════════════════════════════════════════════════════════════════

use llm_runtime::LlmError;

/// A terminal rate-limited error with a rejected snapshot (+ raw windows)
/// cached on the API client emits exactly one `RateLimit` event (rejected)
/// AND one `RawUtilization` event, while the returned error is still the
/// enriched terminal copy. The banner renders alongside the error copy.
#[tokio::test]
async fn terminal_rate_limited_error_emits_rate_limit_event() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }));
    let rejected = RateLimitInfo {
        status: Some("rejected".into()),
        rate_limit_type: Some("seven_day".into()),
        ..RateLimitInfo::default()
    };
    api.set_rate_limit_full(Some(rejected));
    api.set_raw_utilization(Some(RawUtilization::from_headers(&both_window_headers(
        "0.42",
    ))));
    // A composed copy so the terminal error maps to the enriched surface.
    api.set_rate_limit_error_message(Some("You've hit your weekly limit".to_string()));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    let err = orch
        .run_turn("hello")
        .await
        .expect_err("turn must die on the terminal 429");
    // The returned error is still the enriched terminal copy.
    assert_eq!(err.to_string(), "You've hit your weekly limit");

    // Exactly one rejected RateLimit event, emitted at the terminal site.
    let rl = rate_limit_events(&output.snapshot().await);
    assert_eq!(rl.len(), 1, "exactly one RateLimit event: {rl:?}");
    assert!(matches!(
        &rl[0],
        OutputEvent::RateLimit { status, rate_limit_type, .. }
            if status.as_deref() == Some("rejected")
                && rate_limit_type.as_deref() == Some("seven_day")
    ));

    // And one RawUtilization event (the raw windows from the same terminal).
    let raw = raw_utilization_events(&output.snapshot().await);
    assert_eq!(raw.len(), 1, "exactly one RawUtilization event: {raw:?}");
}

/// A NON-rate-limited terminal error (e.g. transport failure) must NOT emit a
/// `RateLimit` event even if a snapshot happens to be cached — the emit hook
/// is gated on the rate-limited discriminant only.
#[tokio::test]
async fn non_rate_limited_terminal_emits_no_rate_limit_event() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::Transport {
        message: "boom".to_string(),
    }));
    // A snapshot is cached, but the terminal is not rate-limited.
    api.set_rate_limit_full(Some(sample_info()));
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    // Post-#10 the Transport error ends the turn GRACEFULLY as `model_error`
    // (no longer a hard bubble); the point of THIS test is unchanged — a
    // non-rate-limited terminal must emit NO RateLimit event.
    let _ = orch.run_turn("hello").await;

    let rl = rate_limit_events(&output.snapshot().await);
    assert!(
        rl.is_empty(),
        "non-rate-limited terminal must not emit a RateLimit event: {rl:?}"
    );
}

/// B1 — DOCUMENTED DIVERGENCE (not parity). TS forces `status='rejected'` and
/// emits even on a HEADERLESS terminal 429 (claudeAiLimits.ts:506-507 — the
/// `newLimits.status = 'rejected'` assignment sits OUTSIDE the `if
/// (error.headers)` block). The Rust seam has no "bare rejected, no windows"
/// representation in `last_rate_limit`, so a headerless terminal 429 (mock
/// snapshot `None`) emits NOTHING — the terminal error copy already conveys
/// the rejection. Pinned so the divergence stays intentional.
#[tokio::test]
async fn headerless_terminal_429_emits_no_rate_limit_event() {
    let api = Arc::new(MockApiClient::new(vec![]));
    api.set_fail_with(Some(LlmError::RateLimited {
        retry_after: None,
        scope: None,
    }));
    // Headerless terminal 429: no snapshot was recorded (default None).
    let output = Arc::new(MockOutputStream::new());
    let orch = build_orch(api.clone(), output.clone());

    let _ = orch
        .run_turn("hello")
        .await
        .expect_err("turn must die on the headerless 429");

    let rl = rate_limit_events(&output.snapshot().await);
    assert!(
        rl.is_empty(),
        "headerless terminal 429 emits no RateLimit event (B1 divergence): {rl:?}"
    );
}
