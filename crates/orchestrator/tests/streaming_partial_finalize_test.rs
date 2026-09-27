//! P1-04 (cc 2.1.199 partial-stream finalize, binary-verified against 2.1.207).
//!
//! When a mid-stream server/overloaded/api error, watchdog stall, or connection
//! close lands AFTER a real content block COMPLETED (`content_block_stop`), the
//! main session finalizes the already-streamed partial IN PLACE instead of
//! discarding it: the partial assistant message is persisted with a synthesized
//! `stop_reason` (`tool_use` if any tool_use else `end_turn`), a byte-exact
//! "API Error: … The response above may be incomplete." notice is appended,
//! `tengu_streaming_partial_finalized` fires (with the right `cause`), and the
//! `tengu_api_success` success telemetry / non-streaming fallback do NOT fire.
//!
//! A transport close after text deltas but before `content_block_stop` also
//! finalizes the text that was already shown to the user. Dropping that text
//! made mobile creation turns end with a raw reqwest error even though useful
//! output was visible on screen. Provider/server errors retain their existing
//! fallback behavior until a block completes.

use llm_runtime::{LlmError, LlmEvent};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_start, text_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use protocol::{ContentBlock, ConversationMessage};
use std::path::PathBuf;
use std::sync::Arc;
use telemetry::{AnalyticsBus, AnalyticsValue, InMemorySink};
use tool_api::registry::ToolRegistry;

/// `AnalyticsValue` has no `PartialEq`, so pull typed values out by hand.
fn meta_str(v: Option<&AnalyticsValue>) -> Option<&str> {
    match v {
        Some(AnalyticsValue::String(s)) => Some(s.as_str()),
        _ => None,
    }
}
fn meta_bool(v: Option<&AnalyticsValue>) -> Option<bool> {
    match v {
        Some(AnalyticsValue::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// Build a stream that yields one COMPLETED text block ("useful") then errors
/// mid-stream with `err`.
fn completed_block_then_error(err: LlmError) -> Vec<Result<LlmEvent, LlmError>> {
    vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "useful")),
        Ok(content_block_stop(0)),
        Err(err),
    ]
}

fn orch_with_bus(
    streaming: Arc<MockStreamingApiClient>,
    api: Arc<MockApiClient>,
    output: Arc<MockOutputStream>,
    bus: Arc<AnalyticsBus>,
) -> ConversationOrchestrator {
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            interactive_session: true,
            ..OrchestratorConfig::default()
        },
        api,
        streaming,
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output,
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    )
    .with_analytics_bus(bus)
}

