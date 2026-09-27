//! Smoke test: the new M3-05 modules compile and export the expected symbols.
//! First failing test of M3-05.

#[test]
fn events_module_exports_emit_fn() {
    // Compile-time assertion: the function signature is reachable.
    // (Was `cost::emit_cost_recorded`; that port-only event was dropped under
    // strict parity — the per-request success emitter is now `emit_api_success`.)
    let _: fn() = || {
        let _: &dyn std::any::Any = &cost::emit_api_success;
    };
    assert_eq!(cost::EVENT_NAME_API_SUCCESS, "tengu_api_success");
}

#[test]
fn summary_module_exports_cost_summary() {
    fn _accepts(_: &cost::CostSummary) {}
}

#[test]
fn cost_error_has_new_variants() {
    use cost::CostError;
    // The two new variants from spec §5.
    let e1 = CostError::UnknownModel {
        model: "claude-foo".to_string(),
    };
    assert_eq!(
        e1.to_string(),
        "Cost tracking unavailable for claude-foo",
        "UnknownModel Display string must match spec byte-for-byte",
    );
    let e2 = CostError::BudgetExceeded {
        limit: 100.00,
        current: 150.00,
    };
    assert_eq!(
        e2.to_string(),
        "Budget exceeded ($150.00); stopped.",
        "BudgetExceeded Display string must match spec byte-for-byte",
    );
}

#[test]
fn budget_threshold_bps_constants_are_locked() {
    assert_eq!(cost::BUDGET_WARNING_THRESHOLD_BPS, 8000_u32);
    assert_eq!(cost::BUDGET_EXCEEDED_THRESHOLD_BPS, 10000_u32);
    assert_eq!(cost::BATCH_DISCOUNT_BPS, 5000_u32);
}
