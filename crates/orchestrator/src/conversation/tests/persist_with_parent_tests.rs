use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use crate::OrchestratorConfig;
use platform_api::OrchestratorHandle;
use platform_posix::fs::PosixFileSystem;
use protocol::SessionId;
use session::jsonl::schema::JsonlMessage;
use std::sync::Arc;
use tool_api::registry::ToolRegistry;
use uuid::Uuid;

/// Build an orchestrator wired with a `JsonlWriter` backed by `path`.
fn orch_with_writer(dir: &std::path::Path, path: std::path::PathBuf) -> ConversationOrchestrator {
    let fs: Arc<dyn platform_api::FileSystem> = Arc::new(PosixFileSystem::new(dir.to_path_buf()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path, fs));
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        dir.to_path_buf(),
    )
    .with_jsonl_writer(writer)
}

/// Read all JSONL lines back from disk and deserialize.
fn read_jsonl(path: &std::path::Path) -> Vec<JsonlMessage> {
    let raw = std::fs::read_to_string(path).expect("read jsonl");
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<JsonlMessage>(l).expect("deserialize jsonl line"))
        .collect()
}

#[tokio::test]
async fn manual_compaction_owns_snapshot_and_commit_before_queued_fusion() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let state_root = dir.path().join("state");
    std::fs::create_dir(&state_root).unwrap();
    let id = SessionId::new();
    let writer = Arc::new(
        session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
        )
        .with_durable_lock(Arc::new(
            session::jsonl::DurableTranscriptWriter::open(&state_root).unwrap(),
        )),
    );
    writer
        .activate_session_target(id, path.clone(), dir.path().to_path_buf())
        .unwrap();
    let orch = crate::test_support::with_scripted_compactor(
        orch_with_writer(dir.path(), path.clone())
            .with_jsonl_writer(writer.clone())
            .with_session_id(id),
        "retained manual compaction summary",
    );
    let owner = orch.turn_gate.lock().await;
    let cancel = tokio_util::sync::CancellationToken::new();
    let canceled_compact = orch.force_compact_with_cancel(cancel.clone());
    tokio::pin!(canceled_compact);
    assert!(futures::poll!(canceled_compact.as_mut()).is_pending());
    cancel.cancel();
    assert!(matches!(
        canceled_compact.await,
        Err(platform_api::HandleError::ActionFailed(message)) if message == "Compaction canceled."
    ));
    assert!(orch.session.lock().await.history.is_empty());
    assert!(!path.exists());

    let compact = orch.force_compact_with_cancel(tokio_util::sync::CancellationToken::new());
    tokio::pin!(compact);
    // Empty history must not be snapshotted/rejected before acquiring the gate.
    assert!(futures::poll!(compact.as_mut()).is_pending());
    for index in 0..6 {
        let message_id = protocol::MessageId::new();
        let text = format!("history {index}: preserve this conversational context");
        let message = if index % 2 == 0 {
            ConversationMessage::user(message_id, text)
        } else {
            ConversationMessage::Assistant {
                id: message_id,
                content: vec![protocol::ContentBlock::Text { text }],
                stop_reason: None,
            }
        };
        orch.session.lock().await.history.push(message.clone());
        orch.persist_message_to_jsonl(&message).await;
    }
    let fusion = ConversationMessage::user_meta(protocol::MessageId::new(), "fusion".into());
    let payload = serde_json::to_value(orch.to_jsonl_message(
        &fusion,
        &id.to_string(),
        None,
        None,
        None,
        None,
    ))
    .unwrap();
    let append = orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload);
    tokio::pin!(append);
    assert!(futures::poll!(append.as_mut()).is_pending());
    drop(owner);
    let (compacted, appended) = tokio::join!(compact, append);
    compacted.unwrap();
    appended.unwrap();
    orch.append_external_history_message(ConversationMessage::user(
        protocol::MessageId::new(),
        "next".into(),
    ))
    .await;
    let contents = std::fs::read_to_string(&path).unwrap();
    let loaded = session::jsonl::reader::route_lines(&contents);
    let (chain, _) = session::jsonl::loader::build_conversation_chain(&loaded, &id.to_string());
    let summary_index = chain
        .iter()
        .position(|row| {
            row.message
                .to_string()
                .contains("retained manual compaction summary")
        })
        .expect("cold replay retains the committed summary");
    let fusion_index = chain
        .iter()
        .position(|row| row.uuid == fusion.id().as_uuid().to_string())
        .expect("cold replay retains the queued Fusion publication");
    assert!(summary_index < fusion_index);
    assert_eq!(
        chain.last().unwrap().parent_uuid.as_deref(),
        Some(chain[fusion_index].uuid.as_str())
    );
}

#[tokio::test]
async fn external_history_and_fusion_share_chain_ownership_in_both_orders() {
    for history_first in [false, true] {
        for bash_history in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let state_root = dir.path().join("state");
            std::fs::create_dir(&state_root).unwrap();
            let id = SessionId::new();
            let writer = Arc::new(
                session::jsonl::JsonlWriter::new(
                    path.clone(),
                    Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
                )
                .with_durable_lock(Arc::new(
                    session::jsonl::DurableTranscriptWriter::open(&state_root).unwrap(),
                )),
            );
            writer
                .activate_session_target(id, path.clone(), dir.path().to_path_buf())
                .unwrap();
            let orch = orch_with_writer(dir.path(), path.clone())
                .with_jsonl_writer(writer.clone())
                .with_session_id(id);
            orch.append_external_history_message(ConversationMessage::user(
                protocol::MessageId::new(),
                "first".into(),
            ))
            .await;
            let external_id = protocol::MessageId::new();
            let external = if bash_history {
                ConversationMessage::user(external_id, "<bash-stdout>result</bash-stdout>".into())
            } else {
                ConversationMessage::Assistant {
                    id: external_id,
                    content: vec![protocol::ContentBlock::Text {
                        text: "SDK history".into(),
                    }],
                    stop_reason: None,
                }
            };
            let fusion =
                ConversationMessage::user_meta(protocol::MessageId::new(), "fusion".into());
            let payload = serde_json::to_value(orch.to_jsonl_message(
                &fusion,
                &id.to_string(),
                None,
                None,
                None,
                None,
            ))
            .unwrap();
            // Poll both real entrypoints while the chain gate is held, fixing
            // their FIFO order without sleeps or a fake persistence layer.
            let owner = orch.turn_gate.lock().await;
            let fusion_append =
                orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload);
            let history_append = orch.append_external_history_message(external);
            tokio::pin!(fusion_append, history_append);
            if history_first {
                assert!(futures::poll!(history_append.as_mut()).is_pending());
                assert!(futures::poll!(fusion_append.as_mut()).is_pending());
            } else {
                assert!(futures::poll!(fusion_append.as_mut()).is_pending());
                assert!(futures::poll!(history_append.as_mut()).is_pending());
            }
            assert!(
                !orch
                    .session
                    .lock()
                    .await
                    .history
                    .iter()
                    .any(|message| message.id() == external_id),
                "external history must not mutate the session before owning its chain"
            );
            drop(owner);
            let (appended, ()) = tokio::join!(fusion_append, history_append);
            appended.unwrap();
            orch.append_external_history_message(ConversationMessage::user(
                protocol::MessageId::new(),
                "next".into(),
            ))
            .await;
            let rows = read_jsonl(&path);
            assert_eq!(rows.len(), 4);
            for pair in rows.windows(2) {
                assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
            }
            let first_queued = if history_first {
                external_id
            } else {
                fusion.id()
            };
            assert_eq!(rows[1].uuid, first_queued.as_uuid().to_string());
            let loaded =
                session::jsonl::reader::route_lines(&std::fs::read_to_string(&path).unwrap());
            let (chain, _) =
                session::jsonl::loader::build_conversation_chain(&loaded, &id.to_string());
            assert_eq!(chain.len(), 4);
            assert!(chain
                .iter()
                .any(|row| row.uuid == fusion.id().as_uuid().to_string()));
            assert!(chain
                .iter()
                .any(|row| row.uuid == external_id.as_uuid().to_string()));
        }
    }
}

#[tokio::test]
async fn fusion_retry_of_visible_unacknowledged_tip_repairs_resume_chain() {
    check_fusion_unacknowledged_resume_chain(false).await;
}

#[tokio::test]
async fn fusion_written_sync_failure_preserves_chain_when_foreground_precedes_retry() {
    check_fusion_unacknowledged_resume_chain(true).await;
}

