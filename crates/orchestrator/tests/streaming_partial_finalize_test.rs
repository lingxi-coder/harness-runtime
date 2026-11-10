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

use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::{HistoryEvent, LlmError};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_start, noop_hook_executor, text_delta,
    MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
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
fn completed_block_then_error(err: LlmError) -> Vec<Result<HistoryEvent, LlmError>> {
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
) -> Arc<ConversationOrchestrator> {
    ConversationOrchestrator::into_shared(
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
        .with_analytics_bus(bus),
    )
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
        matches!(&partial_content[0], ContentBlock::Text { text, .. } if text == "useful"),
        "partial assistant must keep the streamed text; got {partial_content:?}"
    );
    // (2) the incomplete-response notice, byte-exact.
    let (notice_content, _) = assistants[1];
    assert!(
        matches!(&notice_content[0], ContentBlock::Text { text, .. } if text == expected_notice),
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
            lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
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
        Some(ContentBlock::Text { text, .. }) if text == "partial"
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
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new_with_streaming(
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
    ));
    assert!(matches!(
        orch.run_turn_streaming("hi").await.unwrap(),
        ConversationOutcome::EndTurn { .. }
    ));
    let calls = streaming.captured_calls().await;
    assert_eq!(calls.len(), 2);
    assert!(calls[1].messages.iter().any(|message| matches!(message,
        ConversationMessage::User { content, is_meta: true, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. }
                if text == "Your response above was cut off mid-stream. Resume directly from where it stops — no apology, no recap. If none of it survived, answer the request from the start."))
    )));
    assert!(calls[1].messages.iter().any(|message| matches!(message,
        ConversationMessage::Assistant { content, .. }
            if content.iter().any(|block| matches!(block, ContentBlock::Text { text, .. } if text == "useful"))
    )), "truncated output is retained during recovery");
}

#[tokio::test]
async fn transport_and_eof_after_completed_row_preserve_open_tail_in_history_and_jsonl() {
    for error in [
        Some(LlmError::Transport {
            message: "connection reset by peer".into(),
        }),
        None,
    ] {
        let mut events = vec![
            Ok(message_start("tail", "claude-opus-4-7")),
            Ok(content_block_start_text(0)),
            Ok(text_delta(0, "completed prefix")),
            Ok(content_block_stop(0)),
            Ok(content_block_start_text(1)),
            Ok(text_delta(1, "visible open tail")),
        ];
        if let Some(error) = error {
            events.push(Err(error));
        }
        let streaming = Arc::new(MockStreamingApiClient::with_fallible_turns(vec![events]));
        let output = Arc::new(MockOutputStream::new());
        let scratch = tempfile::tempdir().unwrap();
        let transcript = scratch.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            transcript.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                scratch.path().to_path_buf(),
            )),
        ));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    interactive_session: true,
                    ..Default::default()
                },
                Arc::new(MockApiClient::new(vec![])),
                streaming,
                Arc::new(ToolRegistry::new()),
                orchestrator::test_support::noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_jsonl_writer(writer),
        );
        orch.run_turn_streaming("answer the request").await.unwrap();
        assert_eq!(
            output
                .text_events()
                .await
                .iter()
                .filter(|text| *text == "visible open tail")
                .count(),
            1
        );
        let session = orch.session();
        let history = session.lock().await;
        let content = history.history.iter().find_map(|message| match message {
            ConversationMessage::Assistant { content, .. } if content.iter().any(|block|
                matches!(block, ContentBlock::Text { text, .. } if text == "completed prefix")) => Some(content),
            _ => None,
        }).expect("completed prefix remains in canonical history");
        assert_eq!(
            content
                .iter()
                .filter_map(ContentBlock::visible_text)
                .collect::<Vec<_>>(),
            vec!["completed prefix", "visible open tail"]
        );
        drop(history);
        let persisted = std::fs::read_to_string(transcript).unwrap();
        let assistant_text: Vec<_> = persisted
            .lines()
            .filter_map(|line| {
                let row: serde_json::Value = serde_json::from_str(line).unwrap();
                (row["type"] == "assistant")
                    .then(|| {
                        row["message"]["content"][0]["text"]
                            .as_str()
                            .map(str::to_owned)
                    })
                    .flatten()
            })
            .collect();
        assert_eq!(
            &assistant_text[..2],
            &["completed prefix", "visible open tail"]
        );
        assert_eq!(
            assistant_text
                .iter()
                .filter(|text| *text == "visible open tail")
                .count(),
            1
        );
    }
}

