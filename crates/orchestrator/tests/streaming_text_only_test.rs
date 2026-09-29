//!
//! Scripts a stream that emits three text deltas plus the standard
//! lifecycle events. Asserts the orchestrator's `OutputStream` receives
//! THREE `emit_text` calls in order, with the concatenated text matching
//! "hello world", and that the outcome is `EndTurn` after one turn.
//!
//! Fails to compile until Tasks 6, 8, 9, 12 land the streaming surface.

use lingxi_core::host::OutputEvent;
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

#[tokio::test]
async fn streaming_text_only_three_deltas() {
    let stream = scripted![
        message_start("msg_01", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "hel"),
        text_delta(0, "lo wor"),
        text_delta(0, "ld"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];

    let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let tools = Arc::new(ToolRegistry::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let memory = Arc::new(StaticMemoryProvider::empty());

    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        memory,
        PathBuf::from("/tmp"),
    );

    let outcome = orch
        .run_turn_streaming("say hello")
        .await
        .expect("turn must succeed");

    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => assert_eq!(turn_count, 1),
        _ => panic!("unexpected outcome variant"),
    }

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
    // Expect: Text("hel"), Text("lo wor"), Text("ld"), EndTurn { stop_reason: "end_turn" }
    assert_eq!(events.len(), 4, "got {events:?}");
    assert!(matches!(events[0], OutputEvent::Text { text } if text == "hel"));
    assert!(matches!(events[1], OutputEvent::Text { text } if text == "lo wor"));
    assert!(matches!(events[2], OutputEvent::Text { text } if text == "ld"));
    assert!(
        matches!(events[3], OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "end_turn")
    );

    // Mock observed exactly one streaming call.
    assert_eq!(api.captured_calls().await.len(), 1);
}