async fn check_fusion_unacknowledged_resume_chain(foreground_before_retry: bool) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let state_root = dir.path().join("state");
    std::fs::create_dir(&state_root).unwrap();
    let id = SessionId::new();
    let writer = Arc::new(
        session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
        )
        .with_durable_lock(Arc::new(
            session::jsonl::DurableTranscriptWriter::open(&state_root).unwrap(),
        )),
    );
    writer
        .activate_session_target(id, path.clone(), dir.path().to_path_buf())
        .unwrap();
    let orch = orch_with_writer(dir.path(), path.clone())
        .with_jsonl_writer(writer.clone())
        .with_session_id(id);
    let first = ConversationMessage::user(protocol::MessageId::new(), "first".into());
    orch.persist_message_to_jsonl(&first).await;
    let fusion = ConversationMessage::user_meta(protocol::MessageId::new(), "fusion".into());
    let payload = serde_json::to_value(orch.to_jsonl_message(
        &fusion,
        &id.to_string(),
        None,
        None,
        None,
        None,
    ))
    .unwrap();
    let fusion_uuid = fusion.id().as_uuid().to_string();
    let cursor = orch.transcript.last_jsonl_uuid.lock().await.clone();
    {
        let _turn = orch.turn_gate.lock().await;
        let failure = orch
            .reconcile_fusion_transcript_append(
                id,
                fusion_uuid.clone(),
                Err(session::jsonl::TranscriptWriterError::Fs(
                    platform_api::FsError::Io("partial write".into()),
                )),
            )
            .await;
        assert!(failure.is_err());
        assert_eq!(*orch.transcript.last_jsonl_uuid.lock().await, cursor);
        let off_session = orch
            .reconcile_fusion_transcript_append(
                SessionId::new(),
                fusion_uuid.clone(),
                Err(session::jsonl::TranscriptWriterError::WrittenButNotDurable(
                    platform_api::FsError::Io("other session sync failed".into()),
                )),
            )
            .await;
        assert!(off_session.is_err());
        assert_eq!(*orch.transcript.last_jsonl_uuid.lock().await, cursor);
    }
    // Reproduce the orchestrator state after write succeeds but its durability
    // acknowledgement fails: the disk has the row, while the live cursor has
    // not advanced. The session unit test injects the genuine post-write fault.
    writer
        .append_json_once_durable_for_session(id, "delivery", payload.clone())
        .await
        .unwrap();
    if foreground_before_retry {
        let _turn = orch.turn_gate.lock().await;
        let failure = orch
            .reconcile_fusion_transcript_append(
                id,
                fusion_uuid,
                Err(session::jsonl::TranscriptWriterError::WrittenButNotDurable(
                    platform_api::FsError::Io("sync failed".into()),
                )),
            )
            .await;
        assert!(matches!(
            failure,
            Err(session::jsonl::TranscriptWriterError::WrittenButNotDurable(
                _
            ))
        ));
    } else {
        assert_eq!(
            orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload.clone())
                .await
                .unwrap(),
            session::jsonl::TranscriptAppendOutcome::AlreadyPresent
        );
    }
    let next = ConversationMessage::user(protocol::MessageId::new(), "next".into());
    orch.persist_message_to_jsonl(&next).await;
    // Retry after the intervening foreground prompt must not rewind the cursor.
    orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload)
        .await
        .unwrap();
    let last = ConversationMessage::user(protocol::MessageId::new(), "last".into());
    orch.persist_message_to_jsonl(&last).await;
    let rows = read_jsonl(&path);
    let messages: Vec<_> = rows
        .iter()
        .filter(|row| matches!(row.message_type.as_str(), "user" | "assistant"))
        .collect();
    assert_eq!(messages.len(), 4);
    for pair in messages.windows(2) {
        assert_eq!(pair[1].parent_uuid.as_deref(), Some(pair[0].uuid.as_str()));
    }
    let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(&path).unwrap());
    let (chain, _) = session::jsonl::loader::build_conversation_chain(&loaded, &id.to_string());
    assert_eq!(chain.len(), messages.len());
    assert!(chain
        .iter()
        .any(|row| row.uuid == fusion.id().as_uuid().to_string()));
}

#[tokio::test]
async fn fusion_append_waits_for_foreground_and_never_rewinds_a_newer_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let state_root = dir.path().join("state");
    std::fs::create_dir(&state_root).unwrap();
    let id = SessionId::new();
    let writer = Arc::new(
        session::jsonl::JsonlWriter::new(
            path.clone(),
            Arc::new(PosixFileSystem::new(dir.path().to_path_buf())),
        )
        .with_durable_lock(Arc::new(
            session::jsonl::DurableTranscriptWriter::open(&state_root).unwrap(),
        )),
    );
    writer
        .activate_session_target(id, path.clone(), dir.path().to_path_buf())
        .unwrap();
    let orch = Arc::new(
        orch_with_writer(dir.path(), path.clone())
            .with_jsonl_writer(writer.clone())
            .with_session_id(id),
    );
    let user = ConversationMessage::user(protocol::MessageId::new(), "first".into());
    orch.persist_message_to_jsonl(&user).await;
    let foreground = orch.turn_gate.lock().await;
    let fusion_id = protocol::MessageId::new();
    let fusion_uuid = fusion_id.as_uuid().to_string();
    let fusion_message = ConversationMessage::user_meta(fusion_id, "fusion".into());
    let payload = serde_json::to_value(orch.to_jsonl_message(
        &fusion_message,
        &id.to_string(),
        None,
        None,
        None,
        None,
    ))
    .unwrap();
    let append = orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload.clone());
    tokio::pin!(append);
    tokio::select! { biased;
        _ = &mut append => panic!("Fusion disk append must wait for the foreground chain owner"),
        () = tokio::task::yield_now() => {}
    }
    let answer = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "foreground answer".into(),
        }],
        stop_reason: None,
    };
    orch.persist_message_to_jsonl(&answer).await;
    drop(foreground);
    assert_eq!(
        append.await.unwrap(),
        session::jsonl::TranscriptAppendOutcome::Appended
    );
    let next = ConversationMessage::user(protocol::MessageId::new(), "next".into());
    orch.persist_message_to_jsonl(&next).await;
    // A delayed projection and an old durable duplicate may never rewind the
    // cursor after an ordinary message has already advanced it.
    orch.record_persisted_fusion_meta(id, &fusion_uuid, "fusion".into())
        .await
        .unwrap();
    assert_eq!(
        orch.append_fusion_transcript(writer.as_ref(), id, "delivery", payload)
            .await
            .unwrap(),
        session::jsonl::TranscriptAppendOutcome::AlreadyPresent
    );
    let final_user = ConversationMessage::user(protocol::MessageId::new(), "last".into());
    orch.persist_message_to_jsonl(&final_user).await;
    let rows = read_jsonl(&path);
    let messages: Vec<_> = rows
        .iter()
        .filter(|row| matches!(row.message_type.as_str(), "user" | "assistant"))
        .collect();
    assert_eq!(messages.len(), 5);
    for pair in messages.windows(2) {
        assert_eq!(
            pair[1].parent_uuid.as_deref(),
            Some(pair[0].uuid.as_str()),
            "resume must retain the entire foreground/Fusion/next-prompt chain"
        );
    }
    let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(&path).unwrap());
    let (chain, loaded_session) =
        session::jsonl::loader::build_conversation_chain(&loaded, &id.to_string());
    assert_eq!(loaded_session, id.to_string());
    let chain_ids: Vec<_> = chain.iter().map(|row| row.uuid.as_str()).collect();
    for row in messages {
        assert!(
            chain_ids.contains(&row.uuid.as_str()),
            "resume lost {}",
            row.uuid
        );
    }

    let other = SessionId::new();
    let other_path = dir.path().join("other.jsonl");
    let other_root = dir.path().join("other-state");
    std::fs::create_dir(&other_root).unwrap();
    writer.activate_session_target_with_durable_lock(
        other,
        other_path.clone(),
        dir.path().to_path_buf(),
        Arc::new(session::jsonl::DurableTranscriptWriter::open(other_root).unwrap()),
    );
    let cursor = orch.transcript.last_jsonl_uuid.lock().await.clone();
    let other_uuid = protocol::MessageId::new().as_uuid().to_string();
    assert_eq!(
        orch.append_fusion_transcript(
            writer.as_ref(),
            other,
            "other-delivery",
            serde_json::json!({"uuid":other_uuid,"text":"off-session"})
        )
        .await
        .unwrap(),
        session::jsonl::TranscriptAppendOutcome::Appended
    );
    assert_eq!(
        *orch.transcript.last_jsonl_uuid.lock().await,
        cursor,
        "off-session delivery must not mutate the active session cursor"
    );
    assert!(other_path.exists());
}

#[tokio::test]
async fn fusion_meta_appends_only_to_launch_session_and_stays_out_of_model_context() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(branding::DOT_DIR);
    let cwd = dir.path().join("workspace");
    std::fs::create_dir_all(&cwd).expect("workspace");
    let active = SessionId::new();
    let launch = SessionId::new();
    let active_path =
        session::jsonl::session_path(&home, &cwd.to_string_lossy(), &active.as_uuid().to_string());
    let launch_path =
        session::jsonl::session_path(&home, &cwd.to_string_lossy(), &launch.as_uuid().to_string());
    let fs: Arc<dyn platform_api::FileSystem> =
        Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(
        active_path.clone(),
        fs,
    ));
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
    )
    .with_jsonl_writer(writer)
    .with_config_home(home)
    .with_session_id(active);

    orch.append_meta_user_message_to_session(&launch.to_string(), "<fusion_result />")
        .await
        .expect("append to launch session");

    assert!(
        !active_path.exists(),
        "active session must not receive the result"
    );
    let launch_lines = read_jsonl(&launch_path);
    assert_eq!(launch_lines.len(), 1);
    assert_eq!(launch_lines[0].session_id, launch.to_string());
    assert_eq!(
        launch_lines[0].extra.get("isModelContextExcluded"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(orch.session.lock().await.history.is_empty());

    orch.append_meta_user_message_to_session(&active.to_string(), "<fusion_result />")
        .await
        .expect("append to active session");
    let state = orch.session.lock().await;
    assert_eq!(state.history.len(), 1);
    assert!(state.model_context_history().is_empty());
}

#[tokio::test]
async fn fusion_command_messages_remain_in_model_context_and_durable_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());
    let id = orch.current_session_id().await;
    let prompt = "/fusion --quality explain the cursor issue\nPreserve these details.";
    orch.append_fusion_command_message(&id.to_string(), prompt, true)
        .await
        .expect("persist original request");
    orch.append_fusion_command_message(&id.to_string(), "Fusion started: task-1", false)
        .await
        .expect("persist launch reply");
    let state = orch.session.lock().await;
    assert_eq!(state.model_context_history().len(), 2);
    assert!(matches!(
        &state.history[0],
        ConversationMessage::User { is_meta: false, .. }
    ));
    assert!(matches!(
        &state.history[1],
        ConversationMessage::Assistant { .. }
    ));
    let lines = read_jsonl(&path);
    assert_eq!(lines.len(), 2);
    for line in &lines {
        assert_ne!(
            line.extra.get("isModelContextExcluded"),
            Some(&serde_json::Value::Bool(true))
        );
    }
    assert_eq!(
        lines[1].parent_uuid.as_deref(),
        Some(lines[0].uuid.as_str())
    );
    assert!(std::fs::read_to_string(path)
        .unwrap()
        .contains("Preserve these details."));
}

