//! Regression tests for streamed server-fallback tombstones.

use super::*;
use crate::conversation::ConversationOrchestrator;
use crate::streaming_executor::StreamingToolExecutor;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::test_support_stream::{
    content_block_start_text, content_block_start_thinking, content_block_start_tool_use,
    content_block_stop, message_delta_stop, message_start, message_start_with_usage, message_stop,
    text_delta, thinking_delta,
};
use futures::stream::{self, StreamExt};
use lingxi_core::host::{CostSnapshot, OutputStream, ServerFallbackTombstoneMessage};
use lingxi_core::types::{MessageId, ToolUseId};
use llm_runtime::{HistoryEvent, LlmError};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tool_api::registry::ToolRegistry;

fn boxed(
    events: Vec<Result<HistoryEvent, LlmError>>,
) -> BoxStream<'static, Result<HistoryEvent, LlmError>> {
    stream::iter(events).boxed()
}

#[test]
fn server_fallback_reset_tombstones_all_query_tool_result_rows_and_keeps_other_journal_rows() {
    let tool_id = ToolUseId::from("old-tool-use");
    let result_id = MessageId::new();
    let second_tool_id = ToolUseId::from("other-old-tool-use");
    let second_result_id = MessageId::new();
    let other_id = MessageId::new();
    let assistant_id = MessageId::new();
    let result = lingxi_core::types::ConversationMessage::User { api_message_override: None,
        id: result_id,
        content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
            tool_use_id: tool_id.clone(),
            content: "old result".into(),
            is_error: Some(false),
            provider_tool_use_id: Some("provider-id".into()),
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    let second_result = lingxi_core::types::ConversationMessage::User { api_message_override: None,
        id: second_result_id,
        content: vec![lingxi_core::types::ContentBlock::ToolResult { content_projection: None,
            tool_use_id: second_tool_id.clone(),
            content: "other old result".into(),
            is_error: Some(false),
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    let unrelated = lingxi_core::types::ConversationMessage::user(other_id, "keep".into());
    let mut settlement = StreamToolSettlement {
        query_rows: vec![
            (result.clone(), None),
            (second_result.clone(), None),
            (unrelated.clone(), None),
        ],
        journal: vec![
            StreamEventJournalEntry::AssistantRow(assistant_id),
            StreamEventJournalEntry::UserRow {
                stored: result,
                parent_uuid: Some("assistant-row".into()),
                persist: true,
                timestamp: "2026-10-04T12:34:56.789Z".into(),
                tool_completion_id: Some(tool_id.clone()),
            },
            StreamEventJournalEntry::FlushHookAttachments(tool_id),
            StreamEventJournalEntry::UserRow {
                stored: second_result,
                parent_uuid: Some("other-assistant-row".into()),
                persist: true,
                timestamp: "2026-10-04T12:34:57.123Z".into(),
                tool_completion_id: Some(second_tool_id.clone()),
            },
            StreamEventJournalEntry::FlushHookAttachments(second_tool_id),
            StreamEventJournalEntry::UserRow {
                stored: unrelated,
                parent_uuid: None,
                persist: true,
                timestamp: "2026-10-04T12:34:57.000Z".into(),
                tool_completion_id: None,
            },
        ],
        ..Default::default()
    };

    let tombstones = settlement.discard_tool_completion_rows();
    assert_eq!(tombstones.len(), 2);
    assert_eq!(tombstones[0].uuid, result_id);
    assert_eq!(tombstones[0].message_type, "user");
    assert_eq!(tombstones[0].timestamp, "2026-10-04T12:34:56.789Z");
    assert!(matches!(
        tombstones[0].content.as_slice(),
        [lingxi_core::types::ContentBlock::ToolResult { tool_use_id: id, content, .. }]
            if id.as_str() == "old-tool-use" && content == "old result"
    ));
    assert_eq!(tombstones[1].uuid, second_result_id);
    assert_eq!(tombstones[1].timestamp, "2026-10-04T12:34:57.123Z");
    assert_eq!(settlement.query_rows.len(), 1);
    assert_eq!(settlement.query_rows[0].0.id(), other_id);
    assert_eq!(settlement.journal.len(), 2);
    assert!(matches!(
        settlement.journal[0],
        StreamEventJournalEntry::AssistantRow(id) if id == assistant_id
    ));
    assert!(matches!(
        settlement.journal[1],
        StreamEventJournalEntry::UserRow {
            tool_completion_id: None,
            ..
        }
    ));
}

async fn pump_tracked_turn(
    stream: BoxStream<'static, Result<HistoryEvent, LlmError>>,
    output: &Arc<dyn OutputStream>,
    pump: ExecutorPump<'_, '_>,
) -> Result<PumpedTurn, PumpFailure> {
    pump_stream_with_executor_tracked_remaining(stream, output, pump)
        .await
        .map(|(turn, _remaining)| turn)
}

async fn pump_with_fallback_controller(events: Vec<Result<HistoryEvent, LlmError>>) -> PumpedTurn {
    pump_with_fallback_controller_for_source(events, crate::config::QUERY_SOURCE_REPL_MAIN_THREAD)
        .await
}

async fn pump_with_fallback_controller_for_source(
    events: Vec<Result<HistoryEvent, LlmError>>,
    query_source: &str,
) -> PumpedTurn {
    let orch = orchestrator_with_fallback_allowlist_for_source(
        vec!["fallback-model".into()],
        query_source,
    );
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: lingxi_core::types::MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: native_server_fallback_supersedes_enabled(&orch.config.query_source),
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("fallback-controller stream should complete")
}

fn orchestrator_with_fallback_allowlist(
    allowed_models: Vec<String>,
) -> Arc<ConversationOrchestrator> {
    orchestrator_with_fallback_allowlist_for_source(
        allowed_models,
        crate::config::QUERY_SOURCE_REPL_MAIN_THREAD,
    )
}

fn orchestrator_with_fallback_allowlist_for_source(
    allowed_models: Vec<String>,
    query_source: &str,
) -> Arc<ConversationOrchestrator> {
    let mut config = crate::OrchestratorConfig::default();
    config.server_fallback_regular_available_models = Some(allowed_models);
    config.query_source = query_source.to_string();
    ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
        config,
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    ))
}

#[tokio::test]
async fn local_server_stitch_omits_native_supersedes_metadata() {
    let events = vec![
        Ok(message_start("old-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "incoming-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_with_fallback_controller_for_source(events, "agent:custom:reviewer").await;
    let stitched_row = turn
        .assistant_rows
        .iter()
        .find(
            |row| {
                matches!(row.content.first(), Some(ContentBlock::Text { text, .. }) if text == "retained text answer")
            },
        )
        .expect("the local J stitch keeps the incoming row");
    assert!(stitched_row.supersedes_row_ids.is_empty());
}

#[tokio::test]
async fn server_stitch_keeps_prefix_and_metadata_for_opaque_text() {
    let raw =
        serde_json::json!({"type":"text","text":"","citations":null,"extra_native":{"keep":289}});
    let events = vec![
        Ok(message_start("old-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "incoming-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: llm_runtime::ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: raw,
            },
        }),
        Ok(text_delta(1, " answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let turn = pump_with_fallback_controller_for_source(events, "agent:custom:reviewer").await;
    assert_eq!(
        turn.assistant_rows.len(),
        1,
        "stitched prefix has one accepted row"
    );
    assert_eq!(turn.assistant_blocks.len(), 1);
    assert!(
        matches!(turn.assistant_blocks.first(), Some(ContentBlock::ProviderContent { value, .. })
        if value["text"] == "retained text answer" && value["citations"].is_null() && value["extra_native"]["keep"] == 289)
    );
    assert_eq!(turn.assistant_rows[0].content, turn.assistant_blocks);
}

#[tokio::test]
async fn server_stitch_preserves_exact_utf16_in_the_incoming_text() {
    let events = vec![
        Ok(message_start("old-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "incoming-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: llm_runtime::ContentBlock::TextJsUtf16 {
                text: "�".into(),
                utf16_code_units: vec![0xd800],
                citations: None,
                cache_control: None,
            },
        }),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let turn = pump_with_fallback_controller_for_source(events, "agent:custom:reviewer").await;
    let mut expected: Vec<u16> = "retained text".encode_utf16().collect();
    expected.push(0xd800);
    assert_eq!(turn.assistant_rows.len(), 1);
    assert!(
        matches!(turn.assistant_blocks.as_slice(), [ContentBlock::TextJsUtf16 { text, utf16_code_units, .. }]
        if text == "retained text�" && *utf16_code_units == expected)
    );
}

fn fallback_event(discarded_blocks: Vec<usize>, reason: &str) -> HistoryEvent {
    HistoryEvent::ServerFallback {
        event: Box::new(
            lingxi_llm_client::providers::anthropic::fallback_response::ServerFallbackEvent {
                from_model: "requested-model".into(),
                to_model: "fallback-model".into(),
                reason: reason.into(),
                api_refusal_category: Some("policy".into()),
                mid_stream: true,
                request_id: Some("request-1".into()),
                discarded_blocks,
                retained_blocks: vec![0],
                retained_text: "retained text".into(),
                final_stop_reason: None,
            },
        ),
        profile: "anthropic-profile".into(),
        lane: lingxi_llm_client::providers::anthropic::fallback_request::ServerLane {
            for_model: "requested-model".into(),
            model: "fallback-model".into(),
            mode: lingxi_llm_client::providers::anthropic::fallback_request::LaneMode::Explicit,
        },
    }
}

fn response_observed(model: &str, response_id: &str) -> HistoryEvent {
    HistoryEvent::ResponseObserved {
        model: model.into(),
        response_id: Some(response_id.into()),
    }
}

fn retarget_fallback(event: &mut HistoryEvent, model: &str) {
    if let HistoryEvent::ServerFallback { event, lane, .. } = event {
        event.to_model = model.into();
        lane.model = model.into();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RefusalContinuationCall {
    salvage_text: String,
    replaces_uuids: Vec<MessageId>,
    display_salvage_text: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TombstoneSummary {
    uuid: MessageId,
    message_type: String,
    provider_message_id: Option<String>,
    model: Option<String>,
    stop_reason: Option<String>,
    stop_details: Option<serde_json::Value>,
    usage: Option<serde_json::Value>,
    content_blocks: usize,
    tool_use_id: Option<ToolUseId>,
    is_api_error_message: Option<bool>,
    supersedes_uuids: Vec<MessageId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FallbackOutputEvent {
    Text(String),
    AssistantBlockIdentity(MessageId),
    QueryModelChange(String),
    Tombstone(TombstoneSummary, bool),
    RefusalContinuationBegin(RefusalContinuationCall),
    MessageRetracted(MessageId),
}

#[derive(Default)]
struct RecordingFallbackOutput {
    events: Mutex<Vec<FallbackOutputEvent>>,
}

impl RecordingFallbackOutput {
    async fn snapshot(&self) -> Vec<FallbackOutputEvent> {
        self.events.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl OutputStream for RecordingFallbackOutput {
    async fn emit_message_retracted(&self, message_id: &MessageId) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::MessageRetracted(*message_id));
    }

    async fn emit_server_fallback_query_model_change(&self, to_model: &str) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::QueryModelChange(to_model.into()));
    }

    async fn emit_server_fallback_tombstone(
        &self,
        message: &ServerFallbackTombstoneMessage,
        display_only: bool,
    ) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::Tombstone(
                TombstoneSummary {
                    uuid: message.uuid,
                    message_type: message.message_type.clone(),
                    provider_message_id: message.provider_message_id.clone(),
                    model: message.model.clone(),
                    stop_reason: message.stop_reason.clone(),
                    stop_details: message.stop_details.clone(),
                    usage: message.usage.clone(),
                    content_blocks: message.content.len(),
                    tool_use_id: message.content.iter().find_map(|block| match block {
                        ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                        _ => None,
                    }),
                    is_api_error_message: message.is_api_error_message,
                    supersedes_uuids: message.supersedes_uuids.clone().unwrap_or_default(),
                },
                display_only,
            ));
    }

    async fn emit_refusal_continuation_begin(
        &self,
        salvage_text: &str,
        replaces_uuids: &[MessageId],
        display_salvage_text: bool,
    ) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::RefusalContinuationBegin(
                RefusalContinuationCall {
                    salvage_text: salvage_text.into(),
                    replaces_uuids: replaces_uuids.to_vec(),
                    display_salvage_text,
                },
            ));
    }

    async fn emit_assistant_block_identity(&self, _block_key: u64, row_id: &MessageId) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::AssistantBlockIdentity(*row_id));
    }

    async fn emit_text(&self, text: &str, _utf16_code_units: Option<&[u16]>) {
        self.events
            .lock()
            .await
            .push(FallbackOutputEvent::Text(text.to_string()));
    }

    async fn emit_tool_call(&self, _id: &ToolUseId, _tool: &str, _input: &serde_json::Value, _input_projection: Option<&lingxi_core::types::utf16_json::Utf16JsonProjection>) {}

    async fn emit_tool_result(
        &self,
        _id: &ToolUseId,
        _tool: &str,
        _model_text: &str,
        _result: &serde_json::Value,
     _projection: Option<&lingxi_core::host::ToolResultProjection>) {
    }

    async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
}

#[tokio::test]
async fn accepted_midstream_hop_replaces_completed_rows_with_the_eligible_incoming_row() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool_a = ToolUseId::from("old-tool-a".to_owned());
    let old_tool_b = ToolUseId::from("old-tool-b".to_owned());
    let new_tool = ToolUseId::from("new-tool".to_owned());
    let assistant_id = MessageId::new();
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            2,
            old_tool_a.clone(),
            "UnknownOldA",
        )),
        Ok(content_block_stop(2)),
        Ok(content_block_start_tool_use(
            7,
            old_tool_b.clone(),
            "UnknownOldB",
        )),
        Ok(content_block_stop(7)),
        Ok(response_observed("fallback-model", "fallback-inner-id")),
        Ok(fallback_event(vec![2, 7], "refusal")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "fresh answer")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            9,
            new_tool.clone(),
            "UnknownNew",
        )),
        Ok(content_block_stop(9)),
        Ok(message_delta_stop("tool_use")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id,
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("accepted fallback should complete");

    let output_events = sink.snapshot().await;
    assert_eq!(
        output_events[0],
        FallbackOutputEvent::Text("retained text".into())
    );
    let query_change_position = output_events
        .iter()
        .position(|event| *event == FallbackOutputEvent::QueryModelChange("fallback-model".into()))
        .expect("accepted hop emits a query model change");
    let tombstones: Vec<&TombstoneSummary> = output_events
        .iter()
        .filter_map(|event| match event {
            FallbackOutputEvent::Tombstone(row, true) if row.tool_use_id.is_some() => Some(row),
            _ => None,
        })
        .collect();
    assert_eq!(tombstones.len(), 2);
    assert!(tombstones.iter().all(|row| {
        row.message_type == "assistant"
            && row.provider_message_id.as_deref() == Some("old-attempt")
            && row.model.as_deref() == Some("fallback-model")
            && row.content_blocks == 1
            && row.is_api_error_message.is_none()
    }));
    assert_eq!(
        tombstones
            .iter()
            .filter_map(|row| row.tool_use_id.clone())
            .collect::<Vec<_>>(),
        [old_tool_a.clone(), old_tool_b.clone()]
    );
    assert_eq!(
        turn.tool_use_removals,
        vec![lingxi_core::host::tool_use_lifecycle::ToolUseRemoval {
            ids: vec![old_tool_a, old_tool_b],
            reason: Some(
                lingxi_core::host::tool_use_lifecycle::ToolUseRemovalReason::FallbackSweep,
            ),
        }]
    );
    let begin_position = output_events
        .iter()
        .position(|event| matches!(event, FallbackOutputEvent::RefusalContinuationBegin(_)))
        .expect("accepted retained text emits continuation begin");
    let discarded_tombstone_positions: Vec<usize> = output_events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            FallbackOutputEvent::Tombstone(row, true) if row.tool_use_id.is_some() => Some(index),
            _ => None,
        })
        .collect();
    assert!(query_change_position < discarded_tombstone_positions[0]);
    assert!(discarded_tombstone_positions
        .iter()
        .all(|position| *position < begin_position));
    let retained_id = turn
        .assistant_rows
        .iter()
        .find(|row| matches!(row.content.first(), Some(ContentBlock::Text { text, .. }) if text == "retained textfresh answer"))
        .and_then(|row| row.supersedes_row_ids.first())
        .copied()
        .expect("J merge keeps the original retained row identity in supersedes metadata");
    let Some(FallbackOutputEvent::RefusalContinuationBegin(stitch)) = output_events
        .iter()
        .find(|event| matches!(event, FallbackOutputEvent::RefusalContinuationBegin(_)))
    else {
        panic!("expected native refusal continuation begin after tombstones");
    };
    assert_eq!(stitch.salvage_text, "retained text");
    assert_eq!(stitch.replaces_uuids, [retained_id]);
    assert!(stitch.display_salvage_text);
    assert_eq!(stitch.replaces_uuids.len(), 1);

    let stitched_row = turn
        .assistant_rows
        .iter()
        .find(|row| turn.replacement_message_id == Some(row.row_id))
        .expect("replacement identity must be the completed incoming row");
    let replacement_identity_position = output_events
        .iter()
        .position(|event| {
            *event == FallbackOutputEvent::AssistantBlockIdentity(stitched_row.row_id)
        })
        .expect("the candidate row identity is emitted at block stop");
    let retained_row_id = stitched_row.supersedes_row_ids[0];
    let retained_tombstone_position = output_events
        .iter()
        .position(|event| {
            matches!(
                event,
                FallbackOutputEvent::Tombstone(row, true) if row.uuid == retained_row_id
            )
        })
        .expect("J originals are tombstoned after the incoming row is yielded");
    assert!(replacement_identity_position < retained_tombstone_position);
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Text(text) if text == "fresh answer"
    )));
    assert_eq!(stitched_row.provider_message_id, "fallback-inner-id");
    assert_eq!(stitched_row.model, "fallback-model");
    assert_eq!(stitched_row.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(
        stitched_row.model_profile.as_deref(),
        Some("anthropic-profile")
    );
    assert_eq!(stitched_row.supersedes_row_ids.len(), 1);
    assert!(stitch
        .replaces_uuids
        .contains(&stitched_row.supersedes_row_ids[0]));
    assert_eq!(
        stitched_row.content,
        vec![ContentBlock::Text {
            text: "retained textfresh answer".into(),
            citations: None
        }]
    );
    assert_eq!(turn.tool_uses.len(), 1);
    assert_eq!(turn.tool_uses[0].id, new_tool);
    assert_eq!(turn.handled_server_fallback_events, 1);
}

