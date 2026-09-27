//! Smoke test: M3-06 module skeleton + `TelemetryError` + `OverflowPolicy` exist.
//! First failing test of M3-06.

#[test]
fn telemetry_error_has_spec_variants() {
    use telemetry::TelemetryError;
    let e1 = TelemetryError::UnknownEvent {
        name: "tengu_unknown".to_string(),
    };
    assert_eq!(
        e1.to_string(),
        "unknown event name tengu_unknown (not in tengu_* schema)",
        "UnknownEvent Display string must match spec §5 byte-for-byte",
    );
    let e2 = TelemetryError::PayloadInvalid {
        event: "tengu_api_request_succeeded".to_string(),
        detail: "missing model field".to_string(),
    };
    assert_eq!(
        e2.to_string(),
        "event payload validation failed for tengu_api_request_succeeded: missing model field",
        "PayloadInvalid Display string must match spec §5 byte-for-byte",
    );
}

#[test]
fn overflow_policy_default_is_drop_oldest() {
    use telemetry::{AnalyticsBus, OverflowPolicy};
    let bus = AnalyticsBus::new();
    assert!(matches!(bus.overflow_policy(), OverflowPolicy::DropOldest));
}

#[test]
fn tengu_module_compiles() {
    // Reach for the registry constant; subsequent tasks populate it.
    let all: &[&'static str] = telemetry::tengu::ALL_EVENT_NAMES;
    // After Task 1 the registry is empty; once Tasks 2-9 land it grows monotonically.
    // We don't assert an exact length here so this skeleton doesn't churn per task.
    // M4-09 added `lingxi_core_v0_5_0_released` (release marker — not a
    // subsystem event, so it carries the `lingxi_core_` prefix instead of
    // `tengu_`). Allow either.
    assert!(all
        .iter()
        .all(|n| n.starts_with("tengu_") || n.starts_with("lingxi_core_")));
}

#[test]
fn sinks_module_compiles() {
    // Module exists but is empty; just confirm the path resolves.
    let _: fn() = || {
        let _ = telemetry::sinks::_module_marker();
    };
}