#[tokio::test]
async fn fusion_command_off_session_reply_does_not_leak_into_active_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let home = dir.path().join(branding::DOT_DIR);
    let path = dir.path().join("active.jsonl");
    let orch = orch_with_writer(dir.path(), path).with_config_home(home.clone());
    let launch = SessionId::new();
    let cursor = orch.transcript.last_jsonl_uuid.lock().await.clone();
    orch.append_fusion_command_message(
        &launch.to_string(),
        "fusion failed to start: unavailable model",
        false,
    )
    .await
    .expect("persist to launch session");
    assert!(orch.session.lock().await.history.is_empty());
    assert_eq!(*orch.transcript.last_jsonl_uuid.lock().await, cursor);
    let target = session::jsonl::session_path(
        &home,
        &dir.path().to_string_lossy(),
        &launch.as_uuid().to_string(),
    );
    let lines = read_jsonl(&target);
    assert_eq!(lines.len(), 1);
    assert_ne!(
        lines[0].extra.get("isModelContextExcluded"),
        Some(&serde_json::Value::Bool(true))
    );
}

#[tokio::test]
async fn user_lines_persist_the_live_permission_mode_before_the_common_trailer() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());
    orch.session.lock().await.plan_mode = true;

    orch.persist_message_to_jsonl(&ConversationMessage::user(
        protocol::MessageId::new(),
        "plan next".into(),
    ))
    .await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().next().expect("one line");
    assert!(line.contains(r#""permissionMode":"plan""#), "{line}");
    assert!(
        line.find("permissionMode").unwrap() < line.find("userType").unwrap(),
        "permissionMode must occupy Claude's user-message head slot: {line}"
    );
}

// ── test 1: explicit parent_override ─────────────────────────────────────

#[tokio::test]
async fn tool_result_parents_to_explicit_assistant_uuid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    // Persist an assistant message first (linear chain — no override).
    let asst_msg = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "I will call a tool".into(),
        }],
        stop_reason: Some("tool_use".into()),
    };
    orch.persist_message_to_jsonl(&asst_msg).await;

    // Capture the assistant line's uuid from disk.
    let lines_after_asst = read_jsonl(&session_path);
    assert_eq!(
        lines_after_asst.len(),
        1,
        "expected 1 line (the assistant message)"
    );
    let assistant_uuid = lines_after_asst[0].uuid.clone();

    // Persist a tool-result user message via the override variant, passing
    // the assistant's uuid explicitly — simulates streaming executor parenting.
    let tool_result_msg =
        ConversationMessage::user(protocol::MessageId::new(), "tool result body".into());
    orch.persist_message_to_jsonl_with_parent(&tool_result_msg, Some(assistant_uuid.clone()))
        .await;

    // Read back both lines.
    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 2, "expected 2 lines (assistant + tool_result)");
    let tool_result_line = &lines[1];

    // THE KEY ASSERTION: the tool-result line's parentUuid must equal the
    // assistant's uuid, NOT the prior last_jsonl_uuid (which also happens to
    // be the assistant uuid here, but the next test distinguishes them).
    assert_eq!(
        tool_result_line.parent_uuid.as_deref(),
        Some(assistant_uuid.as_str()),
        "tool_result parentUuid must equal the explicit assistant uuid override"
    );
}

// ── test 2: override bypasses last_jsonl_uuid ─────────────────────────────
//
// Three messages: user → assistant → tool_result(override=user_uuid).
// Without the override, the tool_result would parent to the assistant.
// With the override it must parent to the user uuid instead, proving the
// override takes effect independent of what `last_jsonl_uuid` holds.

#[tokio::test]
async fn override_bypasses_last_jsonl_uuid_chain() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    // 1. Persist a user message (no override).
    let user_msg = ConversationMessage::user(protocol::MessageId::new(), "user prompt".into());
    orch.persist_message_to_jsonl(&user_msg).await;
    let lines = read_jsonl(&session_path);
    let user_uuid = lines[0].uuid.clone();

    // 2. Persist an assistant message (no override → chains off user).
    let asst_msg = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "ok calling tool".into(),
        }],
        stop_reason: Some("tool_use".into()),
    };
    orch.persist_message_to_jsonl(&asst_msg).await;
    let lines = read_jsonl(&session_path);
    assert_eq!(lines[1].parent_uuid.as_deref(), Some(user_uuid.as_str()));
    let _asst_uuid = lines[1].uuid.clone();

    // 3. Persist a tool-result user message with an EXPLICIT override pointing
    //    back to the user_uuid (unusual, but proves the override wins over
    //    last_jsonl_uuid which currently holds the assistant uuid).
    let tool_result_msg =
        ConversationMessage::user(protocol::MessageId::new(), "tool result".into());
    orch.persist_message_to_jsonl_with_parent(&tool_result_msg, Some(user_uuid.clone()))
        .await;

    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 3, "expected 3 lines");
    assert_eq!(
        lines[2].parent_uuid.as_deref(),
        Some(user_uuid.as_str()),
        "override must win over last_jsonl_uuid (which holds the assistant uuid)"
    );
}

// ── test 3: None path advances last_jsonl_uuid (regression) ──────────────
//
// Proves `persist_message_to_jsonl_with_parent(msg, None)` is byte-identical
// to the old `persist_message_to_jsonl`: two messages with None form a
// monotonic chain where msg2.parentUuid == msg1.uuid.

#[tokio::test]
async fn none_override_chains_off_last_jsonl_uuid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let msg1 = ConversationMessage::user(protocol::MessageId::new(), "first message".into());
    orch.persist_message_to_jsonl_with_parent(&msg1, None).await;

    let msg2 = ConversationMessage::user(protocol::MessageId::new(), "second message".into());
    orch.persist_message_to_jsonl_with_parent(&msg2, None).await;

    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 2, "expected 2 JSONL lines");
    // First entry: root of chain → parent_uuid is None.
    assert_eq!(
        lines[0].parent_uuid, None,
        "first entry must have no parent"
    );
    // Second entry: must chain off the first.
    assert_eq!(
        lines[1].parent_uuid.as_deref(),
        Some(lines[0].uuid.as_str()),
        "second entry parentUuid must equal first entry uuid (linear chain)"
    );
}

// ── test 4: last_jsonl_uuid advances after override ──────────────────────
//
// After an overridden persist, `last_jsonl_uuid` is still advanced to the
// newly-persisted line's uuid. A subsequent non-overridden line must chain
// off the overridden line (not off whatever the override pointed to).

#[tokio::test]
async fn last_jsonl_uuid_advances_after_override() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    // 1. First message (no override) — root.
    let msg1 = ConversationMessage::user(protocol::MessageId::new(), "root".into());
    orch.persist_message_to_jsonl(&msg1).await;
    let lines = read_jsonl(&session_path);
    let root_uuid = lines[0].uuid.clone();

    // 2. Overridden message pointing back to root — simulates a tool result.
    let msg2 = ConversationMessage::user(protocol::MessageId::new(), "overridden".into());
    orch.persist_message_to_jsonl_with_parent(&msg2, Some(root_uuid.clone()))
        .await;
    let lines = read_jsonl(&session_path);
    let overridden_uuid = lines[1].uuid.clone();
    // Verify the override took effect.
    assert_eq!(
        lines[1].parent_uuid.as_deref(),
        Some(root_uuid.as_str()),
        "overridden line must parent to root, not to itself"
    );

    // 3. Third message (no override) — must chain off msg2 (the overridden line),
    //    not off msg1 (root). This confirms last_jsonl_uuid was advanced.
    let msg3 = ConversationMessage::user(protocol::MessageId::new(), "subsequent".into());
    orch.persist_message_to_jsonl(&msg3).await;
    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 3, "expected 3 JSONL lines");
    assert_eq!(
        lines[2].parent_uuid.as_deref(),
        Some(overridden_uuid.as_str()),
        "subsequent non-overridden line must chain off the overridden line"
    );
}

/// A denied tool's persisted `user` line carries `toolDenialKind`, in
/// claude's slot (after `timestamp`, before the `userType` trailer).
///
/// The kind is recorded against the `tool_use_id` at the permission
/// decision and stamped here, mirroring claude's message-level field. The
/// exactly-one-`tool_result` guard is claude's own (`Tpr`): a message
/// carrying several tool_results cannot attribute one kind, so it gets none.
#[tokio::test]
async fn denied_tool_result_line_carries_tool_denial_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    orch.record_tool_denial_kind(&tuid, "permission-rule").await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "Permission to use Bash has been denied.".into(),
            is_error: true,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().next().expect("one line");
    assert!(
        line.contains(r#""toolDenialKind":"permission-rule""#),
        "denied line must carry the kind, got: {line}"
    );
    let i_kind = line.find("toolDenialKind").expect("kind present");
    let i_trailer = line.find("userType").expect("trailer present");
    assert!(
        i_kind < i_trailer,
        "toolDenialKind must precede the common trailer, got: {line}"
    );
}

