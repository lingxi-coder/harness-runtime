use super::accumulate_code_change;
use cost::{CostTracker, PricingCatalog};
use lingxi_core::types::SessionId;
use serde_json::json;
use std::sync::Arc;

fn make_tracker() -> Arc<CostTracker> {
    let (tx, _rx) = tokio::sync::mpsc::channel(8);
    Arc::new(CostTracker::new(
        SessionId::nil(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ))
}

#[tokio::test]
async fn structured_patch_lines_accumulate_into_the_tracker() {
    let tracker = make_tracker();
    let payload = json!({
        "structuredPatch": [
            { "lines": ["+a", "+b", "-c"] }
        ]
    });
    accumulate_code_change(&payload, Some(&tracker)).await;
    let snap = tracker.snapshot().await;
    assert_eq!(snap.total_lines_added, 2);
    assert_eq!(snap.total_lines_removed, 1);
}

#[tokio::test]
async fn no_tracker_is_a_no_op() {
    // Absent tracker (M6-06 default `None`) must not panic — this is the
    // common case whenever no host has opted into cost tracking.
    let payload = json!({
        "structuredPatch": [ { "lines": ["+a"] } ]
    });
    accumulate_code_change(&payload, None).await;
}

#[tokio::test]
async fn missing_structured_patch_is_a_no_op() {
    let tracker = make_tracker();
    // Every non-edit tool's result data lacks `structuredPatch` entirely.
    accumulate_code_change(&json!({"stdout": "ok"}), Some(&tracker)).await;
    let snap = tracker.snapshot().await;
    assert_eq!(snap.total_lines_added, 0);
    assert_eq!(snap.total_lines_removed, 0);
}