#[tokio::test]
async fn completed_eligible_stitch_is_not_undone_by_a_later_transport_error() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "fallback-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " answer")),
        Ok(content_block_stop(1)),
        Err(LlmError::Transport {
            message: "connection closed after completed block".into(),
        }),
    ];

    let failure = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect_err("the incomplete response remains a stream failure");

    let incoming_id = failure
        .partial
        .replacement_message_id
        .expect("completed eligible text commits the J seam immediately");
    assert_eq!(failure.partial.assistant_rows.len(), 1);
    assert_eq!(failure.partial.assistant_rows[0].row_id, incoming_id);
    assert_eq!(
        failure.partial.assistant_rows[0].content,
        vec![ContentBlock::Text {
            text: "retained text answer".into(),
            citations: None
        }]
    );
    assert_eq!(
        failure.partial.assistant_rows[0].supersedes_row_ids.len(),
        1
    );
    let output_events = sink.snapshot().await;
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::RefusalContinuationBegin(call)
            if call.salvage_text == "retained text" && call.display_salvage_text
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Text(text) if text == " answer"
    )));
}

#[tokio::test]
async fn display_hook_replacement_carries_retained_text_without_rendering_it_early() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let assistant_id = MessageId::new();
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "fallback-inner-id")),
        Ok(fallback_event(vec![], "sticky")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, "fresh answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id,
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: true,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("accepted fallback should complete");

    let output_events = sink.snapshot().await;
    let begins: Vec<&RefusalContinuationCall> = output_events
        .iter()
        .filter_map(|event| match event {
            FallbackOutputEvent::RefusalContinuationBegin(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(begins.len(), 1);
    assert_eq!(begins[0].salvage_text, "retained text");
    assert_eq!(begins[0].replaces_uuids.len(), 1);
    assert!(!begins[0].display_salvage_text);
    assert_eq!(turn.replacement_message_id, turn.assistant_row_identity);
    assert!(output_events
        .iter()
        .all(|event| !matches!(event, FallbackOutputEvent::Text(_))));
    let text_blocks: Vec<&str> = turn
        .assistant_blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text_blocks, ["retained textfresh answer"]);
}

#[tokio::test]
async fn accepted_hop_without_eligible_text_keeps_original_rows_and_does_not_replace_identity() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool = ToolUseId::from("discarded-without-eligible-text".to_owned());
    let assistant_id = MessageId::new();
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            2,
            old_tool.clone(),
            "UnknownOld",
        )),
        Ok(content_block_stop(2)),
        Ok(response_observed("fallback-model", "fallback-inner-id")),
        Ok(fallback_event(vec![2], "refusal")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " \t")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id,
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("an accepted hop without eligible text is still a completed response");

    assert!(turn.replacement_message_id.is_none());
    assert_eq!(turn.assistant_rows.len(), 2);
    assert_eq!(turn.assistant_rows[0].model, "fallback-model");
    assert!(turn.assistant_rows[0].supersedes_row_ids.is_empty());
    assert_eq!(turn.assistant_rows[1].model, "fallback-model");
    assert!(turn.assistant_rows[1].supersedes_row_ids.is_empty());
    let output_events = sink.snapshot().await;
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::RefusalContinuationBegin(call)
            if call.salvage_text == "retained text"
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true)
            if row.tool_use_id.as_ref() == Some(&old_tool)
    )));
    assert!(!output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true)
            if row.message_type == "assistant"
                && row.content_blocks == 1
                && row.tool_use_id.is_none()
                && turn.assistant_rows.iter().any(|retained| retained.row_id == row.uuid)
    )));
    assert!(turn.tool_uses.is_empty());
}

