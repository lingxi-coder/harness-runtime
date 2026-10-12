use super::*;
use crate::prompt::total_tokens::TotalTokensLedger;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::{CurrentUsageSnapshot, OrchestratorHandle};
use std::sync::atomic::Ordering;
use tool_api::registry::ToolRegistry;

fn orchestrator() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

fn seed_usage(orch: &ConversationOrchestrator, used: u64) {
    orch.restore_response_usage(Some(CurrentUsageSnapshot {
        input_tokens: used,
        ..CurrentUsageSnapshot::default()
    }));
}

fn used(orch: &ConversationOrchestrator, current: i64) -> i64 {
    orch.compaction_runtime
        .total_tokens_ledger
        .lock()
        .unwrap()
        .cumulative_used("main", current)
}

fn anchored_usage(orch: &ConversationOrchestrator) {
    orch.compaction_runtime
        .total_tokens_ledger
        .lock()
        .unwrap()
        .reanchor_task_budget("main", 100);
    seed_usage(orch, 1_000);
    assert_eq!(used(orch, 1_000), 900);
}

fn compact_result() -> compaction::IterationCompactionResult {
    compaction::IterationCompactionResult {
        messages: vec![ConversationMessage::user(
            MessageId::new(),
            "summary".into(),
        )],
        layers_applied: vec![compaction::CompactionLayer::Autocompact],
        total_tokens_freed: 1,
        cache_hit: false,
        consecutive_failures: 0,
        was_compacted: true,
        rapid_refill_breaker_tripped: false,
        consecutive_rapid_refills: 0,
        messages_to_preserve: Vec::new(),
        media_analysis_to_preserve: Vec::new(),
        compaction_usage: None,
        compaction_model: None,
        compaction_profile: None,
        raw_summary_text: "test summary".into(),
    }
}

#[test]
fn ledger_matches_source_extracted_286_sequences() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/total_tokens_286_oracle.json"
    ))
    .unwrap();
    assert_eq!(fixture["version"], "2.1.286");
    for case in fixture["cases"].as_array().unwrap() {
        let mut ledger = TotalTokensLedger::default();
        for action in case["actions"].as_array().unwrap() {
            let value = action["used"].as_i64().unwrap();
            let current = match action["type"].as_str().unwrap() {
                "anchor" => {
                    ledger.reanchor_task_budget("main", value);
                    value
                }
                "rollover" => {
                    ledger.roll_over_context("main", value);
                    0
                }
                "usage" => value,
                other => panic!("unknown oracle action: {other}"),
            };
            assert_eq!(
                ledger.cumulative_used("main", current),
                action["expectedUsed"].as_i64().unwrap(),
                "{}: {action}",
                case["name"]
            );
        }
    }
}

#[tokio::test]
async fn automatic_compact_rolls_full_usage_and_zeroes_only_reminder_snapshot() {
    let orch = orchestrator();
    anchored_usage(&orch);
    let applied = orch
        .apply_post_compact(
            compact_result(),
            compaction::CompactTrigger::Auto,
            100,
            1,
            1,
            std::time::Instant::now(),
            None,
        )
        .await;
    assert!(applied.is_some());
    assert_eq!(
        orch.compaction_runtime
            .total_tokens_reminder_usage
            .load(Ordering::Relaxed),
        0,
        "the replacement model context starts without prior assistant usage"
    );
    assert_eq!(
        orch.compaction_runtime
            .last_response_input_tokens
            .load(Ordering::Relaxed),
        1_000,
        "the independent API accounting snapshot keeps its lifetime"
    );
    assert_eq!(used(&orch, 0), 900, "avoid double-counting old context");
    seed_usage(&orch, 200);
    assert_eq!(
        used(&orch, 200),
        1_100,
        "rollover uses full prior usage, rather than estimated tokens freed"
    );
}