struct CompletedCountingTool(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl tool_api::Tool for CompletedCountingTool {
    fn name(&self) -> &str {
        "CompletedCounter"
    }
    fn input_schema(&self) -> &lingxi_core::types::utf16_json::Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<lingxi_core::types::utf16_json::Utf16JsonProjection> =
            std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"type":"object","properties":{}}"#,
            )
            .unwrap()
        })
    }
    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        true
    }
    fn is_read_only(&self, _: &lingxi_core::types::utf16_json::Utf16JsonProjection) -> bool {
        false
    }
    async fn description(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        "count".into()
    }
    async fn prompt(
        &self,
        _: &tool_api::tool_trait::PromptOptions,
    ) -> lingxi_core::types::utf16_json::Utf16JsonProjection {
        "count".into()
    }
    async fn check_permissions(
        &self,
        _: &lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn call(
        &self,
        _: lingxi_core::types::utf16_json::Utf16JsonProjection,
        _: tool_api::context::ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::tool_trait::ToolCallResult, tool_api::tool_trait::ToolError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(tool_api::tool_trait::ToolCallResult {
            data: lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                r#"{"content":"counted"}"#,
            )
            .unwrap(),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            subagent_hooks_fired: false,
            mcp_meta: None,
        })
    }
}

#[tokio::test]
async fn completed_payload_dispatches_missing_tool_once_and_retains_accepted_tool_once() {
    use lingxi_core::types::ToolUseId;
    use orchestrator::test_support::{
        content_block_start_tool_use, input_json_delta, message_delta_stop, message_stop,
    };
    for incremental in [false, true] {
        let HistoryEvent::MessageStart { mut response } =
            message_start("completed", "claude-opus-4-7")
        else {
            unreachable!()
        };
        response.content = vec![llm_runtime::ContentBlock::ToolCall {
            id: "counter-call".into(),
            name: "CompletedCounter".into(),
            input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
        }];
        response.stop_reason = Some("tool_use".into());
        let mut first = vec![];
        if incremental {
            first.extend([
                message_start("completed", "claude-opus-4-7"),
                content_block_start_tool_use(
                    0,
                    ToolUseId::from("counter-call"),
                    "CompletedCounter",
                ),
                input_json_delta(0, "{}"),
                content_block_stop(0),
            ]);
        }
        first.push(HistoryEvent::Completed { response });
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
            first,
            vec![
                message_start("answer", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
        ]));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CompletedCountingTool(calls.clone())));
        let scratch = tempfile::tempdir().unwrap();
        let transcript = scratch.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            transcript.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                scratch.path().to_path_buf(),
            )),
        ));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                streaming.clone(),
                Arc::new(registry),
                orchestrator::test_support::noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                PathBuf::from("/tmp"),
            )
            .with_jsonl_writer(writer),
        );
        orch.run_turn_streaming("count once").await.unwrap();
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "incremental={incremental}"
        );
        let requests = streaming.captured_calls().await;
        assert_eq!(requests.len(), 2);
        let uses = requests[1].messages.iter().flat_map(|message| message.tool_calls()).filter(|block|
            matches!(block, ContentBlock::ToolUse { id, .. } if id.to_string() == "counter-call")).count();
        assert_eq!(
            uses, 1,
            "canonical continuation must carry one matching ToolUse"
        );
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(transcript)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let tools: Vec<_> = rows
            .iter()
            .filter(|row| {
                row["type"] == "assistant" && row["message"]["content"][0]["type"] == "tool_use"
            })
            .collect();
        assert_eq!(tools.len(), 1);
        let results: Vec<_> = rows
            .iter()
            .filter(|row| {
                row["type"] == "user" && row["message"]["content"][0]["type"] == "tool_result"
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["parentUuid"], tools[0]["uuid"]);
    }
}