#[tokio::test]
async fn empty_retained_text_removes_only_discarded_tool_cards() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool = ToolUseId::from("discarded-tool".to_owned());
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let mut fallback = fallback_event(vec![2], "refusal");
    if let HistoryEvent::ServerFallback { event, .. } = &mut fallback {
        event.retained_text.clear();
        event.retained_blocks.clear();
    }
    let events = vec![
        Ok(message_start("attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "keep visible")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            2,
            old_tool.clone(),
            "UnknownDiscarded",
        )),
        Ok(content_block_stop(2)),
        Ok(response_observed("fallback-model", "fallback-inner-id")),
        Ok(fallback),
        Ok(content_block_start_text(4)),
        Ok(text_delta(4, "fresh")),
        Ok(content_block_stop(4)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("fallback without retained text should complete");

    let output_events = sink.snapshot().await;
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true)
            if row.tool_use_id.as_ref() == Some(&old_tool)
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Text(text) if text == "fresh"
    )));
    assert!(!output_events
        .iter()
        .any(|event| matches!(event, FallbackOutputEvent::RefusalContinuationBegin(_))));
    assert!(turn.replacement_message_id.is_none());
    assert!(turn.assistant_rows.iter().all(|row| {
        !row.content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }))
    }));
}

#[tokio::test]
async fn repeated_api_block_index_tombstones_only_the_latest_completed_row() {
    let orch = orchestrator_with_fallback_allowlist(vec![
        "fallback-model".into(),
        "fallback-model-two".into(),
    ]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let mut first_hop = fallback_event(vec![], "other");
    if let HistoryEvent::ServerFallback { event, .. } = &mut first_hop {
        event.retained_blocks.clear();
        event.retained_text.clear();
    }
    let mut second_hop = fallback_event(vec![0], "sticky");
    retarget_fallback(&mut second_hop, "fallback-model-two");
    if let HistoryEvent::ServerFallback { event, .. } = &mut second_hop {
        event.retained_blocks.clear();
        event.retained_text.clear();
    }
    let events = vec![
        Ok(message_start("initial-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "original row")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "first-hop-response")),
        Ok(first_hop),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "superseded row")),
        Ok(content_block_stop(0)),
        Ok(response_observed(
            "fallback-model-two",
            "second-hop-response",
        )),
        Ok(second_hop),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "latest row")),
        Ok(content_block_stop(0)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("multiple accepted hops with reused block indices should complete");

    let texts = turn
        .assistant_rows
        .iter()
        .flat_map(|row| row.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts, ["original row", "latest row"]);
    assert_eq!(
        turn.assistant_rows[0].provider_message_id,
        "initial-response"
    );
    // This older row is no longer the active Tw value for API index 0 after
    // that index was reused. Only the latest row at the discarded index is
    // retargeted/tombstoned by the second visible hop.
    assert_eq!(turn.assistant_rows[0].model, "requested-model");
    assert_eq!(
        turn.assistant_rows[1].provider_message_id,
        "second-hop-response"
    );
    assert_eq!(turn.assistant_rows[1].model, "fallback-model-two");
    assert_eq!(turn.served_model.as_deref(), Some("fallback-model-two"));
}

#[tokio::test]
async fn later_visible_stitch_seed_replaces_earlier_pending_seed() {
    let orch = orchestrator_with_fallback_allowlist(vec![
        "fallback-model".into(),
        "fallback-model-two".into(),
    ]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool = ToolUseId::from("discarded-before-latest-seed".to_owned());
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();

    let mut first_hop = fallback_event(vec![2], "refusal");
    if let HistoryEvent::ServerFallback { event, .. } = &mut first_hop {
        event.retained_text = "seed-A".into();
    }
    let mut second_hop = fallback_event(vec![], "sticky");
    retarget_fallback(&mut second_hop, "fallback-model-two");
    if let HistoryEvent::ServerFallback { event, .. } = &mut second_hop {
        event.retained_blocks = vec![1];
        event.retained_text = " \t".into();
    }

    let events = vec![
        Ok(message_start("response-A", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "seed-A")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            2,
            old_tool.clone(),
            "UnknownOld",
        )),
        Ok(content_block_stop(2)),
        Ok(response_observed("fallback-model", "response-B")),
        Ok(first_hop),
        // Whitespace-only B text is completed but is not eligible to consume J.
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " \t")),
        Ok(content_block_stop(1)),
        Ok(response_observed("fallback-model-two", "response-C")),
        Ok(second_hop),
        Ok(content_block_start_text(3)),
        Ok(text_delta(3, "fresh C row")),
        Ok(content_block_stop(3)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("the latest pending J seed should merge on C");

    let output_events = sink.snapshot().await;
    let replacement = output_events
        .iter()
        .filter_map(|event| match event {
            FallbackOutputEvent::RefusalContinuationBegin(replacement) => Some(replacement),
            _ => None,
        })
        .next_back()
        .expect("C should produce a refusal continuation begin");
    assert_eq!(replacement.salvage_text, " \t");

    let original_a = turn
        .assistant_rows
        .iter()
        .find(|row| matches!(row.content.first(), Some(ContentBlock::Text { text, .. }) if text == "seed-A"))
        .expect("the replaced older J source remains an independent completed row")
        .row_id;
    let stitched = turn
        .assistant_rows
        .iter()
        .find(|row| turn.replacement_message_id == Some(row.row_id))
        .expect("the incoming C row owns the merged identity");
    assert_eq!(
        stitched.content,
        vec![ContentBlock::Text {
            text: " \tfresh C row".into(),
            citations: None
        }]
    );
    // J's replacement list includes the whitespace-only retained row, while
    // native Ff/pre filtering omits that row from supersedesUuids.
    assert!(stitched.supersedes_row_ids.is_empty());
    assert_eq!(replacement.replaces_uuids.len(), 1);
    let b_original = replacement.replaces_uuids[0];
    assert_ne!(b_original, original_a);
    assert!(!replacement.replaces_uuids.contains(&original_a));
    assert!(replacement.replaces_uuids.contains(&b_original));
    assert!(!output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true) if row.uuid == original_a
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true) if row.uuid == b_original
    )));
    assert_eq!(turn.assistant_rows.len(), 2);
}

