//! End-to-end cost pipeline: 3 simulated API responses trip the budget gate,
//! firing `tengu_cost_budget_warning` + `tengu_cost_budget_exceeded` in the
//! locked order.
//!
//! Strict-parity note: the per-call `tengu_cost_recorded` event was PORT-ONLY
//! (0 hits in claude-code 2.1.195) and was dropped — the `CostTracker` no
//! longer emits a per-call telemetry event (the per-request success event,
//! `tengu_api_success`, is fired by the orchestrator, not the tracker). So the
//! tracker pipeline now produces ONLY the two budget-threshold alarms.

use async_trait::async_trait;
use cost::{
    BudgetConfig, BudgetEnforcer, BudgetExceedPolicy, CostTracker, ModelRef, PricingCatalog,
    ProviderId, TokenUsage, Usage,
};
use lingxi_core::types::SessionId;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use telemetry::{AnalyticsBus, AnalyticsSink, LogEventMetadata};
use tokio::sync::mpsc;

#[derive(Default)]
struct CaptureSink {
    events: Mutex<Vec<(String, LogEventMetadata)>>,
}

#[async_trait]
impl AnalyticsSink for CaptureSink {
    async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
        self.events.lock().unwrap().push((name.into(), metadata));
    }
    async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
        self.events.lock().unwrap().push((name.into(), metadata));
    }
    fn name(&self) -> &str {
        "capture"
    }
}

#[tokio::test]
async fn three_calls_trigger_warning_then_exceeded() {
    // $1.00 limit. Each call is 100_000 tokens * 5_000 = 500_000_000 nano-USD = $0.50.
    // 3 calls → 1.5 limit; warning fires after call 2 (50% → 100% crosses 80%);
    // exceeded fires after call 2 (since 2 calls = $1.00 exactly = 100%).
    //
    // Adjust: use 80_000 tokens per call ($0.40 each).
    //   After call 1: $0.40 (40%)              — no alarm
    //   After call 2: $0.80 (80%)              — warning fires
    //   After call 3: $1.20 (120%, saturates)  — exceeded fires

    let (tx, _rx) = mpsc::channel(64);
    let tracker = Arc::new(CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let cfg = BudgetConfig {
        max_session_nano_usd: Some(1_000_000_000), // $1.00
        max_turn_nano_usd: None,
        max_turn_tokens: None,
        warning_thresholds: vec![], // M3-05 BPS path only
        on_exceed: BudgetExceedPolicy::Halt,
    };
    let enforcer = BudgetEnforcer::new(cfg, tracker.clone());

    let bus = Arc::new(AnalyticsBus::new());
    let sink: Arc<CaptureSink> = Arc::new(CaptureSink::default());
    bus.attach_sink(sink.clone() as Arc<dyn AnalyticsSink>)
        .await;

    let mr = ModelRef {
        provider: ProviderId::Anthropic,
        model: "claude-opus-4-6".into(),
    };

    for _ in 0..3 {
        // Record one call.
        let cost = tracker
            .record_api_response_v2(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 80_000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(50),
                0,
                0,
                0,
                false,
                Some(&bus),
            )
            .await;
        assert_eq!(cost, 400_000_000, "$0.40 per call");
        // Then the post-call budget gate.
        enforcer
            .check_post_api_call_with_bus(cost, Some(&bus))
            .await;
    }

    let events = sink.events.lock().unwrap();
    let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();

    // The tracker emits NO per-call event now (tengu_cost_recorded dropped);
    // only the two budget alarms fire. 1 budget_warning + 1 budget_exceeded = 2.
    assert_eq!(events.len(), 2, "expected 2 events; got {names:?}");

    // No per-call cost event is emitted by the tracker any more.
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "tengu_cost_recorded")
            .count(),
        0,
        "tengu_cost_recorded was dropped under strict parity"
    );
    // tengu_cost_budget_warning fires exactly once.
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "tengu_cost_budget_warning")
            .count(),
        1,
        "one 80% warning"
    );
    // tengu_cost_budget_exceeded fires exactly once.
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "tengu_cost_budget_exceeded")
            .count(),
        1,
        "one 100% exceeded"
    );

    // Ordering: warning (after call 2, 80%) precedes exceeded (after call 3, 120%).
    assert_eq!(names[0], "tengu_cost_budget_warning", "call 2 warning");
    assert_eq!(names[1], "tengu_cost_budget_exceeded", "call 3 exceeded");
}

#[tokio::test]
async fn summary_after_pipeline_reflects_all_three_calls() {
    let (tx, _rx) = mpsc::channel(64);
    let tracker = Arc::new(CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let mr = ModelRef {
        provider: ProviderId::Anthropic,
        model: "claude-opus-4-6".into(),
    };
    for _ in 0..3 {
        tracker
            .record_api_response_v2(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 80_000,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(50),
                0,
                0,
                0,
                false,
                None,
            )
            .await;
    }
    let summary = tracker.summary().await;
    assert_eq!(
        summary.session.total_nano_usd, 1_200_000_000,
        "$1.20 cumulative"
    );
    assert_eq!(summary.by_model.len(), 1);
}