/// O1: a tool_result `user` line carries the tool's STRUCTURED result as
/// `toolUseResult` plus `sourceToolAssistantUUID` (== `parentUuid`).
///
/// Oracle: the success arm at 2.1.220 BIN off **235420375** builds
/// `zr({content:Ft, …, toolUseResult: gt, …, sourceToolAssistantUUID:
/// i.uuid})` where `gt = se.data` is the tool's raw structured result
/// (NOT the model-facing string). The writer `insertMessageChain`
/// (BIN off **237862200**) then DERIVES `parentUuid` from
/// `sourceToolAssistantUUID`, which is why the two are equal on all
/// 96 794 real 2.1.220 lines carrying the field.
#[tokio::test]
async fn tool_result_line_carries_structured_result_and_source_assistant_uuid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    // 1. The assistant line owning this tool_use.
    let assistant = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolUse {
            id: tuid.clone(),
            name: "Bash".into(),
            input: serde_json::json!({"command":"ls"}),
            provider_id: None,
        }],
        stop_reason: None,
    };
    let map = orch
        .persist_assistant_per_block(&assistant, None, None)
        .await;
    let assistant_uuid = map
        .get(&tuid)
        .cloned()
        .expect("tool_use line uuid recorded");

    // 2. The tool's structured result, recorded at dispatch.
    orch.record_tool_use_result(
        &tuid,
        serde_json::json!({"stdout":"a\n","stderr":"","interrupted":false}),
    )
    .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "a".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl_with_parent(&msg, Some(assistant_uuid.clone()))
        .await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().nth(1).expect("tool_result line");
    assert!(
        line.contains(r#""toolUseResult":{"stdout":"a\n","stderr":"","interrupted":false}"#),
        "structured result must ride verbatim, got: {line}"
    );
    assert!(
        line.contains(&format!(r#""sourceToolAssistantUUID":"{assistant_uuid}""#)),
        "source assistant uuid must be the tool_use's own line uuid, got: {line}"
    );
    assert!(
        line.contains(&format!(r#""parentUuid":"{assistant_uuid}""#)),
        "parentUuid must equal sourceToolAssistantUUID, got: {line}"
    );
    let i_res = line.find("toolUseResult").expect("result present");
    let i_src = line
        .find("sourceToolAssistantUUID")
        .expect("source present");
    let i_trailer = line.find("userType").expect("trailer present");
    assert!(
        i_res < i_src && i_src < i_trailer,
        "head order must be toolUseResult < sourceToolAssistantUUID < trailer, got: {line}"
    );
}

/// O1: a FAILED tool's `toolUseResult` is the plain string
/// `` `Error: ${message}` ``, NOT the `{"error":…}` object the port sends
/// on its stream-json wire (2.1.220 BIN off **235424595**).
#[tokio::test]
async fn error_tool_result_persists_the_bare_error_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    orch.record_tool_use_result(&tuid, serde_json::Value::String("Error: boom".into()))
        .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "Error: boom".into(),
            is_error: true,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    assert!(
        raw.contains(r#""toolUseResult":"Error: boom""#),
        "error result must be the bare string, got: {raw}"
    );
    assert!(
        !raw.contains(r#""toolUseResult":{"error""#),
        "the {{error:…}} object is the stream-json wire, not the transcript"
    );
}

/// O1: an MCP tool's `mcpMeta` is a TOP-LEVEL sibling between
/// `toolDenialKind` and `sourceToolAssistantUUID`, never nested inside
/// `toolUseResult` (2.1.220 BIN off **232969604**: on the main chain
/// `Uks(undefined, meta)` returns the raw meta verbatim).
#[tokio::test]
async fn mcp_result_line_carries_mcp_meta_as_a_top_level_sibling() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    orch.record_tool_use_result(&tuid, serde_json::json!([{"type":"text","text":"hi"}]))
        .await;
    orch.record_tool_use_mcp_meta(&tuid, serde_json::json!({"_meta":{"claude/endTurn":true}}))
        .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "hi".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().next().expect("one line");
    assert!(
        line.contains(r#""mcpMeta":{"_meta":{"claude/endTurn":true}}"#),
        "mcpMeta must ride verbatim, got: {line}"
    );
    let i_res = line.find(r#""toolUseResult""#).expect("result present");
    let i_mcp = line.find(r#""mcpMeta""#).expect("meta present");
    let i_trailer = line.find("userType").expect("trailer present");
    assert!(
        i_res < i_mcp && i_mcp < i_trailer,
        "mcpMeta is a sibling AFTER toolUseResult and before the trailer, got: {line}"
    );
}

#[tokio::test]
async fn mcp_end_turn_result_persists_only_mcp_meta() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    orch.record_tool_use_result(&tuid, serde_json::json!([{"type":"text","text":"hi"}]))
        .await;
    orch.record_tool_use_mcp_meta(&tuid, serde_json::json!({"_meta":{"claude/endTurn":true}}))
        .await;
    orch.record_pending_tool_result_turn_end(
        &tuid,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::McpMeta,
        },
    )
    .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid.clone(),
            content: "hi".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().next().expect("one line");
    assert!(
        line.contains(r#""mcpMeta":{"_meta":{"claude/endTurn":true}}"#),
        "the MCP marker must persist verbatim, got: {line}"
    );
    assert!(
        !line.contains(r#""toolEndsTurn""#),
        "MCP metadata ends the turn through mcpMeta; toolEndsTurn is reserved for native ToolResult.endsTurn: {line}"
    );
}

#[tokio::test]
async fn native_end_turn_result_persists_tool_ends_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let tuid = protocol::ToolUseId::new();
    orch.record_tool_use_result(&tuid, serde_json::json!({"ok": true}))
        .await;
    orch.record_pending_tool_result_turn_end(
        &tuid,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::Tool,
        },
    )
    .await;

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: tuid,
            content: "done".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    let line = raw.lines().next().expect("one line");
    assert!(
        line.contains(r#""toolEndsTurn":true"#),
        "native ToolResult.endsTurn must persist at the top level: {line}"
    );
}

#[tokio::test]
async fn concurrent_end_turn_markers_use_the_last_result_and_drain_all_entries() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_writer(dir.path(), dir.path().join("session.jsonl"));
    let native = protocol::ToolUseId::new();
    let mcp = protocol::ToolUseId::new();
    orch.record_pending_tool_result_turn_end(
        &native,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::Tool,
        },
    )
    .await;
    orch.record_pending_tool_result_turn_end(
        &mcp,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::McpMeta,
        },
    )
    .await;

    let selected = orch
        .take_pending_tool_result_turn_ends(&[native.clone(), mcp.clone()])
        .await
        .expect("one batch-level end request");
    assert_eq!(
        selected.source,
        tool_api::tool_trait::ToolResultTurnEndSource::McpMeta,
        "the last matching result overwrites the query's scalar end source"
    );
    assert!(
        orch.transcript
            .pending_tool_result_turn_end
            .lock()
            .await
            .is_empty(),
        "all matching side-table entries must be removed after the turn"
    );
}

/// O1: the exactly-one-`tool_result` guard (claude's `Tpr`) applies to
/// EVERY tool-result head key, not just `toolDenialKind` — a batched user
/// message carrying two results cannot attribute one message-level value.
#[tokio::test]
async fn two_tool_results_in_one_user_message_get_no_head_keys() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let a = protocol::ToolUseId::new();
    let b = protocol::ToolUseId::new();
    orch.record_tool_use_result(&a, serde_json::json!({"stdout":"a"}))
        .await;
    orch.record_tool_use_result(&b, serde_json::json!({"stdout":"b"}))
        .await;
    orch.record_source_tool_assistant_uuid(&a, "aaa".into())
        .await;

    let mk = |id: protocol::ToolUseId| protocol::ContentBlock::ToolResult {
        tool_use_id: id,
        content: "x".into(),
        is_error: false,
        provider_tool_use_id: None,
        content_blocks: None,
    };
    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![mk(a), mk(b)],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    assert!(!raw.contains("toolUseResult"), "got: {raw}");
    assert!(!raw.contains("sourceToolAssistantUUID"), "got: {raw}");
}

/// An ALLOWED tool's line is byte-unchanged — no stray key.
#[tokio::test]
async fn allowed_tool_result_line_has_no_denial_kind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let msg = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: protocol::ToolUseId::new(),
            content: "ok".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl(&msg).await;

    let raw = std::fs::read_to_string(&session_path).expect("session file");
    assert!(!raw.contains("toolDenialKind"));
}

#[tokio::test]
async fn persist_message_to_jsonl_uses_the_supplied_message_uuid() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());

    let raw_uuid = uuid::Uuid::new_v4();
    let msg = ConversationMessage::user(
        protocol::MessageId::from_uuid(raw_uuid),
        "sdk replay prompt".into(),
    );
    orch.persist_message_to_jsonl(&msg).await;

    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].uuid, raw_uuid.to_string());
}

#[tokio::test]
async fn session_contains_message_uuid_accepts_bare_and_prefixed_ids() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path);

    let id = protocol::MessageId::new();
    {
        let mut session = orch.session.lock().await;
        session
            .history
            .push(ConversationMessage::user(id, "seed".into()));
    }

    assert!(
        orch.session_contains_message_uuid(&id.as_uuid().to_string())
            .await
    );
    assert!(orch.session_contains_message_uuid(&id.to_string()).await);
    assert!(
        !orch
            .session_contains_message_uuid(&uuid::Uuid::new_v4().to_string())
            .await
    );
}

