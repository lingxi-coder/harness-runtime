use compaction::{CompactionLayer, CompactionOrchestrator};
use protocol::{ContentBlock, ConversationMessage, MessageId};

#[tokio::test]
async fn over_threshold_triggers_autocompact() {
    let orch = CompactionOrchestrator::new(/* autocompact_threshold */ 100);
    let mut messages = Vec::new();
    for i in 0..50 {
        messages.push(ConversationMessage::User {
            id: MessageId::new(),
            content: vec![ContentBlock::Text {
                text: format!(
                    "turn-{i} a very long padding string to inflate tokens beyond the threshold"
                ),
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        });
    }
    let r = orch.process_iteration(messages, 0).await.unwrap();
    assert!(r.layers_applied.contains(&CompactionLayer::Autocompact));
}
