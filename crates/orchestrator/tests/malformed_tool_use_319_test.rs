//! Native 2.1.319 retry, terminal-error and partial-stream contracts.

use lingxi_core::host::OutputEvent;
use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, noop_hook_executor, text_delta, MockApiClient,
    MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

const NUDGE: &str =
    "The previous response failed to produce a valid tool call. Please retry the tool call now.";

fn text_turn(index: usize, stop_reason: &str) -> Vec<llm_runtime::HistoryEvent> {
    scripted![
        message_start(&format!("malformed-{index}"), "claude-sonnet-4-6"),
        content_block_start_text(0),
        text_delta(0, "I will call the tool"),
        content_block_stop(0),
        message_delta_stop(stop_reason),
        message_stop(),
    ]
}

fn build(
    turns: Vec<Vec<llm_runtime::HistoryEvent>>,
) -> (
    Arc<MockStreamingApiClient>,
    Arc<MockOutputStream>,
    Arc<ConversationOrchestrator>,
) {
    let api = Arc::new(MockStreamingApiClient::with_turns(turns));
    let output = Arc::new(MockOutputStream::new().with_partial_stream_events());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    ));
    (api, output, orch)
}

fn count_nudges(history: &[ConversationMessage]) -> usize {
    history.iter().filter(|row| matches!(row,
        ConversationMessage::User { content, is_meta: true, turn_companion: Some(Some(true)), .. }
        if matches!(content.first(), Some(ContentBlock::Text { text, .. }) if text == NUDGE)
    )).count()
}

#[tokio::test]
async fn streaming_retries_twice_then_reports_native_malformed_terminal() {
    let (api, output, orch) = build((0..3).map(|i| text_turn(i, "tool_use")).collect());
    orch.run_turn_streaming("call a tool").await.unwrap();
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 3);
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(count_nudges(&call.messages), index);
        assert!(
            !call
                .messages
                .iter()
                .any(|row| matches!(row, ConversationMessage::Assistant { .. })),
            "retracted malformed responses must not enter the retry request"
        );
    }
    let events = output.snapshot().await;
    let metadata = output.partial_stream_metadata_snapshot().await;
    assert_eq!(metadata.len(), 18);
    for (index, frame) in metadata.iter().enumerate() {
        assert_eq!(
            frame
                .api_message_id
                .as_ref()
                .and_then(|id| id.string_at("")),
            Some(format!("malformed-{}", index / 6).as_str())
        );
        assert!(
            frame.abandoned_blocks.is_none(),
            "complete response has no abandoned blocks"
        );
    }
    assert!(events.iter().any(|event| matches!(event,
        OutputEvent::TerminalApiError { terminal_reason, stop_reason, .. }
        if terminal_reason == "malformed_tool_use_exhausted" && stop_reason == "stop_sequence"
    )));
    assert!(events.iter().any(|event| matches!(event,
        OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "malformed_tool_use_exhausted"
    )));
}

#[tokio::test]
async fn streaming_output_limit_retry_preserves_malformed_allowance() {
    let (api, output, orch) = build(vec![
        text_turn(0, "tool_use"),
        text_turn(1, "max_tokens"),
        text_turn(2, "tool_use"),
        text_turn(3, "tool_use"),
    ]);
    orch.run_turn_streaming("call a tool").await.unwrap();
    assert_eq!(api.captured_calls().await.len(), 4);
    assert!(output.snapshot().await.iter().any(|event| matches!(event,
        OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "malformed_tool_use_exhausted"
    )));
}

#[tokio::test]
async fn streaming_normal_tool_round_replenishes_malformed_allowance() {
    let tool_round = scripted![
        message_start("valid-tool", "claude-sonnet-4-6"),
        content_block_start_tool_use(0, ToolUseId::from("tool-1"), "MissingTool"),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let (api, output, orch) = build(vec![
        text_turn(0, "tool_use"),
        text_turn(1, "tool_use"),
        tool_round,
        text_turn(3, "tool_use"),
        text_turn(4, "tool_use"),
        text_turn(5, "end_turn"),
    ]);
    orch.run_turn_streaming("call a tool").await.unwrap();
    assert_eq!(api.captured_calls().await.len(), 6);
    assert!(output.snapshot().await.iter().any(|event| matches!(event,
        OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "end_turn"
    )));
}

#[tokio::test]
async fn authoritative_native_frames_replace_reconstruction_without_changing_content() {
    use llm_runtime::{HistoryContentDelta, HistoryEvent};
    let sources = [
        r#"{"type":"message_start","message":{"id":"native-exact","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":100,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}},"api_message_id":"forged","thinking_display":"forged"}"#,
        r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
        r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"A\ud800B"}}"#,
        r#"{"type":"content_block_stop","index":0}"#,
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
        r#"{"type":"message_stop"}"#,
    ];
    let semantic = vec![
        message_start("native-exact", "claude-sonnet-4-6"),
        content_block_start_text(0),
        HistoryEvent::ContentBlockDelta {
            index: 0,
            delta: HistoryContentDelta::TextJsUtf16Delta {
                text: "A�B".into(),
                utf16_code_units: vec![65, 0xd800, 66],
            },
        },
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let mut turn = Vec::new();
    for (source, event) in sources.iter().zip(semantic) {
        turn.push(HistoryEvent::NativeStreamFrame {
            event: lingxi_llm_client::protocol::JsonPayload::parse(source).unwrap(),
            thinking_display: Some("updates".into()),
        });
        turn.push(event);
    }
    let (_, output, orch) = build(vec![turn]);
    orch.run_turn_streaming("answer").await.unwrap();
    assert_eq!(output.partial_stream_event_snapshot().await, sources);
    let metadata = output.partial_stream_metadata_snapshot().await;
    assert!(metadata.iter().all(|frame| frame
        .api_message_id
        .as_ref()
        .and_then(|id| id.string_at(""))
        == Some("native-exact")));
    assert_eq!(metadata[0].thinking_display.as_deref(), Some("updates"));
    assert!(metadata[1..]
        .iter()
        .all(|frame| frame.thinking_display.is_none()));
    assert!(output.snapshot().await.iter().any(|event| matches!(event,
        OutputEvent::Text { text } if text.string_units("") == Some(vec![65, 0xd800, 66])
    )));
}