#[tokio::test]
async fn accepted_visible_hop_without_new_seed_reemits_the_pending_j_begin() {
    let orch = orchestrator_with_fallback_allowlist(vec![
        "fallback-model".into(),
        "fallback-model-two".into(),
    ]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let first_hop = fallback_event(vec![], "refusal");
    let mut second_hop = fallback_event(vec![], "sticky");
    retarget_fallback(&mut second_hop, "fallback-model-two");
    if let HistoryEvent::ServerFallback { event, .. } = &mut second_hop {
        event.mid_stream = false;
        event.retained_blocks.clear();
        event.retained_text.clear();
    }
    let events = vec![
        Ok(message_start("original-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "original prefix")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "fallback-response")),
        Ok(first_hop),
        Ok(response_observed(
            "fallback-model-two",
            "fallback-response-two",
        )),
        Ok(second_hop),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, "answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("the second accepted hop reuses pending J despite its empty terminal event");

    let output_events = sink.snapshot().await;
    let begins: Vec<&RefusalContinuationCall> = output_events
        .iter()
        .filter_map(|event| match event {
            FallbackOutputEvent::RefusalContinuationBegin(call) => Some(call),
            _ => None,
        })
        .collect();
    assert_eq!(begins.len(), 2);
    assert!(begins
        .iter()
        .all(|call| call.salvage_text == "retained text"));
    assert_eq!(begins[0].replaces_uuids, begins[1].replaces_uuids);
    assert_eq!(turn.assistant_rows.len(), 1);
    assert!(turn.assistant_rows[0]
        .supersedes_row_ids
        .contains(&begins[0].replaces_uuids[0]));
    let retained_tombstones: Vec<&TombstoneSummary> = output_events
        .iter()
        .filter_map(|event| match event {
            FallbackOutputEvent::Tombstone(row, true)
                if row.uuid == begins[0].replaces_uuids[0] =>
            {
                Some(row)
            }
            _ => None,
        })
        .collect();
    assert_eq!(retained_tombstones.len(), 1);
}

#[tokio::test]
async fn reused_block_index_does_not_relabel_an_older_pending_j_original() {
    let orch = orchestrator_with_fallback_allowlist(vec![
        "fallback-model".into(),
        "fallback-model-two".into(),
        "fallback-model-three".into(),
    ]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let first_hop = fallback_event(vec![], "refusal");
    let mut second_hop = fallback_event(vec![], "sticky");
    retarget_fallback(&mut second_hop, "fallback-model-two");
    if let HistoryEvent::ServerFallback { event, .. } = &mut second_hop {
        event.mid_stream = false;
        event.retained_blocks.clear();
        event.retained_text.clear();
    }
    let mut third_hop = fallback_event(vec![], "sticky");
    retarget_fallback(&mut third_hop, "fallback-model-three");
    if let HistoryEvent::ServerFallback { event, .. } = &mut third_hop {
        event.mid_stream = false;
        event.retained_blocks.clear();
        event.retained_text.clear();
    }
    let mut prior_delta_usage = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 27,
        output_tokens: 8,
        ..Default::default()
    });
    prior_delta_usage.provider_metadata = serde_json::json!({
        "input_tokens": 27,
        "output_tokens": 8,
    });
    let events = vec![
        Ok(message_start("original-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "original prefix")),
        Ok(content_block_stop(0)),
        Ok(response_observed("fallback-model", "fallback-response")),
        Ok(first_hop),
        // A new response row reuses index zero, taking over the producer's
        // current block map while J still holds the older row object.
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, " \t")),
        Ok(content_block_stop(0)),
        Ok(response_observed(
            "fallback-model-two",
            "fallback-response-two",
        )),
        Ok(second_hop),
        // Native MessageDelta mutates row objects retained by J before the
        // next accepted hop; tombstones later carry these latest terminal facts.
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: Some("refusal".into()),
                stop_details: Some(llm_runtime::HistoryStopDetails {
                    category: Some("cyber".into()),
                    explanation: Some("prior refusal".into()),
                }),
            },
            usage: Some(prior_delta_usage.clone()),
        }),
        Ok(response_observed(
            "fallback-model-three",
            "fallback-response-three",
        )),
        Ok(third_hop),
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: None,
                stop_details: None,
            },
            usage: Some(prior_delta_usage.clone()),
        }),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, "answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("later accepted hops preserve the displaced J row's physical model");

    let retained_id = turn.assistant_rows.iter().find_map(|row| {
        (turn.replacement_message_id == Some(row.row_id))
            .then(|| row.supersedes_row_ids.first().copied())
            .flatten()
    });
    let retained_id = retained_id.expect("the eligible third-response row consumes J");
    let tombstone = sink
        .snapshot()
        .await
        .into_iter()
        .find_map(|event| match event {
            FallbackOutputEvent::Tombstone(row, true) if row.uuid == retained_id => Some(row),
            _ => None,
        });
    let tombstone = tombstone.expect("the displaced J original is tombstoned after merge");
    assert_eq!(tombstone.model, Some("fallback-model".into()));
    assert_eq!(tombstone.stop_reason.as_deref(), Some("refusal"));
    assert_eq!(
        tombstone.stop_details,
        Some(serde_json::json!({
            "category": "cyber",
            "explanation": "prior refusal"
        }))
    );
    assert_eq!(
        tombstone.usage,
        Some(serde_json::json!({"input_tokens": 27, "output_tokens": 8}))
    );
    let candidate = turn
        .assistant_rows
        .iter()
        .find(|row| row.row_id == turn.replacement_message_id.unwrap())
        .expect("the next eligible text row uses the latest hop model");
    assert_eq!(candidate.model, "fallback-model-three");
}