/// Assert the finalize contract for a given error → expected `cause` + notice.
async fn assert_finalizes(err: LlmError, expected_cause: &str, expected_notice: &str) {
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        completed_block_then_error(err),
    ]));
    // An empty MockApiClient: if the non-streaming fallback were (wrongly)
    // invoked it would have no response to serve — but we assert seeds stay empty.
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;

    let orch = orch_with_bus(streaming.clone(), api.clone(), output.clone(), bus);

    let outcome = orch
        .run_turn_streaming("hi")
        .await
        .expect("a completed-partial finalize must end the turn gracefully");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );

    // The stream opened exactly once — no re-pump / re-fetch of the partial.
    assert_eq!(
        streaming.captured_calls().await.len(),
        1,
        "finalize must not re-open the stream"
    );
    // The non-streaming fallback was NOT invoked.
    assert!(
        api.captured_seeds().await.is_empty(),
        "finalize must NOT fall back to the non-streaming path"
    );

    // History: the partial assistant message is kept (Text "useful",
    // synthesized stop_reason end_turn), followed by the api-error notice.
    let session = orch.session();
    let guard = session.lock().await;
    let assistants: Vec<(&Vec<ContentBlock>, &Option<String>)> = guard
        .history
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::Assistant {
                content,
                stop_reason,
                ..
            } => Some((content, stop_reason)),
            _ => None,
        })
        .collect();
    assert_eq!(
        assistants.len(),
        2,
        "expected the partial assistant + the notice; got {assistants:?}"
    );
    // (1) partial assistant: the streamed "useful" text, synthesized end_turn.
    let (partial_content, partial_stop) = assistants[0];
    assert_eq!(partial_stop.as_deref(), Some("end_turn"));
    assert!(
        matches!(&partial_content[0], ContentBlock::Text { text } if text == "useful"),
        "partial assistant must keep the streamed text; got {partial_content:?}"
    );
    // (2) the incomplete-response notice, byte-exact.
    let (notice_content, _) = assistants[1];
    assert!(
        matches!(&notice_content[0], ContentBlock::Text { text } if text == expected_notice),
        "notice text must be byte-exact; got {notice_content:?}"
    );
    drop(guard);

    // The notice also reached the output stream live.
    assert!(
        output
            .text_events()
            .await
            .iter()
            .any(|t| t == expected_notice),
        "notice must be surfaced to the output stream"
    );
    // Terminal reason is `model_error`.
    assert!(
        output.snapshot().await.iter().any(|e| matches!(
            e,
            platform_api::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
        )),
        "finalize ends the turn with stop_reason model_error"
    );

    // Telemetry: tengu_streaming_partial_finalized fired with the right cause +
    // synthesized_stop_reason; tengu_api_success did NOT (this is not a success).
    let events = sink.events().await;
    let finalized = events
        .iter()
        .find(|e| e.name == "tengu_streaming_partial_finalized")
        .expect("tengu_streaming_partial_finalized must fire");
    assert_eq!(
        meta_str(finalized.metadata.get("cause")),
        Some(expected_cause),
        "cause mismatch; metadata={:?}",
        finalized.metadata
    );
    assert_eq!(
        meta_str(finalized.metadata.get("synthesized_stop_reason")),
        Some("end_turn")
    );
    assert_eq!(meta_bool(finalized.metadata.get("has_output")), Some(true));
    assert!(
        !events.iter().any(|e| e.name == "tengu_api_success"),
        "tengu_api_success must NOT fire on a partial finalize"
    );
}

/// Overloaded (529) mid-stream after a completed block → server_error finalize.
#[tokio::test]
async fn overloaded_after_completed_block_finalizes_partial() {
    assert_finalizes(
        LlmError::Overloaded { repeated: false },
        "server_error",
        "API Error: Server error mid-response. The response above may be incomplete.",
    )
    .await;
}

/// ProviderInternal (5xx) mid-stream after a completed block → server_error.
#[tokio::test]
async fn provider_internal_after_completed_block_finalizes_partial() {
    assert_finalizes(
        LlmError::ProviderInternal,
        "server_error",
        "API Error: Server error mid-response. The response above may be incomplete.",
    )
    .await;
}

/// Idle-timeout watchdog after a completed block → watchdog finalize (2.1.263
/// "The response stopped arriving").
#[tokio::test]
async fn idle_timeout_after_completed_block_finalizes_partial() {
    assert_finalizes(
        llm_runtime::model::stream_watchdog::idle_timeout_error(std::time::Duration::from_secs(1)),
        "watchdog",
        "API Error: The response stopped arriving. The response above may be incomplete.",
    )
    .await;
}

/// Machine-sleep watchdog after a completed block → stream_suspended finalize.
#[tokio::test]
async fn suspend_after_completed_block_finalizes_partial() {
    assert_finalizes(
        llm_runtime::model::stream_watchdog::watchdog_abort_error(
            std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(5),
        ),
        "stream_suspended",
        "API Error: Your computer went to sleep mid-response. The response above may be incomplete.",
    )
    .await;
}

/// Transport connection-drop mid-stream after a completed block → stale_connection.
#[tokio::test]
async fn transport_after_completed_block_finalizes_partial() {
    assert_finalizes(
        LlmError::Transport {
            message: "connection reset by peer".into(),
        },
        "stale_connection",
        "API Error: Connection lost mid-response. The response above may be incomplete.",
    )
    .await;
}