#[tokio::test]
async fn fragmented_thinking_signature_survives_tool_continuation_and_jsonl_replay() {
    use orchestrator::test_support::{
        content_block_start_thinking, content_block_start_tool_use, input_json_delta,
        message_delta_stop, message_stop, thinking_delta,
    };
    let signature_fragment = |fragment: &str| HistoryEvent::ContentBlockDelta {
        index: 0,
        delta: llm_runtime::HistoryContentDelta::SignatureDelta {
            signature: fragment.into(),
        },
    };
    let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
        vec![
            message_start("signed", "claude-opus-4-7"),
            content_block_start_thinking(0),
            thinking_delta(0, "signed reasoning"),
            signature_fragment("first-"),
            signature_fragment("middle-"),
            signature_fragment("last"),
            content_block_stop(0),
            content_block_start_tool_use(1, "signature-call".into(), "CompletedCounter"),
            input_json_delta(1, "{}"),
            content_block_stop(1),
            message_delta_stop("tool_use"),
            message_stop(),
        ],
        vec![
            message_start("answer", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ],
    ]));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(CompletedCountingTool(calls.clone())));
    let scratch = tempfile::tempdir().unwrap();
    let transcript = scratch.path().join("session.jsonl");
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        transcript.clone(),
        Arc::new(platform_posix::fs::PosixFileSystem::new(
            scratch.path().to_path_buf(),
        )),
    ));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            streaming.clone(),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            scratch.path().to_path_buf(),
        )
        .with_jsonl_writer(writer),
    );
    orch.run_turn_streaming("continue after the tool")
        .await
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    let requests = streaming.captured_calls().await;
    assert_eq!(requests.len(), 2);
    let messages = llm_runtime::convert::normalize_messages_for_api(requests[1].messages.clone());
    let replay = llm_runtime::convert::to_llm_messages(messages)
        .expect("production-normalized request must convert");
    assert!(replay.iter().flat_map(|message| &message.content).any(|block| matches!(block,
        llm_runtime::ContentBlock::Reasoning { text, signature }
            if text == "signed reasoning" && signature.as_deref() == Some("first-middle-last")
    )), "the next provider request must replay every exact signature fragment");
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(transcript)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(
        rows.iter()
            .filter(|row| row["type"] == "assistant")
            .any(|row| row["message"]["content"]
                .as_array()
                .is_some_and(
                    |blocks| blocks.iter().any(|block| block["type"] == "thinking"
                        && block["thinking"] == "signed reasoning"
                        && block["signature"] == "first-middle-last")
                )),
        "cold replay must retain the same signed thinking bytes"
    );
}

