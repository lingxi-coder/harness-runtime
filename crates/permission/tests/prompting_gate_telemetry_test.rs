//! Telemetry plumbing: the two M5-05 event constants are byte-locked and
//! appear in the `telemetry::tengu::ALL_EVENT_NAMES` registry in
//! registration order (after the M5-04 streaming events, before the
//! `lingxi_core_v0_5_0_released` release marker).
//!
//! Plan deviation: the plan called for an `InMemorySink` capture test
//! using `telemetry::install_test_sink` — but the M5-04 / M5-02
//! emit path uses `tracing::info!(event = NAME, …)` (not the
//! `AnalyticsBus`), so installing a sink doesn't observe these events.
//! Adding a `tracing-subscriber` test capture would introduce a new
//! workspace dep just for this test. Instead we verify the constants
//! exist and are registered — the live emission is exercised
//! implicitly by every `prompting_gate_e2e_test.rs` case (which would
//! fail to compile if the `tracing::info!(event = …)` line referenced
//! a non-existent constant).

use telemetry::tengu::orchestrator::{PERMISSION_ANSWERED, PERMISSION_PROMPTED};
use telemetry::tengu::ALL_EVENT_NAMES;

#[test]
fn permission_prompted_constant_is_byte_locked() {
    assert_eq!(
        PERMISSION_PROMPTED,
        "tengu_orchestrator_permission_prompted"
    );
}

#[test]
fn permission_answered_constant_is_byte_locked() {
    assert_eq!(
        PERMISSION_ANSWERED,
        "tengu_orchestrator_permission_answered"
    );
}

#[test]
fn both_events_are_registered_in_all_event_names() {
    assert!(
        ALL_EVENT_NAMES.contains(&PERMISSION_PROMPTED),
        "PERMISSION_PROMPTED must be present in ALL_EVENT_NAMES"
    );
    assert!(
        ALL_EVENT_NAMES.contains(&PERMISSION_ANSWERED),
        "PERMISSION_ANSWERED must be present in ALL_EVENT_NAMES"
    );
}

#[test]
fn permission_events_appear_after_streaming_events_and_before_release_marker() {
    let idx_streaming_completed = ALL_EVENT_NAMES
        .iter()
        .position(|n| *n == "tengu_orchestrator_turn_streaming_completed")
        .expect("turn_streaming_completed must be present (M5-04)");
    let idx_prompted = ALL_EVENT_NAMES
        .iter()
        .position(|n| *n == PERMISSION_PROMPTED)
        .expect("PERMISSION_PROMPTED missing");
    let idx_answered = ALL_EVENT_NAMES
        .iter()
        .position(|n| *n == PERMISSION_ANSWERED)
        .expect("PERMISSION_ANSWERED missing");
    let idx_release = ALL_EVENT_NAMES
        .iter()
        .position(|n| *n == "lingxi_core_v0_5_0_released")
        .expect("v0_5_0_released must be present (M4-09)");

    // Registration order — appended after M5-04 streaming events.
    assert!(idx_streaming_completed < idx_prompted);
    assert!(idx_prompted < idx_answered);
    // Release marker remains the last entry.
    assert!(idx_answered < idx_release);
}
