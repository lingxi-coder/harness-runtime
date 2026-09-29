//!
//! Mirrors the batched recovery tests in `turn_loop.rs`'s
//! `max_output_tokens_recovery_tests` against the STREAMING driver
//! (`try_run_turn_streaming`). The streaming path intercepts `max_tokens` in
//! its disposition arm (before the generic terminal): while the recovery count
//! is below the limit it appends the byte-exact meta nudge and continues; on
//! exhaustion it ends the turn with `stop_reason = "max_tokens"`.
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use platform_api::OutputEvent;
use protocol::{ContentBlock, ConversationMessage};
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

/// The byte-exact nudge string (TS `query.ts:1226-1227`), duplicated here as a
/// black-box expectation so the integration test does not depend on a crate
/// internal const.
const NUDGE: &str = "Output token limit hit. Resume directly \u{2014} no apology, no recap of what you were doing. Pick up mid-thought if that is where the cut happened. Break remaining work into smaller pieces.";

/// One streaming turn that ends with the given `stop_reason`, emitting one
/// short text block.
fn turn_ending_with(idx: u32, stop_reason: &str) -> Vec<llm_runtime::HistoryEvent> {
    scripted![
        message_start(&format!("m{idx}"), "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "partial"),
        content_block_stop(0),
        message_delta_stop(stop_reason),
        message_stop(),
    ]
}

fn build_orch(
    turns: Vec<Vec<llm_runtime::HistoryEvent>>,
) -> (
    Arc<MockStreamingApiClient>,
    Arc<MockOutputStream>,
    ConversationOrchestrator,
) {
    let api = Arc::new(MockStreamingApiClient::with_turns(turns));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    (api, output, orch)
}

/// Count how many of a captured history snapshot are the meta nudge. The nudge
/// MUST be an `is_meta` user message (parity 2.1.207 P2-05: CC builds the
/// max-output-tokens recovery nudge via `createUserMessage({…, isMeta:!0})`), so
/// this filter doubles as the streaming-driver regression guard — before the
/// flip the injected nudge was non-meta and this would count 0.
fn count_nudges(msgs: &[ConversationMessage]) -> usize {
    msgs.iter()
        .filter(|m| {
            matches!(m, ConversationMessage::User { content, is_meta: true, .. }
                if matches!(content.first(), Some(ContentBlock::Text { text }) if text == NUDGE))
        })
        .count()
}

#[tokio::test]
async fn streaming_max_tokens_then_end_turn_injects_one_nudge() {
    // Turn 1: max_tokens (→ nudge + continue). Turn 2: end_turn.
    let (api, output, orch) = build_orch(vec![
        turn_ending_with(1, "max_tokens"),
        turn_ending_with(2, "end_turn"),
    ]);

    let outcome = orch.run_turn_streaming("write a lot").await.expect("ok");
    match outcome {
        ConversationOutcome::EndTurn { turn_count, .. } => assert_eq!(turn_count, 2),
        _ => panic!("unexpected outcome"),
    }

    // The mock captured each turn's history snapshot. Turn 2's snapshot must
    // already contain exactly one nudge (injected after turn 1's max_tokens).
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 2, "two streaming turns");
    assert_eq!(
        count_nudges(&calls[0].messages),
        0,
        "turn 1 sees no nudge yet"
    );
    assert_eq!(count_nudges(&calls[1].messages), 1, "turn 2 sees the nudge");

    // The final emitted EndTurn carries `end_turn`, NOT `max_tokens`.
    let events = output.snapshot().await;
    let end = events
        .iter()
        .rev()
        .find_map(|e| match e {
            OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("an EndTurn event");
    assert_eq!(end, "end_turn");
}

#[tokio::test]
async fn streaming_four_consecutive_max_tokens_exhausts_recovery() {
    // 4 consecutive max_tokens turns: turns 1-3 nudge+continue (count 1,2,3);
    // turn 4 (count == limit) ends with stop_reason `max_tokens`.
    let (api, output, orch) = build_orch(vec![
        turn_ending_with(1, "max_tokens"),
        turn_ending_with(2, "max_tokens"),
        turn_ending_with(3, "max_tokens"),
        turn_ending_with(4, "max_tokens"),
    ]);

    let outcome = orch.run_turn_streaming("write forever").await.expect("ok");
    let final_message_id = match outcome {
        ConversationOutcome::EndTurn {
            turn_count,
            final_message_id,
        } => {
            assert_eq!(turn_count, 4);
            final_message_id
        }
        _ => panic!("unexpected outcome"),
    };

    // Exactly 3 nudges were injected across the 4 turns (one after each of the
    // first three max_tokens; the fourth exhausts and ends the turn).
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 4);
    assert_eq!(count_nudges(&calls[0].messages), 0);
    assert_eq!(count_nudges(&calls[1].messages), 1);
    assert_eq!(count_nudges(&calls[2].messages), 2);
    assert_eq!(count_nudges(&calls[3].messages), 3);

    // The terminal EndTurn carries `max_tokens` (the surfaced cap).
    let events = output.snapshot().await;
    let end = events
        .iter()
        .rev()
        .find_map(|e| match e {
            OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .expect("an EndTurn event");
    assert_eq!(end, "max_tokens");

    let history = orch.session().lock().await.history.clone();
    let last_id = match history.last() {
        Some(ConversationMessage::Assistant { id, .. }) => *id,
        other => panic!("expected final assistant message, got {other:?}"),
    };
    assert_eq!(
        final_message_id, last_id,
        "final_message_id must point at the surfaced terminal max_tokens assistant message"
    );
}
