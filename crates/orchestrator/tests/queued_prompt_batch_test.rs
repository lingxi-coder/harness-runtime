//! A queue batch keeps per-message origin metadata and runs one model turn.
use lingxi_core::host::FileSystem;
use lingxi_core::types::{ConversationMessage, MessageId};
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
use platform_posix::fs::PosixFileSystem;
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
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new_with_streaming(
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
            .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
        );
        let ids = [MessageId::new(), MessageId::new()];
        let inputs: Vec<_> = flags
            .iter()
            .enumerate()
            .map(|(i, is_meta)| QueuedPromptInput {
                goal_retry_id: None,
                text: format!("queued text {i}"),
                is_meta: *is_meta,
                mod_origin: Some(if *is_meta {
                    serde_json::json!({"kind":"scheduled-trigger"})
                } else {
                    serde_json::json!({"kind":"bridge"})
                }),
                message_id: Some(ids[i]),
                transcript_row_token: None,
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
            .find(|message| {
                matches!(message, ConversationMessage::User { .. })
                    && message.text_content() == "later human"
            })
            .is_some_and(|message| !message.is_meta()));
    }
}

#[tokio::test]
async fn prompt_submit_mod_screens_each_batch_entry_before_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let module = dir.path().join("submit.js");
    std::fs::write(
        &module,
        r#"
        export function register(on) {
          on('prompt.submit', ($, e, next) => {
            if (e.origin.kind === 'plugin') return { drop: 'plugin refused' };
            if (e.origin.kind === 'bridge') {
              return next({ ...e, text: 'rewritten bridge', context: ['batch note'] });
            }
            return next(e);
          });
        }
    "#,
    )
    .unwrap();
    let host = hooks::mods::ModHost::start(None).await.unwrap();
    host.load("submit", dir.path(), &module, serde_json::json!({}))
        .await
        .unwrap();
    let mut registry = hooks::HookRegistry::new();
    registry.set_mod_host(host);
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![scripted![
        message_start("msg_submit", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "done"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]]));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
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
        .with_hook_registry(Arc::new(tokio::sync::RwLock::new(registry)))
        .with_jsonl_writer(Arc::new(JsonlWriter::new(path.clone(), fs.clone()))),
    );
    let inputs = vec![
        QueuedPromptInput {
            text: "typed bridge".into(),
            mod_origin: Some(serde_json::json!({"kind":"bridge"})),
            ..Default::default()
        },
        QueuedPromptInput {
            text: "timer".into(),
            is_meta: true,
            mod_origin: Some(serde_json::json!({"kind":"scheduled-trigger"})),
            ..Default::default()
        },
        QueuedPromptInput {
            text: "plugin text".into(),
            is_meta: true,
            mod_origin: Some(serde_json::json!({"kind":"plugin","name":"source"})),
            ..Default::default()
        },
    ];
    orch.run_queued_prompt_batch(inputs, CancellationToken::new())
        .await
        .unwrap();
    let calls = api.captured_calls().await;
    assert_eq!(calls.len(), 1);
    let model_text: Vec<_> = calls[0]
        .messages
        .iter()
        .map(ConversationMessage::text_content)
        .collect();
    let rewritten = model_text
        .iter()
        .position(|text| text == "rewritten bridge")
        .unwrap();
    assert!(model_text[rewritten + 1].contains("prompt.submit hook additional context: batch note"));
    assert_eq!(model_text[rewritten + 2], "timer");
    assert!(!model_text
        .iter()
        .any(|text| text.contains("plugin text") || text.contains("typed bridge")));
    let records = JsonlReader::new(path, fs).read_all().await.unwrap();
    assert_eq!(records[0].message_type, "user");
    assert!(serde_json::to_string(&records[0])
        .unwrap()
        .contains("rewritten bridge"));
    assert_eq!(records[1].message_type, "attachment");
    assert_eq!(records[1].extra["attachment"]["hookName"], "prompt.submit");
    assert_eq!(records[2].message_type, "user");
    assert!(serde_json::to_string(&records[2])
        .unwrap()
        .contains("timer"));
    assert!(!serde_json::to_string(&records)
        .unwrap()
        .contains("plugin text"));
}
