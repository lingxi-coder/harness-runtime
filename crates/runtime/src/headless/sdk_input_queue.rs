//! The SDK's ordered input channel, shared by idle dispatch and the existing
//! driver's post-tool fold. Ordinary responses leave followers for idle dispatch.
use std::collections::VecDeque;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use orchestrator::prompt::mid_turn_input::{MidTurnInput, MidTurnInputPoint, MidTurnInputSource};
use tokio::sync::{mpsc, Mutex};

use super::{QueueLifecycle, LIFECYCLE_STARTED};
use crate::headless::stream_json::StreamJsonStream;
use crate::headless::stream_json_input::{
    content_to_prompt, emit_replay_ack_projected_queued, PendingInputQueue, StreamInput, UserTurn,
};

struct InputState {
    receiver: mpsc::Receiver<StreamInput>,
    reader_pending: PendingInputQueue,
    pending: VecDeque<StreamInput>,
    folded: Vec<UserTurn>,
}

pub(in crate::headless) struct SdkInputQueue {
    state: Mutex<InputState>,
    lifecycle: Arc<QueueLifecycle>,
    stream: Arc<StreamJsonStream>,
    orchestrator: Weak<orchestrator::ConversationOrchestrator>,
    replay: bool,
    session_id: String,
}

impl SdkInputQueue {
    pub(in crate::headless) fn new(
        receiver: mpsc::Receiver<StreamInput>,
        reader_pending: PendingInputQueue,
        lifecycle: Arc<QueueLifecycle>,
        stream: Arc<StreamJsonStream>,
        orchestrator: Arc<orchestrator::ConversationOrchestrator>,
        replay: bool,
        session_id: String,
    ) -> Self {
        Self {
            state: Mutex::new(InputState {
                receiver,
                reader_pending,
                pending: VecDeque::new(),
                folded: Vec::new(),
            }),
            lifecycle,
            stream,
            orchestrator: Arc::downgrade(&orchestrator),
            replay,
            session_id,
        }
    }

    pub(in crate::headless) async fn recv(&self) -> Option<StreamInput> {
        let mut state = self.state.lock().await;
        let reader_pending = state.reader_pending.clone();
        let ready = {
            let mut pending = reader_pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .pending
                .pop_front()
                .or_else(|| state.receiver.try_recv().ok())
                .or_else(|| pending.pop_front())
        };
        if let Some(input) = ready {
            return Some(input);
        }
        state.receiver.recv().await
    }

    pub(in crate::headless) async fn len(&self) -> usize {
        let state = self.state.lock().await;
        let pending = state
            .reader_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.len() + state.receiver.len() + pending.len()
    }

    pub(in crate::headless) async fn close(&self) {
        self.state.lock().await.receiver.close();
    }

    async fn take_prompts(&self) -> Vec<UserTurn> {
        let mut state = self.state.lock().await;
        let mut turns = Vec::new();
        let reader_pending = state.reader_pending.clone();
        let mut reader_pending = reader_pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Native post-tool gather snapshots the queue. New arrivals stay for
        // the next boundary, and continuous input cannot prolong this drain.
        let available = state.pending.len() + state.receiver.len() + reader_pending.len();
        for _ in 0..available {
            let input = state
                .pending
                .pop_front()
                .or_else(|| state.receiver.try_recv().ok())
                .or_else(|| reader_pending.pop_front());
            match input {
                Some(StreamInput::User(turn))
                    if !content_to_prompt(&turn.content).starts_with('/') =>
                {
                    turns.push(turn);
                }
                Some(barrier) => {
                    state.pending.push_front(barrier);
                    break;
                }
                None => break,
            }
        }
        turns
    }
}

#[async_trait]
impl MidTurnInputSource for SdkInputQueue {
    fn admits_at(&self, point: MidTurnInputPoint) -> bool {
        point == MidTurnInputPoint::AfterTools
    }

    async fn take_mid_turn_input(&self) -> Option<String> {
        None
    }

