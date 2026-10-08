use super::*;
use crate::prompt::MemoryFile;
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use lingxi_core::types::ContentBlock;
use serde_json::{json, Value};
use tool_api::registry::ToolRegistry;

#[tokio::test]
async fn second_turn_releases_snapshot_lock_before_context_routing() {
    let dir = tempfile::tempdir().unwrap();
    let api = Arc::new(MockApiClient::new(
        (0..2)
            .map(|_| {
                mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "answer".into(),
                        cache_control: None, citations: None,
                    }],
                    Some("end_turn"),
                )
            })
            .collect(),
    ));
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.path().to_owned(),
    );
    orch.run_turn("first").await.unwrap();
    assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_some());
    tokio::time::timeout(std::time::Duration::from_secs(1), orch.run_turn("second"))
        .await
        .expect("second turn must not retain the routing mutex")
        .unwrap();
    let systems = api.captured_systems().await;
    assert_eq!(systems.len(), 2);
    assert_eq!(
        systems[0], systems[1],
        "the frozen static prompt reaches both actual model calls"
    );
}

#[tokio::test]
async fn cold_valid_session_context_freezes_missing_git_but_invalid_latest_uses_host_probe() {
    let dir = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(std::process::Command::new("git")
        .args([
            "-c",
            "user.name=Context Fixture",
            "-c",
            "user.email=context@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "fixture"
        ])
        .current_dir(dir.path())
        .status()
        .unwrap()
        .success());
    let fixture = oracle();
    for name in [
        "valid-empty-session-context-freezes-missing-git",
        "invalid-latest-context-fetches-host-without-older-fallback",
        "invalid-latest-git-value-fetches-host",
    ] {
        let case = fixture["gitSnapshotCases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap();
        let expects_git = case["expected"]["announced"]["gitStatus"].is_string();
        let orch = orchestrator(Vec::new(), dir.path());
        assert!(
            orch.cached_git_status(dir.path()).await.1.is_some(),
            "the host can provide gitStatus"
        );
        let mut payloads = case["prior"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["attachment"].clone())
            .collect::<Vec<_>>();
        payloads.push(json!({"type":"date","date":crate::prompt::env_meta::current_date_string()}));
        let rows = payloads
            .into_iter()
            .map(|attachment| {
                let message =
                    crate::conversation::context_announcements_impl::context_attachment_projection(
                        MessageId::new(),
                        &attachment,
                    );
                let mut row = orch.to_jsonl_message(
                    &message,
                    &uuid::Uuid::nil().to_string(),
                    None,
                    None,
                    None,
                    None,
                );
                row.message_type = "attachment".into();
                row.message = Value::Null;
                row.extra.clear();
                row.extra.insert("attachment".into(), attachment);
                row
            })
            .collect::<Vec<_>>();
        *orch.session.lock().await = crate::resume::state_from_messages(uuid::Uuid::nil(), &rows);
        orch.restore_resume_runtime_metadata(&rows).await;
        orch.session
            .lock()
            .await
            .history
            .push(ConversationMessage::user(MessageId::new(), "hello".into()));
        let prepared = orch
            .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
            .await
            .unwrap();
        let new_rows = orch.context_attachment_history(&prepared.context_announcements.messages);
        if expects_git {
            assert_eq!(
                new_rows.len(),
                1,
                "invalid latest context has no older valid fallback"
            );
            assert_eq!(new_rows[0]["type"], "session_context");
            assert!(new_rows[0]["context"]["gitStatus"].is_string());
            assert_ne!(new_rows[0]["context"]["gitStatus"], json!("old git"));
        } else {
            assert!(
                new_rows.is_empty(),
                "valid prior empty context freezes absent gitStatus"
            );
            let request = llm_runtime::convert::normalize_messages_for_api(prepared.snapshot);
            assert!(!request
                .iter()
                .filter_map(body)
                .any(|body| body.contains("# gitStatus\n")));
        }
    }
}

fn oracle() -> Value {
    // Native fixtures keep their captured bytes; project the product instruction key here.
    serde_json::from_str(
        &include_str!("../../../../core/tests/fixtures/instruction_announcements_2_1_286.json")
            .replace("claudeMd", "instructions"),
    )
    .unwrap()
}

fn orchestrator(files: Vec<MemoryFile>, cwd: &std::path::Path) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::with_files(files)),
        cwd.to_owned(),
    )
}