#[tokio::test]
async fn assistant_envelope_carries_full_betamessage_shape() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_writer(dir.path(), dir.path().join("s.jsonl"));
    let msg = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text { text: "hi".into() }],
        stop_reason: Some("end_turn".into()),
    };
    let usage = serde_json::json!({ "input_tokens": 5, "output_tokens": 3 });

    // Real path (model + usage supplied) → full BetaMessage envelope, in the
    // claude-code / golden-fixture key order.
    let jmsg = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        Some("inner-abc"),
        Some("claude-opus-4-8"),
        Some(&usage),
        Some("req_test123"),
        None,
    );
    // The real-response path stamps the top-level `requestId` (via `extra`).
    assert_eq!(
        jmsg.extra.get("requestId").and_then(|v| v.as_str()),
        Some("req_test123"),
        "real assistant line carries the top-level requestId"
    );
    let inner = jmsg.message.as_object().expect("inner is an object");
    let keys: Vec<&str> = inner.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "type",
            "role",
            "content",
            "model",
            "stop_reason",
            "stop_sequence",
            "usage"
        ],
        "BetaMessage envelope key order"
    );
    assert_eq!(inner["id"], serde_json::json!("inner-abc"));
    assert_eq!(inner["type"], serde_json::json!("message"));
    assert_eq!(inner["role"], serde_json::json!("assistant"));
    assert_eq!(inner["model"], serde_json::json!("claude-opus-4-8"));
    assert_eq!(inner["stop_reason"], serde_json::json!("end_turn"));
    assert_eq!(inner["stop_sequence"], serde_json::Value::Null);
    assert_eq!(inner["usage"], usage);

    // Synthetic path (no model/usage) → the synthetic BetaMessage envelope.
    // SC-05: re-derived from claude-code 2.1.238 `Mqm` (cc-238 @296633254).
    // This assertion previously pinned a 2.1.185 reading that was wrong on
    // two counts — it demanded `diagnostics` be ABSENT and `usage` be
    // OMITTED. Both are present upstream: `diagnostics:null` is the first
    // key, and `Mqm`'s `usage` parameter DEFAULTS to a zeroed usage object,
    // so the key is always serialized. `stop_reason` stays hardcoded
    // "stop_sequence" (NOT the message's own "end_turn").
    let plain = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        Some("inner-abc"),
        None,
        None,
        None,
        None,
    );
    // No request_id supplied → no top-level `requestId` (the synthetic case).
    assert!(
        !plain.extra.contains_key("requestId"),
        "synthetic line omits requestId"
    );
    let pinner = plain.message.as_object().unwrap();
    let pkeys: Vec<&str> = pinner.keys().map(String::as_str).collect();
    assert_eq!(
        pkeys,
        vec![
            "diagnostics",
            "id",
            "container",
            "model",
            "role",
            "stop_details",
            "stop_reason",
            "stop_sequence",
            "type",
            "usage",
            "content",
            "context_management"
        ],
        "synthetic BetaMessage envelope key order"
    );
    assert_eq!(pinner["id"], serde_json::json!("inner-abc"));
    assert_eq!(pinner["model"], serde_json::json!("<synthetic>"));
    assert_eq!(pinner["container"], serde_json::Value::Null);
    assert_eq!(pinner["role"], serde_json::json!("assistant"));
    assert_eq!(pinner["stop_details"], serde_json::Value::Null);
    // Hardcoded "stop_sequence", NOT the message's own "end_turn".
    assert_eq!(pinner["stop_reason"], serde_json::json!("stop_sequence"));
    assert_eq!(pinner["stop_sequence"], serde_json::json!(""));
    assert_eq!(pinner["type"], serde_json::json!("message"));
    assert_eq!(pinner["context_management"], serde_json::Value::Null);
    // `usage` is PRESENT and is `Mqm`'s zeroed default, key order included.
    assert_eq!(pinner["diagnostics"], serde_json::Value::Null);
    let pusage = pinner["usage"].as_object().expect("synthetic usage object");
    assert_eq!(
        pusage.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "output_tokens_details",
            "input_tokens",
            "output_tokens",
            "cache_creation_input_tokens",
            "cache_read_input_tokens",
            "server_tool_use",
            "service_tier",
            "cache_creation",
            "inference_geo",
            "iterations",
            "speed"
        ],
        "Mqm default usage key order"
    );
    assert_eq!(pusage["output_tokens_details"], serde_json::Value::Null);
    assert_eq!(pusage["input_tokens"], serde_json::json!(0));
    assert_eq!(pusage["output_tokens"], serde_json::json!(0));
    assert_eq!(
        pusage["server_tool_use"],
        serde_json::json!({"web_search_requests": 0, "web_fetch_requests": 0})
    );
    assert_eq!(
        pusage["cache_creation"],
        serde_json::json!({
            "ephemeral_1h_input_tokens": 0,
            "ephemeral_5m_input_tokens": 0
        })
    );
    assert_eq!(pinner["content"][0]["type"], serde_json::json!("text"));
    assert_eq!(pinner["content"][0]["text"], serde_json::json!("hi"));
}

#[tokio::test]
async fn synthetic_api_error_envelope_stamps_top_level_fields() {
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_writer(dir.path(), dir.path().join("s.jsonl"));
    let msg = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "API Error: boom".into(),
        }],
        stop_reason: Some("model_error".into()),
    };

    // 1. No-category builder (top-level `model_error` catch / malformed
    //    terminal): `isApiErrorMessage:true`, `error`/`apiErrorStatus` OMITTED,
    //    inner `stop_reason` stays `"stop_sequence"`.
    let bare = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ApiErrorEnvelope::default()),
    );
    assert_eq!(
        bare.extra.get("isApiErrorMessage"),
        Some(&serde_json::Value::Bool(true)),
        "isApiErrorMessage is always stamped"
    );
    assert!(!bare.extra.contains_key("error"), "no error category");
    assert!(
        !bare.extra.contains_key("apiErrorStatus"),
        "no apiErrorStatus"
    );
    assert_eq!(
        bare.message["stop_reason"],
        serde_json::json!("stop_sequence"),
        "no-override keeps the synthetic stop_sequence"
    );

    // 2. `max_output_tokens` category (max_tokens / context-window cap).
    let cap = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ApiErrorEnvelope {
            error: Some("max_output_tokens"),
            api_error_status: None,
            inner_stop_reason: None,
            truncated_after_output: false,
        }),
    );
    assert_eq!(
        cap.extra.get("error").and_then(|v| v.as_str()),
        Some("max_output_tokens")
    );
    assert_eq!(
        cap.extra.get("isApiErrorMessage"),
        Some(&serde_json::Value::Bool(true))
    );

    // 3. Refusal: `error:"invalid_request"` + inner `stop_reason:"refusal"`.
    let refusal = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ApiErrorEnvelope {
            error: Some("invalid_request"),
            api_error_status: None,
            inner_stop_reason: Some("refusal"),
            truncated_after_output: false,
        }),
    );
    assert_eq!(
        refusal.extra.get("error").and_then(|v| v.as_str()),
        Some("invalid_request")
    );
    assert_eq!(
        refusal.message["stop_reason"],
        serde_json::json!("refusal"),
        "refusal overrides the inner stop_reason"
    );

    // 4. With an HTTP status → `apiErrorStatus` is a JSON number.
    let with_status = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ApiErrorEnvelope {
            error: Some("rate_limit"),
            api_error_status: Some(429),
            inner_stop_reason: None,
            truncated_after_output: false,
        }),
    );
    assert_eq!(
        with_status.extra.get("apiErrorStatus"),
        Some(&serde_json::Value::Number(429.into()))
    );
    assert!(
        !with_status.extra.contains_key("truncatedAfterOutput"),
        "truncatedAfterOutput is omitted when false"
    );

    // 4b. Has-output incomplete-response notice stamps truncatedAfterOutput.
    let truncated = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&ApiErrorEnvelope {
            error: Some("server_error"),
            api_error_status: None,
            inner_stop_reason: None,
            truncated_after_output: true,
        }),
    );
    assert_eq!(
        truncated.extra.get("truncatedAfterOutput"),
        Some(&serde_json::Value::Bool(true))
    );

    // 5. A normal (non-api-error) assistant line stamps NOTHING.
    let normal = orch.to_jsonl_message(&msg, "sess", None, None, None, None);
    assert!(!normal.extra.contains_key("isApiErrorMessage"));
    assert!(!normal.extra.contains_key("error"));
}

// ── per-request api-error classifier (`Flp`/`KNn`) ────────────────────────

/// `classify_api_error` maps each typed [`LlmError`] semantic variant to the
/// canonical (category, status) pair recovered from the 2.1.195 `Flp`/`KNn`
/// classifier + the on-disk transcript aggregate. Status is OMITTED (`None`)
/// where the port has no confident canonical HTTP status (mirrors claude
/// omitting `apiErrorStatus` when the error is not an `APIError`-with-status).
#[test]
fn classify_api_error_prefers_the_true_status_over_the_canonical_table() {
    use llm_runtime::LlmError;
    // A REAL 422 used to be persisted as 400: `InvalidRequest` mapped to its
    // canonical status because the raw one was gone by then. The provider
    // decoders now store the SDK's `${status} ${body}` text, so the true
    // status survives into the transcript's `apiErrorStatus`.
    for (status, expected) in [(422u16, 422u16), (424, 424), (409, 409)] {
        let e = OrchestratorError::ApiCall(LlmError::InvalidRequest {
            message: format!("{status} {{\"type\":\"error\"}}"),
        });
        let env = classify_api_error(&e);
        assert_eq!(env.api_error_status, Some(expected), "status {status}");
        // The CATEGORY still comes from the variant — only the status is
        // sharpened, so the byte-locked file-format value is untouched.
        assert_eq!(env.error, Some("invalid_request"));
    }

    // No prefix (internal validation, not a provider decode) → the canonical
    // table still applies, exactly as before.
    let internal = OrchestratorError::ApiCall(LlmError::InvalidRequest {
        message: "invalid model name".to_string(),
    });
    assert_eq!(classify_api_error(&internal).api_error_status, Some(400));

    // A variant carrying no message can never gain a prefix, so its
    // canonical status is unaffected.
    let auth = OrchestratorError::ApiCall(LlmError::Authentication {
        message: String::new(),
    });
    assert_eq!(classify_api_error(&auth).api_error_status, Some(401));
}