#[tokio::test]
async fn usage_only_delta_can_set_details_before_a_terminal_reason_exists() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let details = llm_runtime::HistoryStopDetails {
        category: Some("bio".into()),
        explanation: Some("pre-terminal detail".into()),
    };
    let events = vec![
        Ok(message_start("original-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: None,
                stop_details: Some(details.clone()),
            },
            usage: None,
        }),
        Ok(response_observed("fallback-model", "fallback-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("the eligible row should consume the pending J seam");

    let retained_id = turn.assistant_rows.iter().find_map(|row| {
        (turn.replacement_message_id == Some(row.row_id))
            .then(|| row.supersedes_row_ids.first().copied())
            .flatten()
    });
    let retained_id = retained_id.expect("the retained row is superseded by the incoming row");
    let tombstone = sink
        .snapshot()
        .await
        .into_iter()
        .find_map(|event| match event {
            FallbackOutputEvent::Tombstone(row, true) if row.uuid == retained_id => Some(row),
            _ => None,
        })
        .expect("the retained row is tombstoned after the incoming row");
    assert_eq!(tombstone.stop_reason, None);
    assert_eq!(
        tombstone.stop_details,
        Some(serde_json::json!({
            "category": "bio",
            "explanation": "pre-terminal detail"
        }))
    );
}

#[tokio::test]
async fn terminal_reason_delta_replaces_and_can_clear_prior_stop_details() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let events = vec![
        Ok(message_start("original-response", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: Some("refusal".into()),
                stop_details: Some(llm_runtime::HistoryStopDetails {
                    category: Some("cyber".into()),
                    explanation: Some("prior refusal".into()),
                }),
            },
            usage: None,
        }),
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: Some("end_turn".into()),
                stop_details: None,
            },
            usage: None,
        }),
        Ok(response_observed("fallback-model", "fallback-response")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(content_block_start_text(1)),
        Ok(text_delta(1, " answer")),
        Ok(content_block_stop(1)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("the eligible row should consume the pending J seam");

    let retained_id = turn.assistant_rows.iter().find_map(|row| {
        (turn.replacement_message_id == Some(row.row_id))
            .then(|| row.supersedes_row_ids.first().copied())
            .flatten()
    });
    let retained_id = retained_id.expect("the retained row is superseded by the incoming row");
    let tombstone = sink
        .snapshot()
        .await
        .into_iter()
        .find_map(|event| match event {
            FallbackOutputEvent::Tombstone(row, true) if row.uuid == retained_id => Some(row),
            _ => None,
        })
        .expect("the retained row is tombstoned after the incoming row");
    assert_eq!(tombstone.stop_reason.as_deref(), Some("end_turn"));
    assert_eq!(tombstone.stop_details, None);
}

#[tokio::test]
async fn native_unpriced_quote_keeps_its_summary_model_without_aggregate_usage() {
    let events = vec![
        Ok(message_start("response", "request-model")),
        Ok(HistoryEvent::CostQuoteObserved {
            estimate: None,
            native_server_fallback: true,
            summary_model: Some("actual-iteration-model".into()),
        }),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "answer")),
        Ok(content_block_stop(0)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let output: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
    let turn = pump_stream(boxed(events), &output)
        .await
        .expect("an unpriced quote observation does not alter the stream");

    assert!(turn.native_server_fallback_quote);
    assert!(turn.cost_quote.is_none());
    assert_eq!(
        turn.native_cost_model.as_deref(),
        Some("actual-iteration-model")
    );
}

#[tokio::test]
async fn terminal_aggregate_quote_clears_a_provisional_native_fallback_marker() {
    let aggregate_estimate = llm_runtime::CostEstimate::unestimated(llm_runtime::PricingModelRef {
        pricing_provider_id: llm_runtime::ProviderId::AnthropicFirstParty,
        billing_model: "request-model".into(),
        request_model: "request-model".into(),
        display_model: "Request model".into(),
    });
    let events = vec![
        Ok(message_start("response", "request-model")),
        Ok(HistoryEvent::CostQuoteObserved {
            estimate: None,
            native_server_fallback: true,
            summary_model: Some("provisional-iteration-model".into()),
        }),
        Ok(HistoryEvent::CostQuoteObserved {
            estimate: Some(aggregate_estimate.clone()),
            native_server_fallback: false,
            summary_model: None,
        }),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "answer")),
        Ok(content_block_stop(0)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let output: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());
    let turn = pump_stream(boxed(events), &output)
        .await
        .expect("the terminal typed quote controls settlement state");

    assert!(turn.cost_quote_observed);
    assert!(!turn.native_server_fallback_quote);
    assert!(turn.native_cost_model.is_none());
    assert_eq!(turn.cost_quote, Some(aggregate_estimate));
}