fn fixture_files(case: &Value) -> Vec<MemoryFile> {
    case["current"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            let tier = match file["type"].as_str().unwrap() {
                "Managed" => memory::lingxi_md::LingxiMdTier::Managed,
                "User" => memory::lingxi_md::LingxiMdTier::User,
                "Project" => memory::lingxi_md::LingxiMdTier::Project,
                "Local" => memory::lingxi_md::LingxiMdTier::Local,
                _ => unreachable!(),
            };
            MemoryFile {
                source_content: None,
                parent: None,
                path: file["path"].as_str().unwrap().into(),
                body: file["content"].as_str().unwrap().into(),
                is_local_override: tier == memory::lingxi_md::LingxiMdTier::Local,
                tier,
                globs: None,
                raw_content: file["content"].as_str().unwrap().into(),
                content_differs_from_disk: false,
            }
        })
        .collect()
}

fn body(message: &ConversationMessage) -> Option<String> {
    match message {
        ConversationMessage::User { content, .. } => Some(
            content
                .iter()
                .filter_map(ContentBlock::visible_text)
                .collect(),
        ),
        _ => None,
    }
}

#[tokio::test]
async fn prepared_main_request_and_raw_jsonl_match_native_initial_announcements() {
    let fixture = oracle();
    let case = &fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "initial-four-file-kinds")
        .unwrap();
    for path in [ModelCallPath::Batched, ModelCallPath::Streaming] {
        let dir = tempfile::tempdir().unwrap();
        let orch = orchestrator(fixture_files(case), dir.path());
        let user = ConversationMessage::user(MessageId::new(), "hello".into());
        orch.session.lock().await.history.push(user.clone());
        let prepared = orch
            .prepare_turn_step(path, None, true, true, None)
            .await
            .unwrap();
        let messages = &prepared.context_announcements.messages;
        assert_eq!(messages.len(), 3);
        assert_eq!(prepared.snapshot[0], user);
        assert_eq!(&prepared.snapshot[1..4], messages);
        assert!(prepared.date_change_reminder.is_none());
        let today = crate::prompt::env_meta::current_date_string();
        let mut rows = vec![orch.to_jsonl_message(
            &user,
            &uuid::Uuid::nil().to_string(),
            None,
            None,
            None,
            None,
        )];
        for (message, expected) in messages.iter().zip(case["expected"].as_array().unwrap()) {
            let row = orch.to_jsonl_message(
                message,
                &uuid::Uuid::nil().to_string(),
                None,
                None,
                None,
                None,
            );
            let mut expected_attachment = expected["attachment"].clone();
            let expected_bodies = expected["rendered"].as_array().unwrap();
            if expected_attachment["type"] == "date" {
                expected_attachment["date"] = json!(today);
            }
            assert_eq!(row.message_type, "attachment");
            assert!(row.message.is_null());
            assert_eq!(row.extra["attachment"], expected_attachment);
            assert_eq!(row.uuid, message.id().as_uuid().to_string());
            if expected_bodies.is_empty() {
                assert!(
                    matches!(message, ConversationMessage::System { content, subtype, .. }
                    if content.is_empty() && subtype.as_deref() == Some("model_reminder_attachment"))
                );
                assert_eq!(
                    compaction::grouping::estimate_tokens_for_range(std::slice::from_ref(message)),
                    0
                );
            } else {
                let expected_body = expected_bodies[0]
                    .as_str()
                    .unwrap()
                    .replace("2026-09-30", &today);
                assert_eq!(body(message).as_deref(), Some(expected_body.as_str()));
            }
            rows.push(row);
        }
        // Retry uses original bytes/IDs even after the host's eager snapshot
        // changes; no second producer invocation is needed.
        let mut retry = orch.session.lock().await.model_context_history();
        let mut turn_reminders = prepared.turn_reminders.clone();
        let mut guarded_async_hook_reminders = prepared.guarded_async_hook_reminders.clone();
        orch.reattach_outgoing_context(
            &mut retry,
            prepared.deferred_reminder.as_ref(),
            prepared.date_change_reminder.as_ref(),
            &mut turn_reminders,
            &mut guarded_async_hook_reminders,
            &prepared.context_announcements,
            false,
        )
        .await;
        assert_eq!(retry, prepared.snapshot);
        // The ordinary producer also persisted the stock total-token suffix.
        // Replay the whole durable step, including that attachment.
        assert_eq!(prepared.turn_reminders.len(), 1);
        let total_tokens = &prepared.turn_reminders[0];
        let total_row = orch.to_jsonl_message(
            total_tokens,
            &uuid::Uuid::nil().to_string(),
            None,
            None,
            None,
            None,
        );
        assert_eq!(
            total_row.extra["attachment"]["type"],
            "total_tokens_reminder"
        );
        rows.push(total_row);
        let cold = orchestrator(fixture_files(case), dir.path());
        let decoded: Vec<session::JsonlMessage> =
            serde_json::from_str(&serde_json::to_string(&rows).unwrap()).unwrap();
        *cold.session.lock().await =
            crate::resume::state_from_messages(uuid::Uuid::nil(), &decoded);
        cold.restore_resume_runtime_metadata(&decoded).await;
        assert_eq!(
            cold.session.lock().await.history,
            orch.session.lock().await.history
        );
        assert!(
            cold.context_announcement_messages().await.is_empty(),
            "cold raw empty anchor must dedupe"
        );
        let captured = cold.instruction_context_snapshot().await;
        assert_eq!(captured.announcement_history.len(), 4);
        assert_eq!(
            captured.announcement_history[3]["type"],
            "total_tokens_reminder"
        );
    }
}