    async fn take_mid_turn_batch(&self) -> Option<Vec<MidTurnInput>> {
        let orchestrator = self.orchestrator.upgrade()?;
        let mut accepted = Vec::new();
        for turn in self.take_prompts().await {
            if let Some(uuid) = turn.uuid.as_deref() {
                if orchestrator.session_contains_message_uuid(uuid).await {
                    if !self.lifecycle.queued.on_duplicate_dequeued(uuid) {
                        continue;
                    }
                    if self.replay {
                        let _ = emit_replay_ack_projected_queued(
                            &self.stream.outbound_tx(),
                            &turn.frame_projection,
                            &self.session_id,
                        );
                    }
                    super::super::run::emit_dedup_skip_terminal(&self.lifecycle, uuid);
                    continue;
                }
            }
            if turn
                .uuid
                .as_deref()
                .is_some_and(|uuid| !self.lifecycle.queued.on_folded(uuid))
            {
                continue;
            }
            accepted.push(turn);
        }
        if accepted.is_empty() {
            return None;
        }
        let inputs = accepted
            .iter()
            .map(|turn| MidTurnInput {
                text: content_to_prompt(&turn.content),
                origin_kind: None,
                projected_content: Some(turn.content_projection.clone()),
                queue_delivery: turn.queue_delivery.clone(),
                source_message_uuid: turn
                    .frame_projection
                    .subprojection("/uuid")
                    .ok()
                    .filter(|uuid| uuid.value.as_str().is_some_and(|uuid| !uuid.is_empty())),
            })
            .collect();
        self.state.lock().await.folded.extend(accepted);
        Some(inputs)
    }

