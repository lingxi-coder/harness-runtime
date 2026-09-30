//! Regression tests for cost schema test.

use telemetry::tengu::cost;

#[test]
fn all_9_cost_event_names_are_locked() {
    // Strict-parity: the former 10th name `tengu_cost_recorded` was PORT-ONLY
    // (0 hits in claude-code 2.1.195) and was dropped; per-request success
    // telemetry is now `tengu_api_success` (cost::EVENT_NAME_API_SUCCESS).
    let names: &[&str] = &[
        cost::BUDGET_WARNING,
        cost::BUDGET_EXCEEDED,
        cost::SUMMARY_REQUESTED,
        cost::SUMMARY_GENERATED,
        cost::UNKNOWN_MODEL,
        cost::BATCH_DISCOUNT_APPLIED,
        cost::PRICING_TABLE_REFRESHED,
        cost::SESSION_TOTAL_UPDATED,
        cost::PERSISTENCE_FAILED,
    ];
    assert_eq!(names.len(), 9);
    for n in names {
        assert!(n.starts_with("tengu_cost_"));
    }
    // M3-05 plan locks these two byte-for-byte.
    assert_eq!(cost::BUDGET_WARNING, "tengu_cost_budget_warning");
    assert_eq!(cost::BUDGET_EXCEEDED, "tengu_cost_budget_exceeded");
}