#[tokio::test]
async fn nonvisible_midstream_observation_does_not_replace_display_or_reset_tools() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let session_model_before = orch.session.lock().await.model.clone();
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool = ToolUseId::from("observed-old-tool".to_owned());
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();
    let mut observation = fallback_event(vec![3], "other");
    let HistoryEvent::ServerFallback { event, .. } = &mut observation else {
        unreachable!();
    };
    event.retained_blocks.clear();
    event.retained_text.clear();
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_tool_use(
            3,
            old_tool.clone(),
            "UnknownOld",
        )),
        Ok(content_block_stop(3)),
        Ok(response_observed(
            "observed-physical-model",
            "physical-response",
        )),
        Ok(observation),
        Ok(message_delta_stop("tool_use")),
        Ok(message_stop()),
    ];

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("nonvisible observation should remain observational");

    assert!(sink
        .snapshot()
        .await
        .iter()
        .all(|event| matches!(event, FallbackOutputEvent::AssistantBlockIdentity(_))));
    assert_eq!(turn.tool_uses.len(), 1);
    assert_eq!(turn.tool_uses[0].id, old_tool);
    assert_eq!(turn.server_fallback_events.len(), 1);
    assert_eq!(turn.handled_server_fallback_events, 0);
    assert_eq!(
        turn.served_model.as_deref(),
        Some("observed-physical-model")
    );
    let session_model_after = orch.session.lock().await.model.clone();
    assert_eq!(session_model_after, session_model_before);
    assert!(turn.assistant_rows[0].content.iter().any(|block| matches!(
        block,
        ContentBlock::ToolUse { id, .. } if id == &turn.tool_uses[0].id
    )));
    assert_eq!(executor.tools.len(), 1);
}

#[tokio::test]
async fn server_fallback_tombstones_tools_by_completed_block_index_and_retains_text() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let old_tool_a = ToolUseId::from("old-tool-a".to_owned());
    let old_tool_b = ToolUseId::from("old-tool-b".to_owned());
    let new_tool = ToolUseId::from("new-tool".to_owned());
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(2, old_tool_a, "UnknownOldA")),
        Ok(content_block_stop(2)),
        Ok(content_block_start_thinking(4)),
        Ok(thinking_delta(4, "discarded reasoning")),
        Ok(content_block_stop(4)),
        Ok(content_block_start_tool_use(7, old_tool_b, "UnknownOldB")),
        Ok(content_block_stop(7)),
        Ok(fallback_event(vec![2, 4, 7], "refusal")),
        Ok(message_start("fallback-attempt", "fallback-model")),
        // Reusing the provider's block index must not change the fact that the
        // prior completed text was retained at the fallback boundary.
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "fresh answer")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            9,
            new_tool.clone(),
            "UnknownNew",
        )),
        Ok(content_block_stop(9)),
        Ok(message_delta_stop("tool_use")),
        Ok(message_stop()),
    ];
    let sink = Arc::new(MockOutputStream::new());
    let output: Arc<dyn OutputStream> = sink.clone();

    let turn = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: lingxi_core::types::MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect("fallback stream should complete");

    let texts: Vec<&str> = turn
        .assistant_blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["retained textfresh answer"]);
    assert!(!turn
        .assistant_blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Thinking { .. })));
    assert_eq!(turn.tool_uses.len(), 1);
    assert_eq!(turn.tool_uses[0].id, new_tool);
    assert_eq!(turn.served_model.as_deref(), Some("fallback-model"));
    assert_eq!(turn.handled_server_fallback_events, 1);
    assert_eq!(turn.server_fallback_events.len(), 1);
    let observation = &turn.server_fallback_events[0];
    assert_eq!(observation.profile, "anthropic-profile");
    assert_eq!(observation.event.request_id.as_deref(), Some("request-1"));
    assert_eq!(observation.event.retained_blocks, [0]);
    assert_eq!(observation.event.retained_text, "retained text");

    // The old tool blocks were completed before the server hop. The live
    // controller applied the hop, so only post-hop results remain drainable.
    assert_eq!(turn.tool_uses[0].id, new_tool);
    assert_eq!(executor.tools.len(), 1);
    assert_eq!(executor.tools[0].id, new_tool);
    executor.run_to_completion().await.unwrap();
    let drained = executor.take_newly_completed();
    assert_eq!(drained.len(), 1);
    assert!(matches!(
        &drained[0].block,
        ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id.as_str() == new_tool.as_str()
    ));
}