#[test]
fn classify_api_error_maps_llm_variants_to_category_and_status() {
    use llm_runtime::LlmError;
    let cases: Vec<(LlmError, Option<&'static str>, Option<u16>)> = vec![
        (
            LlmError::RateLimited {
                retry_after: None,
                scope: None,
            },
            Some("rate_limit"),
            Some(429),
        ),
        // On-disk 529 lines tag `server_error` (NOT the `YNn` statusline
        // `"overloaded"`).
        (
            LlmError::Overloaded { repeated: false },
            Some("server_error"),
            Some(529),
        ),
        (
            LlmError::Authentication {
                message: String::new(),
            },
            Some("authentication_failed"),
            Some(401),
        ),
        (
            LlmError::PermissionDenied {
                message: String::new(),
            },
            Some("authentication_failed"),
            Some(403),
        ),
        // Billing is an Error-message match in `Flp`, not a status branch.
        (LlmError::QuotaExceeded, Some("billing_error"), None),
        // PTL/context-window: `invalid_request` with NO status.
        (
            LlmError::ContextOverflow { token_gap: 12 },
            Some("invalid_request"),
            None,
        ),
        // 2.1.212 413 request-too-large: SAME `invalid_request` category as
        // the context-window branch, no `apiErrorStatus` on the `su` call.
        (LlmError::RequestTooLarge, Some("invalid_request"), None),
        (
            LlmError::InvalidRequest {
                message: "bad".into(),
            },
            Some("invalid_request"),
            Some(400),
        ),
        (
            LlmError::ModelUnavailable,
            Some("model_not_found"),
            Some(404),
        ),
        (LlmError::ProviderInternal, Some("server_error"), Some(500)),
        // Timeout/transport tail → `server_error`, no status.
        (
            LlmError::Transport {
                message: "t".into(),
            },
            Some("server_error"),
            None,
        ),
        (
            LlmError::StreamInterrupted {
                message: "s".into(),
            },
            Some("server_error"),
            None,
        ),
        // Generic `Error` fallthrough → `unknown`.
        (
            LlmError::CostUnavailable {
                message: "c".into(),
            },
            Some("unknown"),
            None,
        ),
        (
            LlmError::UnsupportedCapability {
                capability: "x".into(),
            },
            Some("unknown"),
            None,
        ),
    ];
    for (inner, cat, status) in cases {
        // Both wrapping variants classify identically.
        for wrapped in [
            OrchestratorError::ApiCall(inner.clone()),
            OrchestratorError::Streaming(inner.clone()),
        ] {
            let env = classify_api_error(&wrapped);
            assert_eq!(env.error, cat, "category for {inner:?}");
            assert_eq!(env.api_error_status, status, "status for {inner:?}");
            // The `ql` path never overrides the inner stop_reason.
            assert_eq!(
                env.inner_stop_reason, None,
                "inner stop_reason for {inner:?}"
            );
        }
    }
}

/// The 2.1.212 `$Vi()` request-too-large notice is byte-exact and its tail
/// switches on interactivity (`un()===!Ht.isInteractive`).
#[test]
fn request_too_large_notice_is_byte_exact() {
    // Non-interactive (print) session: generic advice.
    assert_eq!(
        super::request_too_large_notice(false),
        "Request too large (max 32MB). Accumulated images and attachments in the conversation pushed the request over the limit. Remove older images or compact the conversation."
    );

    // Interactive (TUI) session: `/compact` + double-esc actions.
    assert_eq!(
        super::request_too_large_notice(true),
        "Request too large (max 32MB). Accumulated images and attachments in the conversation pushed the request over the limit. Run /compact, or double press esc to go back and remove attachments."
    );
}

/// Orchestrator-internal / generic-Error variants fall through to `unknown`
/// with NO status (the `Flp` generic-`Error` tail).
#[test]
fn classify_api_error_generic_variants_are_unknown_no_status() {
    for e in [
        OrchestratorError::Internal("boom".into()),
        OrchestratorError::StreamingProtocol("bad".into()),
        OrchestratorError::StreamEndedWithoutStop,
        OrchestratorError::RepeatedOverloaded,
        OrchestratorError::PermissionAbort {
            message: "Agent aborted: too many classifier denials in headless mode".into(),
        },
        OrchestratorError::MaxTurnsReached { max_turns: 30 },
        OrchestratorError::MaxBudgetReached {
            budget_nano_usd: 5_000_000_000,
        },
    ] {
        let env = classify_api_error(&e);
        assert_eq!(env.error, Some("unknown"), "{e:?}");
        assert_eq!(env.api_error_status, None, "{e:?}");
        assert_eq!(env.inner_stop_reason, None, "{e:?}");
    }
}

/// End-to-end: a classified envelope drives the persisted JSONL line's
/// top-level `error`/`isApiErrorMessage`/`apiErrorStatus` fields with
/// presence + values 1:1 with the classifier output.
#[test]
fn classified_envelope_stamps_jsonl_top_level_fields() {
    use llm_runtime::LlmError;
    let dir = tempfile::tempdir().expect("tempdir");
    let orch = orch_with_writer(dir.path(), dir.path().join("s.jsonl"));
    let msg = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "invalid request: bad".into(),
        }],
        stop_reason: Some("model_error".into()),
    };

    let env = classify_api_error(&OrchestratorError::ApiCall(LlmError::InvalidRequest {
        message: "bad".into(),
    }));
    let line = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&env),
    );
    assert_eq!(
        line.extra.get("error").and_then(|v| v.as_str()),
        Some("invalid_request")
    );
    assert_eq!(
        line.extra.get("isApiErrorMessage"),
        Some(&serde_json::Value::Bool(true))
    );
    assert_eq!(
        line.extra.get("apiErrorStatus"),
        Some(&serde_json::Value::Number(400.into()))
    );

    // A no-status category (server_error from transport) OMITS apiErrorStatus.
    let env2 = classify_api_error(&OrchestratorError::ApiCall(LlmError::Transport {
        message: "t".into(),
    }));
    let line2 = orch.to_jsonl_message_with_inner_id(
        &msg,
        "sess",
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&env2),
    );
    assert_eq!(
        line2.extra.get("error").and_then(|v| v.as_str()),
        Some("server_error")
    );
    assert!(
        !line2.extra.contains_key("apiErrorStatus"),
        "no-status category must omit apiErrorStatus"
    );
}

// ── transcript per-line cwd reflects the LIVE (post-`cd`) session cwd ──────

#[tokio::test]
async fn transcript_line_cwd_tracks_live_cwd_after_cd() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Share a `current_cwd` cell with the orchestrator — the same `Arc` the
    // desktop composition root hands to `OrchestratorCwdChangedFirer`, which
    // a Bash `cd` mutates. Start it at the init cwd.
    let init_cwd = dir.path().to_path_buf();
    let cell = Arc::new(std::sync::Mutex::new(init_cwd.clone()));
    let orch =
        orch_with_writer(dir.path(), dir.path().join("s.jsonl")).with_current_cwd(cell.clone());

    // First persisted line is stamped with the init cwd.
    let m1 = ConversationMessage::user(protocol::MessageId::new(), "before cd".into());
    let line1 = orch.to_jsonl_message(&m1, "sess", None, None, None, None);
    assert_eq!(
        line1.cwd,
        init_cwd.to_string_lossy(),
        "pre-`cd` line carries the init cwd"
    );

    // Simulate a Bash `cd` advancing the shared cell (what the CwdChanged
    // firer does on every `cd`).
    let new_cwd = dir.path().join("subdir");
    *cell.lock().unwrap() = new_cwd.clone();

    // The NEXT persisted line must reflect the advanced cwd, not the init.
    let m2 = ConversationMessage::user(protocol::MessageId::new(), "after cd".into());
    let line2 = orch.to_jsonl_message(&m2, "sess", None, None, None, None);
    assert_eq!(
        line2.cwd,
        new_cwd.to_string_lossy(),
        "post-`cd` line must carry the advanced live cwd, not the init cwd"
    );
    assert_ne!(
        line2.cwd, line1.cwd,
        "the cwd readback must move with the live session cwd"
    );
}