#[tokio::test]
async fn manual_and_cancelled_compacts_do_not_roll_the_budget() {
    for (trigger, cancelled) in [
        (compaction::CompactTrigger::Manual, false),
        (compaction::CompactTrigger::Auto, true),
    ] {
        let orch = orchestrator();
        anchored_usage(&orch);
        let cancel = tokio_util::sync::CancellationToken::new();
        if cancelled {
            cancel.cancel();
        }
        let applied = orch
            .apply_post_compact(
                compact_result(),
                trigger,
                100,
                1,
                1,
                std::time::Instant::now(),
                Some(&cancel),
            )
            .await;
        assert_eq!(applied.is_none(), cancelled);
        assert_eq!(
            orch.compaction_runtime
                .total_tokens_reminder_usage
                .load(Ordering::Relaxed),
            if cancelled { 1_000 } else { 0 }
        );
        assert_eq!(used(&orch, 200), 900);
    }
}

#[tokio::test]
async fn clear_rolls_usage_and_hot_resume_retains_the_root_ledger() {
    let orch = orchestrator();
    anchored_usage(&orch);
    OrchestratorHandle::clear_session(&orch).await.unwrap();
    assert_eq!(
        orch.compaction_runtime
            .total_tokens_reminder_usage
            .load(Ordering::Relaxed),
        0
    );
    assert_eq!(used(&orch, 0), 900);
    seed_usage(&orch, 200);
    assert_eq!(used(&orch, 200), 1_100);
    OrchestratorHandle::resume_session(
        &orch,
        lingxi_core::types::SessionId::new(),
        Vec::new(),
        None,
        None,
        lingxi_core::host::ResumeRuntimeSnapshot {
            current_usage: Some(CurrentUsageSnapshot {
                input_tokens: 300,
                ..CurrentUsageSnapshot::default()
            }),
            ..lingxi_core::host::ResumeRuntimeSnapshot::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(used(&orch, 300), 1_200);
    assert_eq!(used(&orchestrator(), 300), 300, "cold roots start fresh");
}

struct OnceTaskNotifications(
    std::sync::Mutex<Vec<lingxi_core::host::task_registry::TaskNotification>>,
);

#[async_trait::async_trait]
impl crate::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct CompactDuringPreparation;

#[async_trait::async_trait]
impl ModelCallPreparer for CompactDuringPreparation {
    async fn prepare(
        &self,
        orch: &ConversationOrchestrator,
        _path: ModelCallPath,
        _system_prompt: Option<
            &lingxi_llm_client::providers::anthropic::system_prompt::SystemPromptInput,
        >,
        _cancel: Option<&CancellationToken>,
        mut draft: PreparedModelCall,
    ) -> Result<PreparedModelCall, OrchestratorError> {
        assert!(orch
            .apply_post_compact(
                compact_result(),
                compaction::CompactTrigger::Auto,
                100,
                1,
                1,
                std::time::Instant::now(),
                None,
            )
            .await
            .is_some());
        draft.history_snapshot = orch.session.lock().await.model_context_history();
        Ok(draft)
    }
}

#[tokio::test]
async fn prompt_origin_survives_compaction_and_durable_task_notifications_in_both_paths() {
    use crate::prompt::total_tokens as tt;
    use lingxi_core::types::{ContentBlock, ToolUseId};

    for path in [ModelCallPath::Batched, ModelCallPath::Streaming] {
        for fresh_human in [false, true] {
            let notification = lingxi_core::host::task_registry::TaskNotification {
                task_id: "b12345678".into(),
                task_type: "local_bash".into(),
                status: "completed".into(),
                description: "run tests".into(),
                exit_code: Some(0),
                ..Default::default()
            };
            let orch = orchestrator()
                .with_model_call_preparer(Arc::new(CompactDuringPreparation))
                .with_task_notifications(Arc::new(OnceTaskNotifications(std::sync::Mutex::new(
                    vec![notification],
                ))));
            anchored_usage(&orch);
            {
                let mut session = orch.session.lock().await;
                let incoming = if fresh_human {
                    ConversationMessage::user(MessageId::new(), "new human prompt".into())
                } else {
                    ConversationMessage::User {
                        api_message_override: None,
                        id: MessageId::new(),
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: ToolUseId::from("tu_before_compact"),
                            content: "tool completed".into(),
                            is_error: Some(false),
                            provider_tool_use_id: None,
                            content_blocks: None,
                        }],
                        is_meta: false,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
                        turn_companion: None,
                        usage_limit_note: None,
                        origin: None,
                        turn_origin: None,
                    }
                };
                session.history.push(incoming);
                session.history.push(ConversationMessage::user_meta(
                    MessageId::new(),
                    "previous hook context".into(),
                ));
            }
            let regular_user = orch.regular_user_prompt_for_model_step().await;
            assert_eq!(regular_user, fresh_human);
            let prepared = orch
                .prepare_turn_step(path, None, true, regular_user, None)
                .await
                .unwrap();
            let notification = prepared
                .snapshot
                .iter()
                .position(|message| message.text_content().contains("b12345678"))
                .expect("durable task notification reaches the request");
            let total = prepared
                .snapshot
                .iter()
                .position(|message| message.text_content().contains("<total_tokens>"))
                .expect("total-token reminder reaches the request");
            assert!(notification < total);
            let consumed = if fresh_human { 0 } else { 900 };
            assert_eq!(used(&orch, 0), consumed);
            let remaining = i64::try_from(tt::resolve_budget(None)).unwrap() - consumed;
            assert_eq!(
                prepared.snapshot[total].text_content(),
                format!(
                    "<system-reminder>\n{}\n</system-reminder>",
                    tt::format_total_tokens(tt::TotalTokensMode::PaddedCountdown, remaining)
                ),
                "{path:?}: fresh_human={fresh_human}"
            );
        }
    }
}