/// A stream that ends without `message_stop` after a completed block → the
/// mid-response connection close (`network_down`) finalize.
#[tokio::test]
async fn stream_ended_without_stop_after_completed_block_finalizes_partial() {
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "useful")),
        Ok(content_block_stop(0)),
        // no message_stop — the stream simply ends.
    ]]));
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let orch = orch_with_bus(streaming, api.clone(), output.clone(), bus);

    let outcome = orch.run_turn_streaming("hi").await.expect("graceful end");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    assert!(api.captured_seeds().await.is_empty());

    let expected_notice =
        "API Error: Connection lost mid-response. The response above may be incomplete.";
    assert!(output
        .text_events()
        .await
        .iter()
        .any(|t| t == expected_notice));
    let events = sink.events().await;
    let finalized = events
        .iter()
        .find(|e| e.name == "tengu_streaming_partial_finalized")
        .expect("finalize must fire");
    assert_eq!(
        meta_str(finalized.metadata.get("cause")),
        Some("network_down")
    );
}

/// A transport close after visible text but before `content_block_stop` keeps the
/// in-flight text and follows the normal partial-finalize path. The provider may
/// disappear between any two SSE frames; requiring a stop frame discarded text
/// already emitted to mobile clients and replaced it with a raw transport error.
#[tokio::test]
async fn transport_after_incomplete_text_block_finalizes_visible_partial() {
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![vec![
        Ok(message_start("m1", "claude-opus-4-7")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "partial")),
        // NO content_block_stop — the block never completed.
        Err(LlmError::Transport {
            message: "connection reset by peer".into(),
        }),
    ]]));
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let orch = orch_with_bus(streaming, api, output.clone(), bus);

    let outcome = orch.run_turn_streaming("hi").await.expect("graceful end");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));

    let session = orch.session();
    let guard = session.lock().await;
    let assistants: Vec<&Vec<ContentBlock>> = guard
        .history
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::Assistant { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    assert_eq!(assistants.len(), 2, "partial text plus interruption notice");
    assert!(matches!(
        assistants[0].first(),
        Some(ContentBlock::Text { text }) if text == "partial"
    ));
    drop(guard);

    let events = sink.events().await;
    let finalized = events
        .iter()
        .find(|event| event.name == "tengu_streaming_partial_finalized")
        .expect("visible in-flight text must be finalized");
    assert_eq!(
        meta_str(finalized.metadata.get("cause")),
        Some("stale_connection")
    );

    let expected_notice =
        "API Error: Connection lost mid-response. The response above may be incomplete.";
    assert!(
        output
            .text_events()
            .await
            .iter()
            .any(|text| text == expected_notice),
        "a stable interruption notice must replace the raw transport error"
    );
    assert!(
        !output
            .text_events()
            .await
            .iter()
            .any(|text| text.contains("connection reset by peer")),
        "raw transport details must not be rendered as assistant content"
    );
}

/// tZo (2.1.263): a non-interactive main session resumes truncated output.
#[tokio::test]
async fn noninteractive_partial_finalize_recovers_with_meta_nudge() {
    let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![
        completed_block_then_error(LlmError::ProviderInternal),
        vec![
            Ok(message_start("recovery", "claude-opus-4-7")),
            Ok(content_block_start_text(0)),
            Ok(text_delta(0, "recovered answer")),
            Ok(content_block_stop(0)),
            Ok(orchestrator::test_support::message_delta_stop("end_turn")),
            Ok(orchestrator::test_support::message_stop()),
        ],
    ]));
    let orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            interactive_session: false,
            ..OrchestratorConfig::default()
        },
        Arc::new(MockApiClient::new(Vec::new())),
        streaming.clone(),
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    assert!(matches!(
        orch.run_turn_streaming("hi").await.unwrap(),
        ConversationOutcome::EndTurn { .. }
    ));
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2);
    assert!(calls[1].messages.iter().any(|message| matches!(message,
        ConversationMessage::User { content, is_meta: true, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text }
                if text == "Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. If none of it survived, answer the request from the start."))
    )));
    assert!(calls[1].messages.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text == "useful"))
    )), "truncated output is retained during recovery");
}