// ── test 5: per-content_block_stop single-block assistant lines ───────────
//
// claude.ts:2171-2211: a streaming assistant turn emits ONE JSONL line per
// content block — same inner `message.id`, distinct top-level `uuid`, one
// block each. sessionStorage.ts:1028: each tool_result parents to ITS
// tool_use's line uuid (`sourceToolAssistantUUID`), NOT a shared per-turn
// parent.
//
// This drives `persist_assistant_per_block` directly: an assistant turn with
// content [text, tool_use A, tool_use B] must persist THREE single-block
// assistant lines that (a) share one inner `message.id`, (b) have three
// DISTINCT top-level uuids, (c) carry exactly one block each; then a
// tool_result for A parents to A's line uuid and a tool_result for B parents
// to B's line uuid.
#[tokio::test]
async fn assistant_turn_persists_one_line_per_content_block_with_per_tool_reparenting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());
    let session = orch.session();
    session.lock().await.model_profile = Some("deepseek".to_string());

    let id_a = protocol::ToolUseId::from("toolu_A");
    let id_b = protocol::ToolUseId::from("toolu_B");

    let assistant_id = protocol::MessageId::new();
    let assistant_msg = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![
            protocol::ContentBlock::Text {
                text: "let me call two tools".into(),
            },
            protocol::ContentBlock::ToolUse {
                id: id_a.clone(),
                name: "Alpha".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
            protocol::ContentBlock::ToolUse {
                id: id_b.clone(),
                name: "Bravo".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
        ],
        stop_reason: Some("tool_use".into()),
    };

    let map = orch
        .persist_assistant_per_block(&assistant_msg, None, None)
        .await;

    let lines = read_jsonl(&session_path);
    // (c) THREE single-block assistant lines.
    let asst_lines: Vec<&JsonlMessage> = lines
        .iter()
        .filter(|l| l.message_type == "assistant")
        .collect();
    assert_eq!(
        asst_lines.len(),
        3,
        "expected 3 single-block assistant lines (one per content block), got {}",
        asst_lines.len()
    );
    assert!(
        asst_lines[0].parent_uuid.is_none(),
        "first block should start a fresh chain link"
    );
    assert_eq!(
        asst_lines[1].parent_uuid.as_deref(),
        Some(asst_lines[0].uuid.as_str()),
        "second block should chain from first block uuid"
    );
    assert_eq!(
        asst_lines[2].parent_uuid.as_deref(),
        Some(asst_lines[1].uuid.as_str()),
        "third block should chain from second block uuid"
    );
    for (i, l) in asst_lines.iter().enumerate() {
        let blocks = l
            .message
            .get("content")
            .and_then(|c| c.as_array())
            .unwrap_or_else(|| panic!("line {i} content must be an array"));
        assert_eq!(blocks.len(), 1, "line {i} must carry exactly one block");
        assert_eq!(
            l.extra.get("modelProfile").and_then(|value| value.as_str()),
            Some("deepseek"),
            "line {i} must persist the provider profile used for the response"
        );
    }

    // (a) all three share ONE inner `message.id`.
    let inner_ids: Vec<&str> = asst_lines
        .iter()
        .map(|l| {
            l.message
                .get("id")
                .and_then(|v| v.as_str())
                .expect("inner message.id present")
        })
        .collect();
    assert_eq!(
        inner_ids[0], inner_ids[1],
        "all blocks must share the same inner message.id"
    );
    assert_eq!(inner_ids[1], inner_ids[2]);
    assert_eq!(
        inner_ids[0],
        assistant_id.as_uuid().to_string(),
        "shared inner message.id must be the turn's logical id"
    );

    // (b) three DISTINCT top-level uuids.
    let uuids: std::collections::HashSet<&str> =
        asst_lines.iter().map(|l| l.uuid.as_str()).collect();
    assert_eq!(
        uuids.len(),
        3,
        "the three lines must have distinct top-level uuids"
    );

    // map must hold A and B -> their respective line uuids (text block none).
    let a_uuid = map.get(&id_a).expect("A in map").clone();
    let b_uuid = map.get(&id_b).expect("B in map").clone();
    assert_ne!(a_uuid, b_uuid, "A and B must map to different line uuids");

    // Persist a tool_result for A and for B; each must parent to ITS line.
    let tr_a = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: id_a.clone(),
            content: "result-A".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl_with_parent(&tr_a, Some(a_uuid.clone()))
        .await;
    let tr_b = ConversationMessage::User {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::ToolResult {
            tool_use_id: id_b.clone(),
            content: "result-B".into(),
            is_error: false,
            provider_tool_use_id: None,
            content_blocks: None,
        }],
        is_meta: false,
        is_compact_summary: false,
        is_visible_in_transcript_only: false,
    };
    orch.persist_message_to_jsonl_with_parent(&tr_b, Some(b_uuid.clone()))
        .await;

    let lines = read_jsonl(&session_path);
    let tr_lines: Vec<&JsonlMessage> = lines
        .iter()
        .filter(|l| {
            l.message_type == "user"
                && serde_json::to_string(&l.message)
                    .map(|s| s.contains("tool_result"))
                    .unwrap_or(false)
        })
        .collect();
    assert_eq!(tr_lines.len(), 2, "expected 2 tool_result user lines");
    // tr_a parents to A's line; tr_b parents to B's line — NOT one shared parent.
    assert_eq!(
        tr_lines[0].parent_uuid.as_deref(),
        Some(a_uuid.as_str()),
        "tool_result A must parent to A's tool_use line uuid"
    );
    assert_eq!(
        tr_lines[1].parent_uuid.as_deref(),
        Some(b_uuid.as_str()),
        "tool_result B must parent to B's tool_use line uuid"
    );
    assert_ne!(
        tr_lines[0].parent_uuid, tr_lines[1].parent_uuid,
        "the two tool_results must NOT share one parent (per-tool reparenting)"
    );
}

#[tokio::test]
async fn assistant_per_block_uuids_are_deterministic_for_identical_inputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path_1 = dir.path().join("session1.jsonl");
    let session_path_2 = dir.path().join("session2.jsonl");
    let session_path_3 = dir.path().join("session3.jsonl");
    let session_path_4 = dir.path().join("session4.jsonl");
    let session_path_5 = dir.path().join("session5.jsonl");
    let orch1 = orch_with_writer(dir.path(), session_path_1.clone());
    let orch2 = orch_with_writer(dir.path(), session_path_2.clone());
    let orch3 = orch_with_writer(dir.path(), session_path_3.clone());
    let orch4 = orch_with_writer(dir.path(), session_path_4.clone());
    let orch5 = orch_with_writer(dir.path(), session_path_5.clone());

    let assistant_id =
        protocol::MessageId::from_uuid(Uuid::from_u128(0x0a0b_0c0d_0e0f_1011_1213_1415_1617_1819));
    let content = vec![
        protocol::ContentBlock::Text {
            text: "seeded block".into(),
        },
        protocol::ContentBlock::ToolUse {
            id: protocol::ToolUseId::from("toolu_shared"),
            name: "Echo".into(),
            input: serde_json::json!({ "x": 1 }),
            provider_id: None,
        },
        protocol::ContentBlock::ToolUse {
            id: protocol::ToolUseId::from("toolu_followup"),
            name: "Echo".into(),
            input: serde_json::json!({ "x": 2 }),
            provider_id: None,
        },
    ];
    let msg_1 = ConversationMessage::Assistant {
        id: assistant_id,
        content: content.clone(),
        stop_reason: Some("tool_use".into()),
    };
    let msg_2 = ConversationMessage::Assistant {
        id: assistant_id,
        content,
        stop_reason: Some("tool_use".into()),
    };

    orch1.persist_assistant_per_block(&msg_1, None, None).await;
    orch2.persist_assistant_per_block(&msg_2, None, None).await;
    let msg_3 = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![
            protocol::ContentBlock::Text {
                text: "different seed block".into(),
            },
            protocol::ContentBlock::ToolUse {
                id: protocol::ToolUseId::from("toolu_shared"),
                name: "Echo".into(),
                input: serde_json::json!({ "x": 1 }),
                provider_id: None,
            },
            protocol::ContentBlock::ToolUse {
                id: protocol::ToolUseId::from("toolu_followup"),
                name: "Echo".into(),
                input: serde_json::json!({ "x": 2 }),
                provider_id: None,
            },
        ],
        stop_reason: Some("tool_use".into()),
    };
    orch3.persist_assistant_per_block(&msg_3, None, None).await;
    let mut input_tool_4 = serde_json::Map::new();
    input_tool_4.insert(
        "x".to_string(),
        serde_json::json!({ "nested": { "b": 2, "a": 1 } }),
    );
    input_tool_4.insert("y".to_string(), serde_json::json!(1));
    // Same VALUE as `input_tool_4`, different key ORDER at both levels:
    // `y` before `x` at the top, `a` before `b` inside `nested`. The
    // `nested` wrapper has to be present here too — without it the two
    // inputs differ in content, the signatures diverge for the right
    // reason, and the assertion stops testing ordering at all.
    let mut input_tool_5 = serde_json::Map::new();
    input_tool_5.insert("y".to_string(), serde_json::json!(1));
    input_tool_5.insert(
        "x".to_string(),
        serde_json::json!({ "nested": { "a": 1, "b": 2 } }),
    );
    let msg_4 = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![protocol::ContentBlock::ToolUse {
            id: protocol::ToolUseId::from("toolu_ordered"),
            name: "Echo".into(),
            input: serde_json::Value::Object(input_tool_4),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
    };
    let msg_5 = ConversationMessage::Assistant {
        id: assistant_id,
        content: vec![protocol::ContentBlock::ToolUse {
            id: protocol::ToolUseId::from("toolu_ordered"),
            name: "Echo".into(),
            input: serde_json::Value::Object(input_tool_5),
            provider_id: None,
        }],
        stop_reason: Some("tool_use".into()),
    };
    orch4.persist_assistant_per_block(&msg_4, None, None).await;
    orch5.persist_assistant_per_block(&msg_5, None, None).await;

    let first = read_jsonl(&session_path_1);
    let second = read_jsonl(&session_path_2);
    let third = read_jsonl(&session_path_3);
    let fourth = read_jsonl(&session_path_4);
    let fifth = read_jsonl(&session_path_5);
    let uuids_1: Vec<String> = first
        .into_iter()
        .filter(|line| line.message_type == "assistant")
        .map(|line| line.uuid)
        .collect();
    let uuids_2: Vec<String> = second
        .into_iter()
        .filter(|line| line.message_type == "assistant")
        .map(|line| line.uuid)
        .collect();
    let uuids_3: Vec<String> = third
        .into_iter()
        .filter(|line| line.message_type == "assistant")
        .map(|line| line.uuid)
        .collect();
    let uuids_4: Vec<String> = fourth
        .into_iter()
        .filter(|line| line.message_type == "assistant")
        .map(|line| line.uuid)
        .collect();
    let uuids_5: Vec<String> = fifth
        .into_iter()
        .filter(|line| line.message_type == "assistant")
        .map(|line| line.uuid)
        .collect();

    assert_eq!(
        uuids_1, uuids_2,
        "same turn id / content / position should deterministically derive identical block uuids"
    );
    assert_ne!(
        uuids_1, uuids_3,
        "content-sensitive signature should change when block payload changes"
    );
    assert_eq!(
        uuids_4, uuids_5,
        "equivalent tool input json object order must not alter per-block signature"
    );
}

#[tokio::test]
async fn merged_assistant_persists_model_profile_for_resume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());
    let session = orch.session();
    session.lock().await.model_profile = Some("openrouter".to_string());
    let assistant = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![protocol::ContentBlock::Text {
            text: "done".to_string(),
        }],
        stop_reason: Some("end_turn".to_string()),
    };

    orch.persist_assistant_merged(&assistant, None, None).await;

    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0]
            .extra
            .get("modelProfile")
            .and_then(|value| value.as_str()),
        Some("openrouter")
    );
}

