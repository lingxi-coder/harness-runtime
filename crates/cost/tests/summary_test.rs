//! Verifies `CostTracker::summary() -> CostSummary` returns aggregated session
//! totals, per-model breakdown, and labeled day/month buckets.

use cost::{CostSummary, CostTracker, ModelRef, PricingCatalog, ProviderId, TokenUsage, Usage};
use lingxi_core::types::SessionId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

#[tokio::test]
async fn summary_empty_tracker_zero_totals() {
    let (tx, _rx) = mpsc::channel(8);
    let tracker = CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    );
    let summary: CostSummary = tracker.summary().await;
    assert_eq!(summary.session.session_id, SessionId::nil());
    assert_eq!(summary.session.total_nano_usd, 0);
    assert_eq!(summary.session.total_tokens, 0);
    assert_eq!(summary.day.total_nano_usd, 0);
    assert_eq!(summary.month.total_nano_usd, 0);
    assert!(summary.by_model.is_empty());

    // Day label is "YYYY-MM-DD"; month is "YYYY-MM". Just check format.
    assert_eq!(
        summary.day.label.len(),
        10,
        "day label is YYYY-MM-DD (10 chars): {:?}",
        summary.day.label
    );
    assert_eq!(
        summary.month.label.len(),
        7,
        "month label is YYYY-MM (7 chars): {:?}",
        summary.month.label
    );
}

#[tokio::test]
async fn summary_reflects_recorded_calls() {
    let (tx, _rx) = mpsc::channel(8);
    let tracker = CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    );
    let mr = ModelRef {
        provider: ProviderId::Anthropic,
        model: "claude-opus-4-6".into(),
    };
    tracker
        .record_api_response_v2(
            mr.clone(),
            Usage {
                tokens: TokenUsage {
                    input: 1_000,
                    output: 500,
                    ..Default::default()
                },
                ..Default::default()
            },
            Duration::from_millis(200),
            0,
            128,
            64,
            false,
            None,
        )
        .await;

    let summary = tracker.summary().await;
    // 17_500_000 nano-USD = $0.0175
    assert_eq!(summary.session.total_nano_usd, 17_500_000);
    // total_tokens = sum of input + output (cache tokens are separate; not in Usage.tokens.total_tokens())
    assert_eq!(summary.session.total_tokens, 1_500); // 1000 input + 500 output

    // Per-model breakdown.
    assert_eq!(summary.by_model.len(), 1);
    let entry = summary.by_model.get(&mr).expect("model entry present");
    assert_eq!(entry.model_ref, mr);
    assert_eq!(entry.total_nano_usd, 17_500_000);
    assert_eq!(entry.input_tokens, 1_000);
    assert_eq!(entry.output_tokens, 500);
    assert_eq!(entry.cache_read_input_tokens, 128);
    assert_eq!(entry.cache_creation_input_tokens, 64);

    // Day + month stub: amounts equal session total in M3-05.
    assert_eq!(summary.day.total_nano_usd, 17_500_000);
    assert_eq!(summary.month.total_nano_usd, 17_500_000);
}

#[tokio::test]
async fn summary_two_models_breakdown_correct() {
    let (tx, _rx) = mpsc::channel(8);
    let tracker = CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    );
    let m_opus = ModelRef {
        provider: ProviderId::Anthropic,
        model: "claude-opus-4-6".into(),
    };
    let m_son = ModelRef {
        provider: ProviderId::Anthropic,
        model: "claude-sonnet-4-6".into(),
    };
    for mr in [&m_opus, &m_son] {
        tracker
            .record_api_response_v2(
                mr.clone(),
                Usage {
                    tokens: TokenUsage {
                        input: 100,
                        output: 50,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                Duration::from_millis(10),
                0,
                0,
                0,
                false,
                None,
            )
            .await;
    }
    let summary = tracker.summary().await;
    assert_eq!(summary.by_model.len(), 2, "two models tracked");
    assert!(summary.by_model.contains_key(&m_opus));
    assert!(summary.by_model.contains_key(&m_son));
}

#[test]
fn cost_summary_serializes_roundtrip() {
    // The summary shape is serializable for IPC / sidecar consumption.
    let json = serde_json::to_string(&CostSummary::default()).expect("encode");
    let _decoded: CostSummary = serde_json::from_str(&json).expect("decode");
}
