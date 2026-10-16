//! Context threshold boundary regressions.
use compaction::thresholds::{threshold_for_input_budget, warning_for_input_budget};
#[test]
fn small_context_retains_positive_working_space() {
    let threshold = threshold_for_input_budget(8192);
    assert_eq!(threshold, 7373);
    assert!(threshold < 8192);
    assert_eq!(threshold_for_input_budget(272000), 259000);
}
#[test]
fn unknown_capacity_is_not_reported_as_full_or_blocked() {
    let warning = warning_for_input_budget(300000, None);
    assert!(!warning.is_at_blocking_limit);
    assert!(!warning.is_above_auto_compact_threshold);
    let warning = warning_for_input_budget(300000, Some(272000));
    assert!(warning.is_above_auto_compact_threshold);
    assert!(
        !warning.is_at_blocking_limit,
        "an estimate cannot be an exact provider count"
    );
}