#[tokio::test]
async fn batched_parallel_results_keep_individual_metadata_and_shared_assistant_parent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let orch = orch_with_writer(dir.path(), session_path.clone());
    let mcp_id = protocol::ToolUseId::new();
    let native_id = protocol::ToolUseId::new();
    let assistant = ConversationMessage::Assistant {
        id: protocol::MessageId::new(),
        content: vec![
            protocol::ContentBlock::ToolUse {
                id: mcp_id.clone(),
                name: "McpEnd".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
            protocol::ContentBlock::ToolUse {
                id: native_id.clone(),
                name: "NativeEnd".into(),
                input: serde_json::json!({}),
                provider_id: None,
            },
        ],
        stop_reason: Some("tool_use".into()),
    };
    orch.persist_assistant_merged(&assistant, None, None).await;

    orch.record_tool_use_result(&mcp_id, serde_json::json!({"mcp": true}))
        .await;
    orch.record_tool_use_mcp_meta(
        &mcp_id,
        serde_json::json!({"_meta":{"claude/endTurn":true}}),
    )
    .await;
    orch.record_pending_tool_result_turn_end(
        &mcp_id,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::McpMeta,
        },
    )
    .await;
    orch.record_tool_use_result(&native_id, serde_json::json!({"native": true}))
        .await;
    orch.record_pending_tool_result_turn_end(
        &native_id,
        tool_api::tool_trait::ToolResultTurnEnd {
            source: tool_api::tool_trait::ToolResultTurnEndSource::Tool,
        },
    )
    .await;

    for (id, text) in [(&mcp_id, "mcp"), (&native_id, "native")] {
        let result = ConversationMessage::User {
            id: protocol::MessageId::new(),
            content: vec![protocol::ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: text.into(),
                is_error: false,
                provider_tool_use_id: None,
                content_blocks: None,
            }],
            is_meta: false,
            is_compact_summary: false,
            is_visible_in_transcript_only: false,
        };
        let parent = orch.source_tool_assistant_uuid(id).await;
        orch.persist_message_to_jsonl_with_parent(&result, parent)
            .await;
    }

    let lines = read_jsonl(&session_path);
    assert_eq!(lines.len(), 3);
    let assistant_uuid = lines[0].uuid.as_str();
    assert_eq!(lines[1].parent_uuid.as_deref(), Some(assistant_uuid));
    assert_eq!(lines[2].parent_uuid.as_deref(), Some(assistant_uuid));
    assert_eq!(
        lines[1].extra.get("mcpMeta"),
        Some(&serde_json::json!({"_meta":{"claude/endTurn":true}}))
    );
    assert!(lines[1].extra.get("toolEndsTurn").is_none());
    assert_eq!(
        lines[2].extra.get("toolEndsTurn"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(lines[2].extra.get("mcpMeta").is_none());
}

#[tokio::test]
async fn scheduled_persistence_keeps_actual_settings_separate_from_session_defaults() {
    for merged in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let orch = orch_with_writer(dir.path(), path.clone());
        {
            let session = orch.session();
            let mut state = session.lock().await;
            state.model = "claude-sonnet-4-6".into();
            state.model_profile = Some("anthropic".into());
        }
        let assistant = ConversationMessage::Assistant {
            id: protocol::MessageId::new(),
            content: vec![protocol::ContentBlock::Text {
                text: "actual result".into(),
            }],
            stop_reason: Some("end_turn".into()),
        };
        let settings = crate::scheduled_turn::ScheduledSettings {
            model: "claude-opus-4-6".into(),
            provider: "anthropic".into(),
            reasoning: platform_api::ReasoningSelection::Level { id: "high".into() },
            thinking: llm_runtime::model::thinking::ThinkingConfig::Adaptive,
            effort: Some(serde_json::json!("high")),
        };
        crate::scheduled_turn::SETTINGS
            .scope(settings, async {
                if merged {
                    orch.persist_assistant_merged(&assistant, None, None).await;
                } else {
                    orch.persist_assistant_per_block(&assistant, None, None)
                        .await;
                }
            })
            .await;
        let rows = read_jsonl(&path);
        let row = rows
            .iter()
            .find(|row| row.message_type == "assistant")
            .unwrap();
        assert_eq!(row.message["model"], "claude-opus-4-6");
        assert_eq!(row.extra["modelProfile"], "anthropic");
        assert_eq!(row.extra["effort"], "high");
        assert_eq!(
            row.extra["reasoningSelection"],
            serde_json::json!({"type":"level","id":"high"})
        );
        assert_eq!(row.extra["perTurnSettings"], true);
        assert_eq!(orch.session().lock().await.model, "claude-sonnet-4-6");
    }
}

#[tokio::test]
async fn loop_wakeup_survives_jsonl_resume_without_entering_model_context() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("loop.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());
    orch.append_scheduled_loop_wakeup(
        "fire one".into(),
        None,
        0,
        0,
        crate::ScheduledLoopFire {
            fire_id: protocol::MessageId::new(),
            task_id: "first".into(),
            cron: "0 9 * * *".into(),
            prompt: "/loop".into(),
            task_kind_loop: true,
        },
    )
    .await
    .unwrap();
    orch.append_scheduled_loop_wakeup(
        "fire two".into(),
        Some("healthy".into()),
        1,
        123,
        crate::ScheduledLoopFire {
            fire_id: protocol::MessageId::new(),
            task_id: "second".into(),
            cron: "1 9 * * *".into(),
            prompt: "/loop".into(),
            task_kind_loop: true,
        },
    )
    .await
    .unwrap();
    let rows = read_jsonl(&path);
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].parent_uuid.as_deref(), Some(rows[0].uuid.as_str()));
    assert_eq!(rows[1].extra["subtype"], "scheduled_task_fire");
    assert_eq!(rows[1].extra["noOpStreak"], 1);
    assert_eq!(
        rows[1].extra["foldedUuids"],
        serde_json::json!([rows[0].uuid])
    );
    assert!(rows[1].extra.get("loopWakeup").is_none());
    assert!(rows[1].extra.get("isModelContextExcluded").is_none());
    assert_eq!(rows[2].extra["turnCompanion"], true);
    let restored = crate::resume::state_from_messages(Uuid::new_v4(), &rows);
    assert_eq!(restored.history.len(), 3);
    assert_eq!(restored.model_context_excluded_messages.len(), 2);
    assert!(!restored
        .model_context_excluded_messages
        .contains(&restored.history[2].id()));
}

#[tokio::test]
async fn fixed_scheduled_fire_omits_loop_only_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixed.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());
    orch.append_scheduled_loop_wakeup(
        "Running scheduled task".into(),
        None,
        0,
        0,
        crate::ScheduledLoopFire {
            fire_id: protocol::MessageId::new(),
            task_id: "fixed".into(),
            cron: "*/5 * * * *".into(),
            prompt: "check".into(),
            task_kind_loop: false,
        },
    )
    .await
    .unwrap();
    let rows = read_jsonl(&path);
    for field in [
        "taskKind",
        "cronKind",
        "noOpStreak",
        "streakStartedAt",
        "foldedUuids",
    ] {
        assert!(!rows[0].extra.contains_key(field), "{field} is loop-only");
    }
    let restored = crate::resume::state_from_messages(Uuid::new_v4(), &rows);
    assert_eq!(restored.history.len(), 1);
    assert!(
        matches!(&restored.history[0], ConversationMessage::System { content, .. }
        if content == "Running scheduled task")
    );
}

#[tokio::test]
async fn scheduled_fire_producer_matches_oracle_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("oracle.jsonl");
    let orch = orch_with_writer(dir.path(), path.clone());
    let fire_id = protocol::MessageId::new();
    orch.append_scheduled_loop_wakeup(
        "fire".into(),
        None,
        0,
        0,
        crate::ScheduledLoopFire {
            fire_id,
            task_id: "task-1".into(),
            cron: "1 9 * * *".into(),
            prompt: "/loop".into(),
            task_kind_loop: true,
        },
    )
    .await
    .unwrap();
    let mut row = read_jsonl(&path).remove(0);
    assert_eq!(row.uuid, fire_id.as_uuid().to_string());
    // Pin only nondeterministic host/session fields; all fire fields and their
    // presence/order remain exactly what the production writer emitted.
    row.uuid = "11111111-1111-4111-8111-111111111111".into();
    row.timestamp = "2026-09-12T09:00:00.000Z".into();
    row.session_id = "session-1".into();
    row.cwd = "/fixture".into();
    row.version = "0.12.0".into();
    row.entrypoint = Some("cli".into());
    row.git_branch = None;
    let expected =
        include_str!("../../../../session/tests/fixtures/loop-2.1.270/scheduled_fire.jsonl")
            .lines()
            .nth(1)
            .unwrap();
    assert_eq!(serde_json::to_string(&row).unwrap(), expected);
}

#[tokio::test]
async fn scheduled_streaming_turn_preserves_meta_origin_in_jsonl_and_history() {
    use crate::test_support_stream::{
        content_block_start_text, content_block_stop, message_delta_stop, message_start,
        message_stop, text_delta, MockStreamingApiClient,
    };
    for in_human_turn in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("origin.jsonl");
        let fs: Arc<dyn platform_api::FileSystem> =
            Arc::new(PosixFileSystem::new(dir.path().into()));
        let writer = Arc::new(session::jsonl::writer::JsonlWriter::new(path.clone(), fs));
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![crate::scripted![
            message_start("msg_origin", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "done"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ]]));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            api.clone(),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            dir.path().into(),
        )
        .with_jsonl_writer(writer);
        let prompt_id = protocol::MessageId::new();
        orch.run_turn_streaming_with_origin(
            "scheduled origin fixture",
            Vec::new(),
            tokio_util::sync::CancellationToken::new(),
            Some(prompt_id),
            in_human_turn,
        )
        .await
        .unwrap();
        let rows = read_jsonl(&path);
        let input = rows
            .iter()
            .find(|row| row.uuid == prompt_id.as_uuid().to_string())
            .unwrap();
        assert_eq!(
            input
                .extra
                .get("isMeta")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            !in_human_turn
        );
        let session = orch.session.lock().await;
        let input = session
            .history
            .iter()
            .find(|message| message.id() == prompt_id)
            .unwrap();
        assert_eq!(input.is_meta(), !in_human_turn);
        drop(session);
        let sent = api.captured_calls().await;
        assert_eq!(sent.len(), 1);
        // Provider conversion does not retain internal UUID/meta attributes;
        // those are asserted on the committed classifier history above.
        assert!(serde_json::to_string(&sent[0].messages)
            .unwrap()
            .contains("scheduled origin fixture"));
    }
}