#[tokio::test]
async fn fallback_observation_and_served_model_survive_a_later_stream_failure() {
    let orch = orchestrator_with_fallback_allowlist(vec!["fallback-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let events = vec![
        Ok(message_start("old-attempt", "requested-model")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained text")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            3,
            ToolUseId::from("discarded-tool".to_owned()),
            "UnknownOld",
        )),
        Ok(content_block_stop(3)),
        Ok(fallback_event(vec![3], "refusal")),
        Ok(message_start("fallback-attempt", "fallback-model")),
        Ok(content_block_start_text(5)),
        Ok(text_delta(5, "partial fallback answer")),
        Err(LlmError::Transport {
            message: "connection closed".into(),
        }),
    ];
    let sink = Arc::new(RecordingFallbackOutput::default());
    let output: Arc<dyn OutputStream> = sink.clone();

    let failure = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect_err("transport close should remain a stream failure");

    assert_eq!(
        failure.partial.served_model.as_deref(),
        Some("fallback-model")
    );
    assert_eq!(failure.partial.server_fallback_events.len(), 1);
    assert_eq!(
        failure.partial.tool_use_removals,
        vec![lingxi_core::host::tool_use_lifecycle::ToolUseRemoval {
            ids: vec![ToolUseId::from("discarded-tool")],
            reason: Some(
                lingxi_core::host::tool_use_lifecycle::ToolUseRemovalReason::FallbackSweep,
            ),
        }]
    );
    assert_eq!(failure.partial.handled_server_fallback_events, 1);
    assert_eq!(
        failure.partial.server_fallback_events[0].profile,
        "anthropic-profile"
    );
    let texts: Vec<&str> = failure
        .partial
        .assistant_blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["retained text", "partial fallback answer"]);
    assert!(failure.partial.tool_uses.is_empty());
    assert!(failure.partial.replacement_message_id.is_none());
    let retained_row_id = failure
        .partial
        .assistant_rows
        .iter()
        .find(|row| matches!(row.content.first(), Some(ContentBlock::Text { text, .. }) if text == "retained text"))
        .expect("the pending J original remains after an incomplete incoming block")
        .row_id;
    let output_events = sink.snapshot().await;
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::QueryModelChange(model) if model == "fallback-model"
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true)
            if row.tool_use_id.as_ref().is_some_and(|id| id.as_str() == "discarded-tool")
    )));
    assert!(output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::RefusalContinuationBegin(call)
            if call.salvage_text == "retained text"
                && call.replaces_uuids.len() == 1
                && call.replaces_uuids[0] == retained_row_id
    )));
    assert!(!output_events.iter().any(|event| matches!(
        event,
        FallbackOutputEvent::Tombstone(row, true) if row.uuid == retained_row_id
    )));
}

#[tokio::test]
async fn declined_server_fallback_returns_event_and_paid_usage_without_partial_output() {
    let orch = orchestrator_with_fallback_allowlist(vec!["requested-model".into()]);
    let mut executor = StreamingToolExecutor::try_new(&orch, Vec::new())
        .await
        .expect("fallback test binds the streaming scheduler owner");
    let mut start_usage = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 321,
        ..Default::default()
    });
    start_usage.provider_metadata = serde_json::json!({"provider": "fixture"});
    let events = vec![
        Ok(message_start_with_usage(
            "old-attempt",
            "requested-model",
            start_usage,
        )),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "retained before decline")),
        Ok(content_block_stop(0)),
        Ok(content_block_start_tool_use(
            2,
            ToolUseId::from("discarded-tool".to_owned()),
            "UnknownOld",
        )),
        Ok(content_block_stop(2)),
        Ok(fallback_event(vec![2], "sticky")),
        // The pump must stop at the declined control event and never observe
        // the response body belonging to the rejected model.
        Ok(message_start("declined-attempt", "fallback-model")),
        Ok(content_block_start_text(3)),
        Ok(text_delta(3, "must not reach history")),
        Ok(content_block_stop(3)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let sink = Arc::new(MockOutputStream::new());
    let output: Arc<dyn OutputStream> = sink.clone();

    let failure = pump_tracked_turn(
        boxed(events),
        &output,
        ExecutorPump {
            executor: &mut executor,
            assistant_id: lingxi_core::types::MessageId::new(),
            query_history: Vec::new(),
            model_profile: None,
            record_supersedes: true,
            user_cancel: None,
            suppress_live_text: false,
            suppress_live_thinking: false,
            settlement: None,
        },
    )
    .await
    .expect_err("the regular fallback allowlist rejects this model");

    assert_eq!(
        failure.disposition,
        PumpFailureDisposition::ServerFallbackDeclined
    );
    assert!(failure.partial.assistant_blocks.is_empty());
    assert!(failure.partial.tool_uses.is_empty());
    assert_eq!(failure.partial.handled_server_fallback_events, 0);
    assert_eq!(failure.partial.server_fallback_events.len(), 1);
    assert_eq!(
        failure.partial.server_fallback_events[0].event.reason,
        "sticky"
    );
    assert_eq!(
        failure
            .partial
            .usage
            .as_ref()
            .unwrap()
            .counts()
            .input_tokens,
        321
    );
    assert_eq!(
        sink.text_events().await,
        ["retained before decline"],
        "the rejected model body is never projected to the output sink"
    );
}

