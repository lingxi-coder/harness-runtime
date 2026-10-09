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
        _system_prompt: Option<&str>,
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
                    ConversationMessage::User { api_message_override: None,
                        id: MessageId::new(),
                        content: vec![ContentBlock::ToolResult { content_projection: None,
                            tool_use_id: ToolUseId::from("tu_before_compact"),
                            content: "tool completed".into(),
                            is_error: Some(false),
                            provider_tool_use_id: None,
                            content_blocks: None,
                        }],
                        is_meta: false,
                        is_compact_summary: false,
                        is_visible_in_transcript_only: false,
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