#[tokio::test]
async fn task_notification_attachment_replays_from_jsonl_with_native_identity_and_order() {
    use lingxi_core::types::utf16_json::Utf16JsonProjection as Source;
    use lingxi_core::types::{ContentBlock, MessageId};
    use platform_posix::fs::PosixFileSystem;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-notification.jsonl");
    let writer = Arc::new(session::jsonl::JsonlWriter::new(
        path.clone(),
        Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
    ));

    let mut description_units = "run ".encode_utf16().collect::<Vec<_>>();
    description_units.push(0xd800);
    description_units.extend(" and </system-reminder>".encode_utf16());
    let description = Source::root_string(
        String::from_utf16_lossy(&description_units),
        description_units,
    )
    .unwrap();
    let notification = lingxi_core::host::task_registry::TaskNotification {
        task_id: "b12345678".into(),
        task_type: "local_bash".into(),
        status: "completed".into(),
        description,
        exit_code: Some(0),
        ..Default::default()
    };
    let warm = orchestrator()
        .with_jsonl_writer(writer)
        .with_task_notifications(Arc::new(OnceTaskNotifications(std::sync::Mutex::new(
            vec![notification.clone()],
        ))));
    let assistant = ConversationMessage::Assistant {
        per_turn_effort: None,
        id: MessageId::new(),
        content: vec![ContentBlock::Text {
            text: "prior assistant turn".into(),
            citations: None,
        }],
        stop_reason: Some("end_turn".into()),
        resumable: None,
    };
    let previous_meta =
        ConversationMessage::user_meta(MessageId::new(), "previous hook context".into());
    {
        let mut session = warm.session.lock().await;
        session.history.push(assistant.clone());
        session.history.push(previous_meta.clone());
    }
    warm.persist_message_to_jsonl(&assistant).await;
    warm.persist_message_to_jsonl(&previous_meta).await;
    let hot = warm
        .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
        .await
        .unwrap();
    assert!(hot
        .snapshot
        .iter()
        .any(|message| message.text_content().contains("b12345678")));

    let raw = std::fs::read_to_string(&path).unwrap();
    let rows = raw
        .lines()
        .map(|line| serde_json::from_str::<session::JsonlMessage>(line).unwrap())
        .collect::<Vec<_>>();
    let task_row = rows
        .iter()
        .find(|row| {
            row.attachment_projection
                .as_ref()
                .is_some_and(|attachment| {
                    attachment.string_at("/type") == Some("queued_command")
                        && attachment.string_at("/commandMode") == Some("task-notification")
                })
        })
        .expect("the notification is persisted as a typed queued-command attachment");
    assert_eq!(task_row.message_type, "attachment");
    let outer_uuid = uuid::Uuid::parse_str(&task_row.uuid).unwrap();
    let hot_message = hot
        .snapshot
        .iter()
        .find(|message| message.text_content().contains("b12345678"))
        .expect("hot request contains the persisted notification message");
    assert_eq!(hot_message.id().as_uuid(), outer_uuid);
    let attachment = task_row.attachment_projection.as_ref().unwrap();
    let source_uuid = attachment.string_at("/source_uuid").unwrap();
    let delivery_id = attachment.string_at("/delivery_id").unwrap();
    assert!(uuid::Uuid::parse_str(source_uuid).is_ok());
    assert!(uuid::Uuid::parse_str(delivery_id).is_ok());
    assert_ne!(source_uuid, delivery_id);
    assert_ne!(source_uuid, task_row.uuid.as_str());
    assert_ne!(delivery_id, task_row.uuid.as_str());
    assert_eq!(task_row.extra.string_at("/renderedRole"), Some("system"));

    let (expected_prompt, expected_rendered) =
        crate::prompt::task_notification::render_reminder_sources_in_turn(
            std::slice::from_ref(&notification),
            true,
        )
        .pop()
        .unwrap();
    assert_eq!(
        attachment.string_units("/prompt"),
        expected_prompt.string_units("")
    );
    assert!(expected_prompt.string_units("").unwrap().contains(&0xd800));
    let rendered = task_row.extra.subprojection("/rendered/0/content").unwrap();
    assert_eq!(
        rendered.string_units(""),
        expected_rendered.string_units("")
    );
    assert!(rendered.string_units("").unwrap().contains(&0xd800));

    // Keep the restored transcript intact for this cold-resume ordering check;
    // the separate hot-path test above covers compaction's replacement-history
    // behavior in both model-call paths.
    let cold = orchestrator();
    let session_uuid = uuid::Uuid::parse_str(&rows[0].session_id).unwrap();
    *cold.session.lock().await = crate::resume::state_from_messages(session_uuid, &rows);
    cold.restore_resume_runtime_metadata(&rows).await;
    let notification_id = MessageId::from_uuid(outer_uuid);
    let sidecar = cold
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .get(&notification_id)
        .cloned()
        .expect("cold resume restores the typed payload under the outer message ID");
    assert_eq!(sidecar.string_at("/commandMode"), Some("task-notification"));
    assert_eq!(sidecar.string_at("/source_uuid"), Some(source_uuid));
    assert_eq!(sidecar.string_at("/delivery_id"), Some(delivery_id));
    let resumed_message = cold
        .session
        .lock()
        .await
        .history
        .iter()
        .find(|message| message.id() == notification_id)
        .cloned()
        .expect("cold resume restores the model-facing system-role row");
    let resumed_units = match &resumed_message {
        ConversationMessage::User { content, .. } => match content.first().unwrap() {
            ContentBlock::TextJsUtf16 {
                utf16_code_units, ..
            } => utf16_code_units.clone(),
            other => panic!("cold task-notification row lost UTF-16 source: {other:?}"),
        },
        other => panic!("cold task-notification row has wrong role: {other:?}"),
    };
    assert_eq!(Some(resumed_units), expected_rendered.string_units(""));

    anchored_usage(&cold);
    let cold_prepared = cold
        .prepare_turn_step(ModelCallPath::Batched, None, true, false, None)
        .await
        .unwrap();
    let notification_index = cold_prepared
        .snapshot
        .iter()
        .position(|message| message.id() == notification_id)
        .expect("cold prepared request keeps the notification row");
    let total_index = cold_prepared
        .snapshot
        .iter()
        .position(|message| message.text_content().contains("<total_tokens>"))
        .expect("cold preparation emits the typed total-token reminder");
    assert!(
        notification_index < total_index,
        "cold full-history repacking keeps the task notification before later reminders"
    );
}