#[test]
fn fallback_tombstones_use_completed_block_indices_not_vector_positions() {
    let kept_tool = ToolUseId::from("kept-tool".to_owned());
    let discarded_tool = ToolUseId::from("discarded-tool".to_owned());
    let kept_text_row_id = MessageId::new();
    let discarded_thinking_row_id = MessageId::new();
    let kept_tool_row_id = MessageId::new();
    let discarded_tool_row_id = MessageId::new();
    let mut turn = PumpedTurn {
        assistant_blocks: vec![
            ContentBlock::Text {
                text: "keep".into(),
                citations: None,
            },
            ContentBlock::Thinking {
                thinking: "discard".into(),
                signature: None,
            },
        ],
        tool_uses: vec![
            ObservedToolUse {
                id: kept_tool.clone(),
                name: "Read".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
            ObservedToolUse {
                id: discarded_tool.clone(),
                name: "Read".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
        ],
        assistant_rows: vec![
            CompletedAssistantRow { per_turn_effort: None,
                stream_order: 0,
                row_id: kept_text_row_id,
                provider_message_id: "response-1".into(),
                model: "model-1".into(),
                model_profile: None,
                is_api_error: false,
                stop_reason: None,
                stop_details: None,
                usage: None,
                request_id: None,
                timestamp: "2026-10-03T00:00:00.000Z".into(),
                persisted_link: None,
                content: vec![ContentBlock::Text {
                    text: "keep".into(),
                    citations: None,
                }],
                session_append_dispatched: false,
                supersedes_row_ids: Vec::new(),
            },
            CompletedAssistantRow { per_turn_effort: None,
                stream_order: 1,
                row_id: kept_tool_row_id,
                provider_message_id: "response-1".into(),
                model: "model-1".into(),
                model_profile: None,
                is_api_error: false,
                stop_reason: None,
                stop_details: None,
                usage: None,
                request_id: None,
                timestamp: "2026-10-03T00:00:00.000Z".into(),
                persisted_link: None,
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: kept_tool.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                    provider_id: None,
                }],
                session_append_dispatched: false,
                supersedes_row_ids: Vec::new(),
            },
            CompletedAssistantRow { per_turn_effort: None,
                stream_order: 2,
                row_id: discarded_thinking_row_id,
                provider_message_id: "response-1".into(),
                model: "model-1".into(),
                model_profile: None,
                is_api_error: false,
                stop_reason: None,
                stop_details: None,
                usage: None,
                request_id: None,
                timestamp: "2026-10-03T00:00:00.000Z".into(),
                persisted_link: None,
                content: vec![ContentBlock::Thinking {
                    thinking: "discard".into(),
                    signature: None,
                }],
                session_append_dispatched: false,
                supersedes_row_ids: Vec::new(),
            },
            CompletedAssistantRow { per_turn_effort: None,
                stream_order: 3,
                row_id: discarded_tool_row_id,
                provider_message_id: "response-1".into(),
                model: "model-1".into(),
                model_profile: None,
                is_api_error: false,
                stop_reason: None,
                stop_details: None,
                usage: None,
                request_id: None,
                timestamp: "2026-10-03T00:00:00.000Z".into(),
                persisted_link: None,
                content: vec![ContentBlock::ToolUse { input_projection: None,
                    id: discarded_tool.clone(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                    provider_id: None,
                }],
                session_append_dispatched: false,
                supersedes_row_ids: Vec::new(),
            },
        ],
        ..PumpedTurn::default()
    };
    let mut assistant_indices = vec![(0, false), (4, false)];
    let mut assistant_row_ids = vec![kept_text_row_id, discarded_thinking_row_id];
    let mut tool_row_ids = vec![kept_tool_row_id, discarded_tool_row_id];
    let mut current_row_by_api_index = std::collections::HashMap::from([
        (0, kept_text_row_id),
        (2, kept_tool_row_id),
        (4, discarded_thinking_row_id),
        (7, discarded_tool_row_id),
    ]);
    let discarded_row_ids = rows_for_api_indices(&current_row_by_api_index, &[4, 7]);

    let discarded_tool_use = tombstone_server_fallback_blocks(
        &mut turn,
        &mut assistant_indices,
        &mut assistant_row_ids,
        &mut tool_row_ids,
        &mut current_row_by_api_index,
        &discarded_row_ids,
    );

    assert!(discarded_tool_use);
    assert_eq!(turn.assistant_blocks.len(), 1);
    assert!(matches!(
        &turn.assistant_blocks[0],
        ContentBlock::Text { text, .. } if text == "keep"
    ));
    assert_eq!(turn.tool_uses.len(), 1);
    assert_eq!(turn.tool_uses[0].id, kept_tool);
    assert_eq!(assistant_indices, [(0, false)]);
    assert_eq!(assistant_row_ids, [kept_text_row_id]);
    assert_eq!(tool_row_ids, [kept_tool_row_id]);
    assert_eq!(
        turn.assistant_rows
            .iter()
            .map(|row| row.row_id)
            .collect::<Vec<_>>(),
        [kept_text_row_id, kept_tool_row_id]
    );
    assert_eq!(current_row_by_api_index.len(), 2);
    assert_eq!(current_row_by_api_index.get(&0), Some(&kept_text_row_id));
    assert_eq!(current_row_by_api_index.get(&2), Some(&kept_tool_row_id));
}

fn projected_response_usage(model: &str) -> llm_runtime::ExecutionUsage {
    let mut usage = llm_runtime::ExecutionUsage::from_counts(llm_runtime::Usage {
        input_tokens: 17,
        output_tokens: 3,
        ..Default::default()
    });
    usage.provider_metadata = serde_json::json!({
        "stream":{"llm_client":{"response_model":model}}
    });
    usage
}

fn message_delta_with_usage(model: &str) -> HistoryEvent {
    HistoryEvent::MessageDelta {
        delta: llm_runtime::HistoryMessageDelta {
            stop_reason: Some("end_turn".into()),
            stop_details: None,
        },
        usage: Some(projected_response_usage(model)),
    }
}

#[tokio::test]
async fn suppressed_hop_updates_served_model_from_terminal_usage_metadata() {
    let events = vec![
        Ok(message_start("primary", "requested-model")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(message_delta_with_usage("second-served-model")),
        Ok(message_stop()),
    ];
    let turn = pump_with_fallback_controller(events).await;

    assert_eq!(turn.served_model.as_deref(), Some("second-served-model"));
    assert_eq!(turn.server_fallback_events.len(), 1);
    assert_eq!(turn.handled_server_fallback_events, 1);
    assert_eq!(turn.stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn suppressed_hop_updates_served_model_from_no_usage_observation_block() {
    let observation = HistoryEvent::ContentBlockStart {
        index: u32::MAX,
        content_block: llm_runtime::ContentBlock::ProviderContent {
            protocol: "anthropic_messages".into(),
            value: serde_json::json!({
                "type":"lingxi_observation",
                "metadata":{"llm_client":{"response_model":"second-served-model"}}
            }),
        },
    };
    let events = vec![
        Ok(message_start("primary", "requested-model")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(observation),
        Ok(content_block_stop(u32::MAX)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let turn = pump_with_fallback_controller(events).await;

    assert_eq!(turn.served_model.as_deref(), Some("second-served-model"));
    assert_eq!(turn.server_fallback_events.len(), 1);
    assert_eq!(turn.handled_server_fallback_events, 1);
    assert!(
        turn.assistant_blocks.is_empty(),
        "host metadata is not assistant content"
    );
}

#[tokio::test]
async fn ordinary_turn_does_not_adopt_a_projected_model_without_a_fallback_event() {
    let mut spoofed_usage = projected_response_usage("spoofed-served-model");
    spoofed_usage.provider_metadata = serde_json::json!({
        "stream":{"llm_client":{"response_model":"spoofed-served-model"}}
    });
    let events = vec![
        Ok(message_start("ordinary", "provider-served-version")),
        Ok(content_block_start_text(0)),
        Ok(text_delta(0, "ordinary answer")),
        Ok(content_block_stop(0)),
        // A provider-native unknown block may use the reserved type label in
        // its payload. Only the projector's high-end sentinel carries host
        // response metadata; ordinary provider blocks remain visible content.
        Ok(HistoryEvent::ContentBlockStart {
            index: 1,
            content_block: llm_runtime::ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: serde_json::json!({
                    "type":"lingxi_observation",
                    "metadata":{"llm_client":{"response_model":"spoofed-served-model"}}
                }),
            },
        }),
        Ok(content_block_stop(1)),
        Ok(HistoryEvent::MessageDelta {
            delta: llm_runtime::HistoryMessageDelta {
                stop_reason: Some("end_turn".into()),
                stop_details: None,
            },
            usage: Some(spoofed_usage),
        }),
        Ok(message_stop()),
    ];
    let output: Arc<dyn OutputStream> = Arc::new(MockOutputStream::new());

    let turn = pump_stream_inner(boxed(events), &output, None)
        .await
        .map(|(turn, _remaining)| turn)
        .expect("ordinary stream should complete");

    assert_eq!(turn.served_model, None);
    assert!(turn.server_fallback_events.is_empty());
    assert!(matches!(
        turn.assistant_blocks.as_slice(),
        [
            ContentBlock::Text { text, .. },
            ContentBlock::ProviderContent { value, .. }
        ] if text == "ordinary answer" && value["type"] == "lingxi_observation"
    ));
}

#[tokio::test]
async fn provider_native_observation_type_does_not_override_a_fallback_model() {
    let provider_block = serde_json::json!({
        "type":"lingxi_observation",
        "metadata":{"llm_client":{"response_model":"spoofed-served-model"}}
    });
    let events = vec![
        Ok(message_start("primary", "requested-model")),
        Ok(fallback_event(vec![], "refusal")),
        Ok(HistoryEvent::ContentBlockStart {
            index: 7,
            content_block: llm_runtime::ContentBlock::ProviderContent {
                protocol: "anthropic_messages".into(),
                value: provider_block.clone(),
            },
        }),
        Ok(content_block_stop(7)),
        Ok(message_delta_stop("end_turn")),
        Ok(message_stop()),
    ];
    let turn = pump_with_fallback_controller(events).await;

    assert_eq!(turn.served_model.as_deref(), Some("fallback-model"));
    assert_eq!(turn.server_fallback_events.len(), 1);
    assert_eq!(turn.handled_server_fallback_events, 1);
    assert!(matches!(
        turn.assistant_blocks.as_slice(),
        [ContentBlock::ProviderContent { value, .. }] if value == &provider_block
    ));
}