#[tokio::test]
async fn successful_empty_provider_and_compaction_use_actual_durable_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let orch = orchestrator(Vec::new(), dir.path());
    let initial = orch.context_announcement_messages().await;
    assert_eq!(
        initial.len(),
        2,
        "known empty files: session_context then date"
    );
    assert!(orch.context_announcement_messages().await.is_empty());
    // History replacement drops the zero projection along with the old chain;
    // the side table alone must never suppress a new chain's initial rows.
    orch.session.lock().await.history = vec![ConversationMessage::user(
        MessageId::new(),
        "summary".into(),
    )];
    let reannounced = orch.context_announcement_messages().await;
    assert_eq!(reannounced.len(), 2);
    assert!(reannounced
        .iter()
        .zip(initial.iter())
        .all(|(new, old)| new.id() != old.id()));
}

#[tokio::test]
async fn ptl_history_replacement_reannounces_frozen_files_without_resurrecting_old_ids() {
    let fixture = oracle();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "initial-four-file-kinds")
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let orch = orchestrator(fixture_files(case), dir.path());
    orch.session
        .lock()
        .await
        .history
        .push(ConversationMessage::user(MessageId::new(), "hello".into()));
    let prepared = orch
        .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
        .await
        .unwrap();
    let old_ids = prepared
        .context_announcements
        .messages
        .iter()
        .map(ConversationMessage::id)
        .collect::<HashSet<_>>();
    let replacement = vec![ConversationMessage::user(
        MessageId::new(),
        "compacted summary".into(),
    )];
    orch.session
        .lock()
        .await
        .replace_model_context_history(replacement.clone());
    let mut retry = replacement;
    let mut turn_reminders = prepared.turn_reminders.clone();
    let mut guarded_async_hook_reminders = prepared.guarded_async_hook_reminders.clone();
    orch.reattach_outgoing_context(
        &mut retry,
        None,
        None,
        &mut turn_reminders,
        &mut guarded_async_hook_reminders,
        &prepared.context_announcements,
        true,
    )
    .await;
    assert!(retry.iter().all(|message| !old_ids.contains(&message.id())));
    let current = orch.instruction_context_snapshot().await;
    let payloads = current
        .announcement_history
        .iter()
        .filter(|attachment| {
            matches!(
                attachment["type"].as_str(),
                Some("instructions" | "session_context" | "date")
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(payloads.len(), 3);
    assert_eq!(
        payloads[0]["files"],
        case["expected"][0]["attachment"]["files"]
    );
    assert!(orch
        .context_announcements_from_frozen_snapshot()
        .await
        .is_empty());
}

#[tokio::test]
async fn cold_raw_snapshot_routing_matches_native_and_invalid_latest_cannot_revive_older_inline() {
    let fixture = oracle();
    for name in [
        "valid-inline-empty-system-prompt",
        "invalid-latest-does-not-fall-back-to-older-inline",
        "invalid-context-rendering-is-caught-undefined",
    ] {
        let case = fixture["snapshotCases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["name"] == name)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let orch = orchestrator(Vec::new(), dir.path());
        let rows = case["prior"]
            .as_array()
            .unwrap()
            .iter()
            .map(|prior| {
                let metadata = ConversationMessage::user_meta(MessageId::new(), String::new());
                let mut row = orch.to_jsonl_message(
                    &metadata,
                    &uuid::Uuid::nil().to_string(),
                    None,
                    None,
                    None,
                    None,
                );
                row.message_type = "attachment".into();
                row.message = Value::Null;
                row.extra.clear();
                row.extra
                    .insert("attachment".into(), prior["attachment"].clone());
                row
            })
            .collect::<Vec<_>>();
        orch.restore_resume_runtime_metadata(&rows).await;
        let expected_inline = case["expected"]["selectedRendering"] == "inline";
        assert_eq!(
            !orch.uses_announced_context().await,
            expected_inline,
            "{name}"
        );
        let original = ConversationMessage::user(MessageId::new(), "hello".into());
        orch.session.lock().await.history.push(original.clone());
        let prepared = orch
            .prepare_turn_step(ModelCallPath::Batched, None, true, true, None)
            .await
            .unwrap();
        if expected_inline {
            assert!(prepared.context_announcements.messages.is_empty());
            assert!(body(&prepared.snapshot[0])
                .unwrap()
                .contains("# currentDate\n"));
            assert_eq!(prepared.snapshot[1], original);
            assert_eq!(prepared.turn_reminders.len(), 1);
            assert_eq!(
                orch.context_attachment_history(&prepared.turn_reminders)[0]["type"],
                "total_tokens_reminder"
            );
            assert_eq!(
                orch.session.lock().await.history,
                vec![original.clone(), prepared.turn_reminders[0].clone()],
                "only the human prompt and durable total-token attachment enter history"
            );
            assert_ne!(prepared.snapshot[0].id(), original.id());
        } else {
            assert_eq!(prepared.snapshot[0], original);
            assert_eq!(prepared.context_announcements.messages.len(), 2);
        }
        if name == "invalid-latest-does-not-fall-back-to-older-inline" {
            assert_eq!(
                orch.prompt_runtime
                    .prompt_snapshot
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .system_prompt,
                vec![case["prior"][0]["attachment"]["systemPrompt"][0]
                    .as_str()
                    .unwrap()
                    .to_owned()],
                "static prefix restoration keeps its independent last-valid behavior"
            );
        }
    }
}

#[tokio::test]
async fn auto_kept_tail_places_missing_native_announcements_before_summary_and_skips_empty_api_projection(
) {
    let fixture = oracle();
    let case = fixture["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["name"] == "initial-four-file-kinds")
        .unwrap();
    for keep_announcements in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let orch = orchestrator(fixture_files(case), dir.path());
        let announced = orch.context_announcement_messages().await;
        let tail = ConversationMessage::user(MessageId::new(), "kept human prompt".into());
        orch.session.lock().await.history.push(tail.clone());
        let summary =
            ConversationMessage::compact_summary(MessageId::new(), "compacted summary".into());
        let mut kept = if keep_announcements {
            announced.clone()
        } else {
            Vec::new()
        };
        kept.push(tail.clone());
        let result = compaction::IterationCompactionResult {
            messages: vec![summary.clone()],
            messages_to_preserve: kept,
            media_analysis_to_preserve: Vec::new(),
            raw_summary_text: "compacted summary".into(),
            layers_applied: vec![compaction::CompactionLayer::Autocompact],
            total_tokens_freed: 1,
            cache_hit: false,
            consecutive_failures: 0,
            was_compacted: true,
            rapid_refill_breaker_tripped: false,
            consecutive_rapid_refills: 0,
            compaction_usage: None,
            compaction_model: None,
        };
        orch.apply_post_compact(
            result,
            compaction::CompactTrigger::Auto,
            1000,
            4,
            1000,
            std::time::Instant::now(),
            None,
        )
        .await
        .unwrap();
        let history = orch.session.lock().await.model_context_history();
        assert!(
            matches!(&history[0], ConversationMessage::System { subtype, .. }
            if subtype.as_deref() == Some("compact_boundary"))
        );
        let summary_index = history
            .iter()
            .position(|message| message.id() == summary.id())
            .unwrap();
        if keep_announcements {
            assert_eq!(
                summary_index, 1,
                "families already in kept tail are not inserted again"
            );
            assert_eq!(&history[2..5], announced.as_slice());
        } else {
            assert_eq!(summary_index, 4, "Cn inserts all three rows before summary");
            let attachments = orch.context_attachment_history(&history[1..4]);
            assert_eq!(
                attachments
                    .iter()
                    .map(|attachment| attachment["type"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                vec!["instructions", "session_context", "date"]
            );
            assert_eq!(attachments[0], case["expected"][0]["attachment"]);
            assert!(history[1..4]
                .iter()
                .zip(announced.iter())
                .all(|(new, old)| new.id() != old.id()));
        }
        assert_eq!(history.last(), Some(&tail));
        let normalized = llm_runtime::convert::normalize_messages_for_api(history);
        assert!(!normalized
            .iter()
            .any(|message| matches!(message, ConversationMessage::System { .. })));
        assert!(normalized
            .iter()
            .any(|message| body(message).is_some_and(|body| body.contains("managed policy"))));
        assert!(orch
            .context_announcements_from_frozen_snapshot()
            .await
            .is_empty());
    }
}
