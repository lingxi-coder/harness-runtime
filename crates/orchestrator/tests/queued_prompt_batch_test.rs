//! A queue batch keeps per-message origin metadata and runs one model turn.
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use orchestrator::{
    scripted, ConversationOrchestrator, OrchestratorConfig, QueuedPromptInput, TurnOutcome,
};
use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use protocol::{ConversationMessage, MessageId};
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn queued_batch_preserves_each_meta_flag_uuid_and_jsonl_parent_in_one_turn() {
    for flags in [[false, true], [true, false], [true, true]] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
        let stream = || {
            scripted![
                message_start("msg_batch", "claude-opus-4-7"),
                content_block_start_text(0),
                text_delta(0, "done"),
                content_block_stop(0),
                message_delta_stop("end_turn"),
                message_stop(),
            ]
        };
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream(), stream()]));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().to_path_buf(),
        )
        .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone())));
        let ids = [MessageId::new(), MessageId::new()];
        let inputs: Vec<_> = flags
            .iter()
            .enumerate()
            .map(|(i, is_meta)| QueuedPromptInput {
                goal_retry_id: None,
                text: format!("queued text {i}"),
                is_meta: *is_meta,
                message_id: Some(ids[i]),
                queue_priority: is_meta.then(|| "later".into()),
                scheduled_task_id: is_meta.then(|| format!("task-{i}")),
                scheduled_fire_id: Some(format!("fire-{i}")),
            })
            .collect();

        assert!(matches!(
            orch.run_queued_prompt_batch(vec![], CancellationToken::new())
                .await
                .unwrap(),
            TurnOutcome::EndTurn
        ));
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            orch.run_queued_prompt_batch(inputs.clone(), cancelled)
                .await
                .unwrap(),
            TurnOutcome::Cancelled
        ));
        assert!(orch.snapshot_history().await.is_empty());
        assert!(api.captured_calls().await.is_empty());

        assert!(matches!(
            orch.run_queued_prompt_batch(inputs, CancellationToken::new())
                .await
                .unwrap(),
            TurnOutcome::EndTurn
        ));
        assert_eq!(
            api.captured_calls().await.len(),
            1,
            "one model request for the whole batch"
        );
        let history = orch.snapshot_history().await;
        for (i, expected) in flags.iter().enumerate() {
            assert!(
                matches!(&history[i], ConversationMessage::User { id, is_meta, .. } if *id == ids[i] && is_meta == expected)
            );
        }
        let records = JsonlReader::new(path.clone(), fs.clone())
            .read_all()
            .await
            .unwrap();
        assert_eq!(records[0].uuid, ids[0].as_uuid().to_string());
        assert_eq!(records[1].uuid, ids[1].as_uuid().to_string());
        assert_eq!(
            records[1].parent_uuid.as_deref(),
            Some(records[0].uuid.as_str())
        );
        for (i, is_meta) in flags.iter().enumerate() {
            assert_eq!(
                records[i]
                    .extra
                    .get("queuePriority")
                    .and_then(serde_json::Value::as_str),
                if *is_meta { Some("later") } else { None }
            );
        }
        assert!(
            !serde_json::to_string(&api.captured_calls().await[0].messages)
                .unwrap()
                .contains("queuePriority")
        );
        for (i, is_meta) in flags.iter().enumerate() {
            assert_eq!(
                records[i]
                    .extra
                    .get("scheduledTaskId")
                    .and_then(serde_json::Value::as_str),
                is_meta.then(|| format!("task-{i}")).as_deref()
            );
            assert_eq!(
                records[i]
                    .extra
                    .get("scheduledFireId")
                    .and_then(serde_json::Value::as_str),
                is_meta.then(|| format!("fire-{i}")).as_deref(),
                "fire id without task id stays absent"
            );
        }
        for (i, expected) in flags.iter().enumerate() {
            assert_eq!(
                records[i]
                    .extra
                    .get("isMeta")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false),
                *expected
            );
        }

        // No per-turn override can leak out of an all-meta or mixed batch.
        orch.run_turn_streaming_with_cancel("later human", CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(api.captured_calls().await.len(), 2);
        let after = JsonlReader::new(path, fs).read_all().await.unwrap();
        assert!(after
            .iter()
            .rev()
            .find(|row| row.message_type == "user")
            .unwrap()
            .extra
            .get("queuePriority")
            .is_none());
        let ordinary = after
            .iter()
            .rev()
            .find(|row| row.message_type == "user")
            .unwrap();
        assert!(!ordinary.extra.contains_key("scheduledTaskId"));
        assert!(!ordinary.extra.contains_key("scheduledFireId"));
        let model_messages =
            serde_json::to_string(&api.captured_calls().await[0].messages).unwrap();
        assert!(
            !model_messages.contains("scheduledTaskId")
                && !model_messages.contains("scheduledFireId")
        );
        let later = orch.snapshot_history().await;
        assert!(later
            .iter()
            .rev()
            .find(|message| matches!(message, ConversationMessage::User { .. }))
            .is_some_and(|message| !message.is_meta()));
    }
}