    async fn input_consumed(&self, inputs: &[MidTurnInput]) -> Result<(), String> {
        let turns = std::mem::take(&mut self.state.lock().await.folded);
        for turn in &turns {
            if self.replay {
                emit_replay_ack_projected_queued(
                    &self.stream.outbound_tx(),
                    &turn.frame_projection,
                    &self.session_id,
                )
                .map_err(|error| error.to_string())?;
            }
        }
        for input in inputs {
            if let Some(uuid) = &input.source_message_uuid {
                self.stream.stage_queued_request_marker(uuid.clone())?;
            }
        }
        for (turn, input) in turns.into_iter().zip(inputs) {
            if let Some(uuid) = &input.source_message_uuid {
                self.stream.start_queued_request_marker(uuid.clone())?;
            }
            if let Some(uuid) = turn.uuid.as_deref() {
                self.lifecycle
                    .emit(uuid, LIFECYCLE_STARTED)
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
    use orchestrator::test_support::{
        content_block_start_text, content_block_start_tool_use, content_block_stop,
        input_json_delta, message_delta_stop, message_start, message_stop, noop_hook_executor,
        text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
        StaticMemoryProvider,
    };
    use tokio_util::sync::CancellationToken;

    fn user(id: &str, text: &str) -> StreamInput {
        let frame =
            Utf16JsonProjection::plain(serde_json::json!({"uuid":id,"message":{"content":text}}));
        StreamInput::User(UserTurn {
            queue_delivery: None,
            content: serde_json::json!(text),
            content_projection: frame.subprojection("/message/content").unwrap(),
            frame_projection: frame,
            uuid: Some(id.into()),
        })
    }

    fn text_response() -> Vec<llm_runtime::HistoryEvent> {
        vec![
            message_start("final", "claude-sonnet-5-5"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]
    }

    async fn setup(
        turns: Vec<Vec<llm_runtime::HistoryEvent>>,
    ) -> (
        Arc<SdkInputQueue>,
        mpsc::Sender<StreamInput>,
        Arc<MockStreamingApiClient>,
        Arc<orchestrator::ConversationOrchestrator>,
    ) {
        setup_with_writer(turns, None).await
    }

    async fn setup_with_writer(
        turns: Vec<Vec<llm_runtime::HistoryEvent>>,
        writer: Option<Arc<session::jsonl::JsonlWriter>>,
    ) -> (
        Arc<SdkInputQueue>,
        mpsc::Sender<StreamInput>,
        Arc<MockStreamingApiClient>,
        Arc<orchestrator::ConversationOrchestrator>,
    ) {
        setup_inner(turns, writer, true).await
    }

    async fn setup_inner(
        turns: Vec<Vec<llm_runtime::HistoryEvent>>,
        writer: Option<Arc<session::jsonl::JsonlWriter>>,
        bind: bool,
    ) -> (
        Arc<SdkInputQueue>,
        mpsc::Sender<StreamInput>,
        Arc<MockStreamingApiClient>,
        Arc<orchestrator::ConversationOrchestrator>,
    ) {
        let api = Arc::new(MockStreamingApiClient::with_turns(turns));
        let stream = Arc::new(StreamJsonStream::new_placeholder(
            crate::headless::io::Output::new(tokio::io::sink()),
        ));
        let orch = orchestrator::ConversationOrchestrator::new_with_streaming(
            orchestrator::OrchestratorConfig {
                bare: true,
                ..Default::default()
            },
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        );
        let orch = orchestrator::ConversationOrchestrator::into_shared(match writer {
            Some(writer) => orch.with_jsonl_writer(writer),
            None => orch,
        });
        let session_id = lingxi_core::host::OrchestratorHandle::current_session_id(orch.as_ref())
            .await
            .as_uuid()
            .to_string();
        let lifecycle = Arc::new(QueueLifecycle::new(
            stream.outbound_tx(),
            session_id.clone(),
        ));
        lifecycle
            .bind_journal(orch.session_transcript_writer())
            .unwrap();
        let (tx, rx) = mpsc::channel(64);
        let source = Arc::new(SdkInputQueue::new(
            rx,
            PendingInputQueue::default(),
            lifecycle,
            stream,
            orch.clone(),
            true,
            session_id,
        ));
        if bind {
            orch.set_mid_turn_input(source.clone());
        }
        (source, tx, api, orch)
    }

    #[tokio::test]
    async fn ordinary_completion_keeps_the_next_human_for_a_distinct_query() {
        let (source, tx, api, orch) = setup(vec![text_response()]).await;
        let id = "00000000-0000-4000-8000-000000000032";
        source.lifecycle.queued.on_queued(id);
        tx.send(user(id, "HEADLESS_BATCH_B")).await.unwrap();
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("HEADLESS_BATCH_A")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(api.captured_calls().await.len(), 1);
        assert_eq!(source.len().await, 1);
        assert_eq!(source.lifecycle.queued.still_queued(), [id]);
        assert!(
            matches!(source.recv().await, Some(StreamInput::User(turn)) if turn.uuid.as_deref()==Some(id))
        );
    }

    #[tokio::test]
    async fn actual_tool_continuation_consumes_human_once_and_retains_delivery_identity() {
        let tool = vec![
            message_start("tool", "claude-sonnet-5-5"),
            content_block_start_tool_use(
                0,
                ToolUseId::from("fixture_tool"),
                "UnavailableFixtureTool",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let (source, tx, api, orch) = setup(vec![tool, text_response()]).await;
        let id = "00000000-0000-4000-8000-000000000042";
        let marker = Utf16JsonProjection::plain(serde_json::json!("primary"));
        source
            .stream
            .begin_request_markers(Some(marker.clone()), vec![marker], false)
            .unwrap();
        source.lifecycle.queued.on_queued("primary");
        assert!(source.lifecycle.queued.on_dequeued("primary"));
        source.lifecycle.queued.on_queued(id);
        tx.send(user(id, "HEADLESS_MIDTOOL_B")).await.unwrap();
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("HEADLESS_MIDTOOL_A")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2);
        assert!(!calls[0]
            .messages
            .iter()
            .any(|message| message.text_content().contains("HEADLESS_MIDTOOL_B")));
        let index = calls[1]
            .messages
            .iter()
            .position(|message| message.id().as_uuid().to_string() == id)
            .unwrap();
        let ConversationMessage::User {
            content,
            api_message_override: Some(api_message),
            ..
        } = &calls[1].messages[index]
        else {
            panic!("missing consumed human API projection");
        };
        assert_eq!(*content, api_message.content);
        assert!(
            matches!(&content[0], ContentBlock::Text { text, .. } | ContentBlock::TextJsUtf16 { text, .. } if text.starts_with("The user sent a new message while you were working:\nHEADLESS_MIDTOOL_B\n\nThis is how Claude Code"))
        );
        assert!(calls[1].messages[..index].iter().any(|message| matches!(message,
            ConversationMessage::User { content, .. } if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { .. })))));
        assert_eq!(source.len().await, 0);
        assert_eq!(
            source.lifecycle.queued.take_current_turn_uuids(),
            ["primary", id]
        );
        assert!(source.take_mid_turn_batch().await.is_none());
        let result = source
            .stream
            .build_result_success_frame(
                "done",
                "end_turn",
                &lingxi_core::host::CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        assert_eq!(result["user_message_uuid"], "primary");
        assert_eq!(
            result["user_message_uuids"],
            serde_json::json!(["primary", id])
        );
    }

    #[tokio::test]
    async fn post_tool_batch_retains_each_delivery_and_all_consumed_markers() {
        let tool = vec![
            message_start("tool", "claude-sonnet-5-5"),
            content_block_start_tool_use(
                0,
                ToolUseId::from("fixture_tool"),
                "UnavailableFixtureTool",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let (source, tx, api, orch) = setup(vec![tool, text_response()]).await;
        let first = "00000000-0000-4000-8000-000000000042";
        let last = "00000000-0000-4000-8000-000000000043";
        let marker = Utf16JsonProjection::plain(serde_json::json!("primary"));
        source
            .stream
            .begin_request_markers(Some(marker.clone()), vec![marker], false)
            .unwrap();
        for (id, text) in [(first, "first"), (last, "last")] {
            source.lifecycle.queued.on_queued(id);
            tx.send(user(id, text)).await.unwrap();
        }
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("initial")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2);
        let row = calls[1]
            .messages
            .iter()
            .find(|message| message.id().as_uuid().to_string() == last)
            .unwrap();
        assert!(row.text_content().contains("working:\nlast\n\n"));
        let first_row = calls[1]
            .messages
            .iter()
            .find(|message| message.id().as_uuid().to_string() == first)
            .unwrap();
        assert!(first_row.text_content().contains("working:\nfirst\n\n"));
        let model = llm_runtime::convert::to_llm_messages(
            llm_runtime::convert::normalize_messages_for_api(calls[1].messages.clone()),
        )
        .unwrap();
        let last_message = model.last().unwrap();
        assert_eq!(last_message.role, "system");
        assert_eq!(last_message.content.len(), 1);
        let system_text = match &last_message.content[0] {
            llm_runtime::ContentBlock::Text { text, .. }
            | llm_runtime::ContentBlock::TextJsUtf16 { text, .. } => text,
            _ => panic!("merged system text"),
        };
        assert_eq!(
            *system_text,
            format!("{}\n\n{}", first_row.text_content(), row.text_content())
        );
        let result = source
            .stream
            .build_result_success_frame(
                "done",
                "end_turn",
                &lingxi_core::host::CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        assert_eq!(result["user_message_uuid"], "primary");
        assert_eq!(
            result["user_message_uuids"],
            serde_json::json!(["primary", first, last])
        );
        assert_eq!(
            source.lifecycle.queued.take_current_turn_uuids(),
            [first, last]
        );
    }

    #[tokio::test]
    async fn cancellation_and_history_barriers_preserve_idle_order() {
        let (source, tx, _, _orch) = setup(vec![]).await;
        source.lifecycle.queued.on_queued("cancelled");
        assert_eq!(source.lifecycle.queued.cancel_all_queued(), ["cancelled"]);
        tx.send(user("cancelled", "drop")).await.unwrap();
        tx.send(StreamInput::Bash(
            crate::headless::stream_json_input::BashCommand {
                command: "barrier".into(),
            },
        ))
        .await
        .unwrap();
        tx.send(user("later", "retain")).await.unwrap();
        assert!(source.take_mid_turn_batch().await.is_none());
        assert!(matches!(source.recv().await, Some(StreamInput::Bash(_))));
        assert!(
            matches!(source.recv().await, Some(StreamInput::User(turn)) if turn.uuid.as_deref()==Some("later"))
        );
        assert!(source.lifecycle.queued.take_current_turn_uuids().is_empty());
    }

    #[tokio::test]
    async fn real_driver_persists_native_attachment_and_removal_without_a_user_row() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../scripts/tests/headless-fixtures/native-2.1.293-queued-command-jsonl.json"
        ))
        .unwrap();
        let native: serde_json::Value =
            serde_json::from_str(fixture["rows"][0]["rawLine"].as_str().unwrap()).unwrap();
        let attachment = &native["attachment"];
        let source_id = attachment["source_uuid"].as_str().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.jsonl");
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(
                root.path().to_path_buf(),
            )),
        ));
        let tool = vec![
            message_start("tool", "claude-sonnet-5-5"),
            content_block_start_tool_use(
                0,
                ToolUseId::from("fixture_tool"),
                "UnavailableFixtureTool",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let (source, tx, api, orch) =
            setup_with_writer(vec![tool, text_response()], Some(writer)).await;
        let StreamInput::User(mut turn) = user(source_id, "HEADLESS_MIDTOOL_B") else {
            unreachable!()
        };
        turn.queue_delivery = Some(orchestrator::prompt::mid_turn_input::MidTurnInputDelivery {
            delivery_id: attachment["delivery_id"].as_str().unwrap().into(),
            timestamp: attachment["timestamp"].as_str().unwrap().into(),
            reference_version: "2.1.293".into(),
        });
        source.lifecycle.record_enqueued(&turn);
        source.lifecycle.queued.on_queued(source_id);
        tx.send(StreamInput::User(turn)).await.unwrap();
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("initial")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        let rows = raw
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows[0]["type"], "queue-operation");
        assert_eq!(rows[0]["operation"], "enqueue");
        assert_eq!(rows[0]["content"], attachment["prompt"]);
        assert_eq!(rows[0]["timestamp"], attachment["timestamp"]);
        assert!(rows[0].get("commandUuid").is_none());
        assert!(rows[0].get("deliveryId").is_none());
        let saved = rows
            .iter()
            .find(|row| {
                row.pointer("/attachment/type") == Some(&serde_json::json!("queued_command"))
            })
            .unwrap_or_else(|| panic!("missing queued attachment: {raw}"));
        assert_eq!(saved["attachment"], *attachment);
        assert_eq!(saved["rendered"], native["rendered"]);
        assert_eq!(saved["renderedRole"], native["renderedRole"]);
        assert!(!rows
            .iter()
            .any(|row| row["type"] == "user" && row["uuid"] == source_id));
        let remove = rows
            .iter()
            .find(|row| row["reason"] == "absorbed_mid_turn")
            .unwrap();
        assert_eq!(remove["commandUuid"], source_id);
        assert_eq!(remove["deliveryId"], attachment["delivery_id"]);
        assert_eq!(remove["content"], "HEADLESS_MIDTOOL_B");
        assert_eq!(api.captured_calls().await.len(), 2);
    }

    #[tokio::test]
    async fn actual_router_more_than_channel_capacity_folds_one_snapshot_and_keeps_barrier() {
        let tool = vec![
            message_start("tool", "claude-sonnet-5-5"),
            content_block_start_tool_use(
                0,
                ToolUseId::from("fixture_tool"),
                "UnavailableFixtureTool",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let (source, _tx, api, orch) = setup(vec![tool, text_response()]).await;
        let mut wire = String::new();
        let ids = (1..=75)
            .map(|index| format!("00000000-0000-4000-8000-{index:012}"))
            .collect::<Vec<_>>();
        for (index, id) in ids.iter().enumerate() {
            if index == 70 {
                wire.push_str("{\"type\":\"bash_command\",\"command\":\"barrier\"}\n");
            }
            wire.push_str(&serde_json::json!({"type":"user","uuid":id,"message":{"role":"user","content":format!("queued-{index}")}}).to_string());
            wire.push('\n');
        }
        let crate::headless::stream_json_input::StdinChannels {
            input_rx,
            input_pending,
            mut status,
            reader,
            control_req_rx: _requests,
            control_resp_rx: _responses,
        } = crate::headless::stream_json_input::spawn_stdin_router_from_reader(
            std::io::Cursor::new(wire),
            crate::headless::io::Output::new(tokio::io::sink()),
            true,
            "session".into(),
            source.stream.outbound_tx(),
            source.lifecycle.clone(),
        );
        while *status.borrow_and_update()
            == crate::headless::stream_json_input::StdinReaderStatus::Reading
        {
            status.changed().await.unwrap();
        }
        {
            let mut state = source.state.lock().await;
            state.receiver = input_rx;
            state.reader_pending = input_pending;
        }
        assert_eq!(source.len().await, 76);
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("initial")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2);
        for id in &ids[..70] {
            assert!(calls[1]
                .messages
                .iter()
                .any(|message| message.id().as_uuid().to_string() == *id));
        }
        for id in &ids[70..] {
            assert!(!calls[1]
                .messages
                .iter()
                .any(|message| message.id().as_uuid().to_string() == *id));
        }
        assert_eq!(source.lifecycle.queued.take_current_turn_uuids(), ids[..70]);
        let result = source
            .stream
            .build_result_success_frame(
                "done",
                "end_turn",
                &lingxi_core::host::CostSnapshot::default(),
                "model",
                "off",
                None,
                &[],
            )
            .await;
        assert_eq!(result["user_message_uuids"], serde_json::json!(&ids[..64]));
        assert_eq!(result["user_message_uuid"], ids[63]);
        assert_eq!(source.len().await, 6);
        assert!(matches!(source.recv().await, Some(StreamInput::Bash(_))));
        for id in &ids[70..] {
            assert!(
                matches!(source.recv().await,Some(StreamInput::User(turn)) if turn.uuid.as_ref()==Some(id))
            );
        }
        reader.stop();
        reader.join().await.unwrap();
    }

    struct ArrivalAfterSnapshot {
        source: Arc<SdkInputQueue>,
        late: std::sync::Mutex<Option<StreamInput>>,
    }
    #[async_trait]
    impl MidTurnInputSource for ArrivalAfterSnapshot {
        fn admits_at(&self, point: MidTurnInputPoint) -> bool {
            point == MidTurnInputPoint::AfterTools
        }
        async fn take_mid_turn_input(&self) -> Option<String> {
            None
        }
        async fn take_mid_turn_batch(&self) -> Option<Vec<MidTurnInput>> {
            let batch = self.source.take_mid_turn_batch().await?;
            let late = self.late.lock().unwrap().take();
            if let Some(input) = late {
                let pending = self.source.state.lock().await.reader_pending.clone();
                let lifecycle = self.source.lifecycle.clone();
                tokio::spawn(async move {
                    let mut pending = pending.lock().unwrap();
                    if let StreamInput::User(turn) = &input {
                        lifecycle.queued.on_queued(turn.uuid.as_deref().unwrap());
                    }
                    pending.push_back(input);
                })
                .await
                .unwrap();
            }
            Some(batch)
        }
        async fn input_consumed(&self, inputs: &[MidTurnInput]) -> Result<(), String> {
            self.source.input_consumed(inputs).await
        }
    }

    #[tokio::test]
    async fn concurrent_arrival_after_snapshot_stays_for_the_next_admission() {
        let tool = vec![
            message_start("tool", "claude-sonnet-5-5"),
            content_block_start_tool_use(
                0,
                ToolUseId::from("fixture_tool"),
                "UnavailableFixtureTool",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let (source, tx, api, orch) = setup_inner(vec![tool, text_response()], None, false).await;
        let first = "00000000-0000-4000-8000-000000000042";
        let late = "00000000-0000-4000-8000-000000000043";
        orch.set_mid_turn_input(Arc::new(ArrivalAfterSnapshot {
            source: source.clone(),
            late: std::sync::Mutex::new(Some(user(late, "after snapshot"))),
        }));
        source.lifecycle.queued.on_queued(first);
        tx.send(user(first, "before snapshot")).await.unwrap();
        orch.run_turn_streaming_with_cancel_projected_content(
            &Utf16JsonProjection::plain(serde_json::json!("initial")),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let calls = api.captured_calls().await;
        assert_eq!(calls.len(), 2);
        assert!(calls[1]
            .messages
            .iter()
            .any(|message| message.id().as_uuid().to_string() == first));
        assert!(!calls[1]
            .messages
            .iter()
            .any(|message| message.id().as_uuid().to_string() == late));
        assert_eq!(source.len().await, 1);
        assert_eq!(source.lifecycle.queued.still_queued(), [late]);
        assert!(
            matches!(source.recv().await,Some(StreamInput::User(turn)) if turn.uuid.as_deref()==Some(late))
        );
    }
}
