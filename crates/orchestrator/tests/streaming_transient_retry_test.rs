//! cc 2.1.198 mid-response transient retry (`query.ts` stream loop @219649648).
//!
//! A transient network drop (ECONNRESET / connection closed / reset) mid-stream
//! re-opens + re-pumps the SAME streaming request with backoff — but ONLY while
//! no *real* (non-thinking) content has started (binary `!Hr`). Because a
//! `tool_use` block STARTING flips that flag, this also guarantees a
//! non-idempotent tool that already started is never re-run.

use llm_runtime::{HistoryEvent, LlmError};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_thinking, content_block_start_tool_use,
    input_json_delta, message_start, message_stop, text_delta, thinking_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use protocol::ToolUseId;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

fn orch(
    streaming: Arc<MockStreamingApiClient>,
    output: Arc<MockOutputStream>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        streaming,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
}

fn end_turn(index_after: u32) -> Vec<Result<HistoryEvent, LlmError>> {
    vec![
        Ok(message_start("m2", "claude-opus-4-7")),
        Ok(content_block_start_text(index_after)),
        Ok(text_delta(index_after, "recovered answer")),
        Ok(orchestrator::test_support::content_block_stop(index_after)),
        Ok(orchestrator::test_support::message_delta_stop("end_turn")),
        Ok(message_stop()),
    ]
}

/// THINKING-only then a transient reset ⇒ retry the streaming request and
/// succeed on the second open. `stream()` is called TWICE.
#[tokio::test]
async fn thinking_only_transient_reset_retries_and_succeeds() {
    let failing: Vec<Result<HistoryEvent, LlmError>> = vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_thinking(0)),
        Ok(thinking_delta(0, "pondering")),
        Err(LlmError::Transport {
            message: "ECONNRESET: connection reset by peer".into(),
        }),
    ];
    let api = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        failing,
        end_turn(0),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = orch(api.clone(), output.clone());

    let outcome = orch.run_turn_streaming("hi").await.expect("turn ok");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );

    // The retry re-opened the stream: exactly two `stream()` calls.
    let calls = api.captured_calls().await;
    assert_eq!(
        calls.len(),
        2,
        "thinking-only transient reset must retry once (2 stream opens); calls={}",
        calls.len()
    );

    // Ends normally (end_turn), NOT model_error.
    let events = output.snapshot().await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            platform_api::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "end_turn"
        )),
        "retry must succeed with end_turn; events={events:#?}"
    );
    assert!(
        output
            .text_events()
            .await
            .iter()
            .any(|t| t.contains("recovered answer")),
        "the recovered response text must be surfaced"
    );
}

/// GUARD: a `tool_use` block that already STARTED then a transient reset must
/// NOT retry (a non-idempotent tool can never be re-run). The turn ends
/// gracefully as `model_error` and `stream()` is called exactly ONCE.
#[tokio::test]
async fn tool_started_then_transient_reset_does_not_retry() {
    let failing: Vec<Result<HistoryEvent, LlmError>> = vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_tool_use(
            0,
            ToolUseId::from("tu_1"),
            "Bash",
        )),
        Ok(input_json_delta(0, "{\"command\":\"rm -rf x\"}")),
        Err(LlmError::Transport {
            message: "ECONNRESET: connection reset by peer".into(),
        }),
    ];
    // Second turn scripted but MUST NOT be consumed (the guard forbids retry).
    let api = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        failing,
        end_turn(0),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = orch(api.clone(), output.clone());

    let outcome = orch
        .run_turn_streaming("hi")
        .await
        .expect("a mid-stream error after a tool started ends the turn gracefully");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );

    // Exactly ONE stream open — no resubmit after the tool started.
    let calls = api.captured_calls().await;
    assert_eq!(
        calls.len(),
        1,
        "a started tool must forbid the streaming retry (1 open); calls={}",
        calls.len()
    );

    let events = output.snapshot().await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            platform_api::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
        )),
        "turn must end model_error (no retry); events={events:#?}"
    );
}

/// A non-transient mid-stream error (StreamingProtocol via malformed) must not
/// retry either — only transport/idle-timeout are eligible. Here a plain
/// `ProviderInternal` still routes to the non-streaming fallback path, so the
/// guard specifically excludes it from the streaming re-open retry: the
/// transient classifier returns false for it.
#[tokio::test]
async fn thinking_only_non_transient_error_does_not_stream_retry() {
    // Malformed input JSON on a tool block → StreamingProtocol error, which is
    // NOT transient. Even though only thinking-ish content preceded, no
    // streaming re-open happens.
    let failing: Vec<Result<HistoryEvent, LlmError>> = vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_thinking(0)),
        Ok(thinking_delta(0, "pondering")),
        Err(LlmError::StreamInterrupted {
            message: "malformed frame".into(),
        }),
    ];
    let api = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        failing,
        end_turn(0),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = orch(api.clone(), output.clone());

    let _ = orch.run_turn_streaming("hi").await.expect("graceful end");
    let calls = api.captured_calls().await;
    assert_eq!(
        calls.len(),
        1,
        "a non-transient stream error must not trigger the streaming retry; calls={}",
        calls.len()
    );
}