#[tokio::test]
async fn initial_thinking_snapshot_survives_live_output_continuation_and_jsonl() {
    use orchestrator::test_support::{
        content_block_start_tool_use, input_json_delta, message_delta_stop, message_stop,
        mock_message_response, thinking_delta,
    };
    // A public StreamingApiClient may supply a complete start snapshot, a
    // snapshot followed by fragments, or only its authoritative final response.
    for mode in ["initial_only", "initial_and_delta", "completed_only"] {
        let thinking = "initial reasoning";
        let signature = "initial-signature";
        let mut first = if mode == "completed_only" {
            vec![HistoryEvent::Completed {
                response: Box::new(mock_message_response(
                    vec![
                        llm_runtime::ContentBlock::Reasoning {
                            text: thinking.into(),
                            signature: Some(signature.into()),
                        },
                        llm_runtime::ContentBlock::ToolCall {
                            id: "initial-call".into(),
                            name: "CompletedCounter".into(),
                            input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
                        },
                    ],
                    Some("tool_use"),
                )),
            }]
        } else {
            vec![
                message_start("initial", "claude-opus-4-7"),
                HistoryEvent::ContentBlockStart {
                    index: 0,
                    content_block: llm_runtime::ContentBlock::Reasoning {
                        text: if mode == "initial_and_delta" {
                            "initial "
                        } else {
                            thinking
                        }
                        .into(),
                        signature: Some(
                            if mode == "initial_and_delta" {
                                "initial-"
                            } else {
                                signature
                            }
                            .into(),
                        ),
                    },
                },
            ]
        };
        if mode == "initial_and_delta" {
            first.push(thinking_delta(0, "reasoning"));
            first.push(HistoryEvent::ContentBlockDelta {
                index: 0,
                delta: llm_runtime::HistoryContentDelta::SignatureDelta {
                    signature: "signature".into(),
                },
            });
        }
        if mode != "completed_only" {
            first.extend([
                content_block_stop(0),
                content_block_start_tool_use(1, "initial-call".into(), "CompletedCounter"),
                input_json_delta(1, "{}"),
                content_block_stop(1),
                message_delta_stop("tool_use"),
                message_stop(),
            ]);
        }
        let streaming = Arc::new(MockStreamingApiClient::with_turns(vec![
            first,
            vec![
                message_start("answer", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ],
        ]));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CompletedCountingTool(calls.clone())));
        let scratch = tempfile::tempdir().unwrap();
        let transcript = scratch.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
            transcript.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                scratch.path().to_path_buf(),
            )),
        ));
        let output = Arc::new(MockOutputStream::new().with_partial_stream_events());
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                streaming.clone(),
                Arc::new(registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                scratch.path().to_path_buf(),
            )
            .with_jsonl_writer(writer),
        );
        orch.run_turn_streaming("continue after the tool")
            .await
            .unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1, "{mode}");
        let live_thinking: String = output
            .snapshot()
            .await
            .into_iter()
            .filter_map(|event| match event {
                lingxi_core::host::OutputEvent::Thinking { thinking, .. } => Some(thinking),
                _ => None,
            })
            .collect();
        assert_eq!(
            live_thinking, thinking,
            "each initial/synthesized snapshot renders once: {mode}"
        );
        let mut partial_thinking = String::new();
        let mut partial_signature = String::new();
        let mut thinking_starts = 0;
        for source in output.partial_stream_event_snapshot().await {
            let event: serde_json::Value = serde_json::from_str(&source).unwrap();
            match event["type"].as_str() {
                Some("content_block_start") if event["content_block"]["type"] == "thinking" => {
                    thinking_starts += 1;
                    partial_thinking.push_str(event["content_block"]["thinking"].as_str().unwrap());
                    if let Some(signature) = event["content_block"]["signature"].as_str() {
                        partial_signature.push_str(signature);
                    }
                }
                Some("content_block_delta") => match event["delta"]["type"].as_str() {
                    Some("thinking_delta") => {
                        partial_thinking.push_str(event["delta"]["thinking"].as_str().unwrap())
                    }
                    Some("signature_delta") => {
                        partial_signature.push_str(event["delta"]["signature"].as_str().unwrap())
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        assert_eq!(thinking_starts, 1, "{mode}");
        assert_eq!(
            partial_thinking, thinking,
            "partial-event consumers receive the complete snapshot exactly once: {mode}"
        );
        assert_eq!(
            partial_signature, signature,
            "partial-event consumers retain initial and incremental signature: {mode}"
        );
        let requests = streaming.captured_calls().await;
        assert_eq!(requests.len(), 2, "{mode}");
        let messages =
            llm_runtime::convert::normalize_messages_for_api(requests[1].messages.clone());
        let replay = llm_runtime::convert::to_llm_messages(messages)
            .expect("production-normalized request must convert");
        let reasoning: Vec<_> = replay
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                llm_runtime::ContentBlock::Reasoning { text, signature } => {
                    Some((text.as_str(), signature.as_deref()))
                }
                _ => None,
            })
            .collect();
        assert_eq!(reasoning, [(thinking, Some(signature))], "{mode}");
        let rows: Vec<serde_json::Value> = std::fs::read_to_string(transcript)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let stored: Vec<_> = rows
            .iter()
            .filter(|row| row["type"] == "assistant")
            .flat_map(|row| row["message"]["content"].as_array().unwrap())
            .filter(|block| block["type"] == "thinking")
            .collect();
        assert_eq!(stored.len(), 1, "{mode}");
        assert_eq!(stored[0]["thinking"], thinking, "{mode}");
        assert_eq!(stored[0]["signature"], signature, "{mode}");
    }
}
