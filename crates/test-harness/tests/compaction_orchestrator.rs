//! Regression tests for compaction orchestrator.

use compaction::thresholds::AutoCompactTrackingState;
use compaction::{CompactionLayer, CompactionOrchestrator};
use lingxi_core::types::{ConversationMessage, MessageId};

#[tokio::test]
async fn over_threshold_triggers_autocompact() {
    let orch = CompactionOrchestrator::new(/* autocompact_threshold */ 100);
    let mut messages = Vec::new();
    for i in 0..50 {
        messages.push(ConversationMessage::user(
            MessageId::new(),
            format!("turn-{i} a very long padding string to inflate tokens beyond the threshold"),
        ));
    }
    let r = orch
        .process_iteration_tracked(messages, 0, &mut AutoCompactTrackingState::default(), None)
        .await
        .unwrap();
    assert!(r.layers_applied.contains(&CompactionLayer::Autocompact));
}
