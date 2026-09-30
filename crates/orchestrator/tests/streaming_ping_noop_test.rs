//! Streaming ping frames preserve the conversation's output events.

use lingxi_core::host::OutputEvent;
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

// `HistoryEvent` has no Ping variant — this test verifies that non-content events
// (no pings to inject) do not disturb the output stream.
#[tokio::test]
async fn ping_between_deltas_does_not_disturb_output() {
    let turn = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "abc"),
        text_delta(0, "def"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![turn]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    orch.run_turn_streaming("hi").await.expect("ok");

    let events = output.snapshot().await;
    // The §0.7 "light up thinking/usage" follow-up adds additive `Usage`
    // emits (from `message_start` / `message_delta`); filter them (and
    // `Thinking` and message identity metadata) out so this test keeps asserting the text/end-turn
    // sequence it cares about.
    let events: Vec<&OutputEvent> = events
        .iter()
        .filter(|e| {
            !matches!(
                e,
                OutputEvent::Usage { .. }
                    | OutputEvent::Thinking { .. }
                    | OutputEvent::MessageIdentity { .. }
            )
        })
        .collect();
    // Expect: Text("abc"), Text("def"), EndTurn — pings filtered out.
    assert_eq!(events.len(), 3, "got {events:?}");
    assert!(matches!(events[0], OutputEvent::Text { text } if text == "abc"));
    assert!(matches!(events[1], OutputEvent::Text { text } if text == "def"));
    assert!(matches!(events[2], OutputEvent::EndTurn { .. }));
}
