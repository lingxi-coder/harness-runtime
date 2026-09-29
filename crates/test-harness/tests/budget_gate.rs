//! Integration test — `BudgetEnforcer` halts subsequent calls after the
//! cumulative session cost crosses the configured ceiling.

use cost::{BudgetConfig, BudgetEnforcer, BudgetExceedPolicy, CostTracker, PricingCatalog};
use lingxi_core::types::SessionId;
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::test]
async fn budget_halts_after_three_expensive_calls() {
    let (tx, _rx) = mpsc::channel(8);
    let tracker = Arc::new(CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let cfg = BudgetConfig {
        max_session_nano_usd: Some(30_000_000), // $0.03
        max_turn_nano_usd: None,
        max_turn_tokens: None,
        warning_thresholds: vec![0.5, 0.8],
        on_exceed: BudgetExceedPolicy::Halt,
    };
    let enforcer = BudgetEnforcer::new(cfg, tracker.clone());

    let mr = cost::ModelRef {
        provider: cost::ProviderId::Anthropic,
        model: "claude-opus-4-6".into(),
    };
    let u = cost::Usage {
        tokens: cost::TokenUsage {
            input: 2000,
            output: 0,
            ..Default::default()
        },
        ..Default::default()
    };

    // Call 1: under budget, expect Ok or ThresholdWarning (0.5 threshold may fire).
    assert!(matches!(
        enforcer.check_pre_api_call(10_000_000).await,
        cost::BudgetCheckResult::Ok | cost::BudgetCheckResult::ThresholdWarning { .. }
    ));
    tracker
        .record_api_response(mr.clone(), u, std::time::Duration::from_millis(100), 0)
        .await;

    // Call 2: still under.
    let _ = enforcer.check_pre_api_call(10_000_000).await;
    tracker
        .record_api_response(mr.clone(), u, std::time::Duration::from_millis(100), 0)
        .await;

    enforcer.check_post_api_call(10_000_000).await;

    // Call 3: now over, expect Halt.
    let result = enforcer.check_pre_api_call(15_000_000).await;
    assert!(matches!(result, cost::BudgetCheckResult::Halt { .. }));
}
