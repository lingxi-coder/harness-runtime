use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::host::instructions::InstructionRendering;
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;

fn orchestrator(root: &std::path::Path) -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.to_path_buf(),
    )
}

fn mode_source(id: uuid::Uuid, case: &str) -> String {
    let user = uuid::Uuid::new_v4();
    let snapshot = |text: &str| {
        serde_json::json!({
            "type":"attachment", "uuid":uuid::Uuid::new_v4(), "sessionId":id,
            "parentUuid":user, "attachment": {"type":"prompt_snapshot",
                "systemPrompt":[text], "tools":[], "contextRendering":"inline"}
        })
    };
    let mut rows = vec![serde_json::json!({
        "type":"user", "uuid":user, "sessionId":id, "parentUuid":null,
        "message":{"role":"user", "content":"hello"}
    })];
    if case != "missing" {
        rows.push(snapshot("old prefix"));
    }
    if case != "legacy" {
        rows.push(serde_json::json!({"type":"session-mode", "sessionId":id,
            "sessionMode":"chat", "contextRevision":1, "profileEpoch":1,
            "admittedMode":"chat", "admittedEpoch":1, "pending":false}));
        if case == "current" {
            rows.push(snapshot("current prefix"));
        }
        // A queued change must retain the snapshot of the admitted epoch.
        rows.push(serde_json::json!({"type":"session-mode", "sessionId":id,
            "sessionMode":"code", "contextRevision":2, "profileEpoch":2,
            "admittedMode":"chat", "admittedEpoch":1, "pending":true}));
    }
    rows.iter().map(|row| format!("{row}\n")).collect()
}

#[test]
fn legacy_snapshot_uuid_case_is_normalized() {
    let id = uuid::Uuid::new_v4();
    let source = mode_source(id, "legacy").replace(&id.to_string(), &id.to_string().to_uppercase());
    let (snapshot, rendering) =
        crate::resume::prompt_state_for_epoch_from_source(&source, &id.to_string(), 0);
    assert_eq!(snapshot.unwrap().system_prompt, vec!["old prefix"]);
    assert_eq!(rendering, Some(InstructionRendering::Inline));
}

#[tokio::test]
async fn fixed_session_rejects_a_different_mode_before_mutating_state() {
    let root = tempfile::tempdir().unwrap();
    let orch = orchestrator(root.path());
    assert!(orch
        .ensure_conversation_mode(ConversationMode::Chat)
        .await
        .is_err());
    let state = orch.conversation_mode().await;
    assert_eq!(
        (state.active, state.selected, state.revision, state.pending),
        (ConversationMode::Code, ConversationMode::Code, 0, false)
    );
}

#[tokio::test]
async fn cold_restore_rejects_another_space_before_replacing_session_state() {
    let root = tempfile::tempdir().unwrap();
    let orch = orchestrator(root.path());
    let original_id = orch.current_session_id().await;
    let target = uuid::Uuid::new_v4();
    let source = format!(
        "{}\n{}\n",
        serde_json::json!({"type":"user", "uuid":uuid::Uuid::new_v4(), "sessionId":target, "parentUuid":null,
            "message":{"role":"user", "content":"foreign"}}),
        serde_json::json!({"type":"session-mode", "sessionId":target, "sessionMode":"chat"}),
    );
    let loaded = session::jsonl::route_lines(&source);
    let (messages, _) = session::jsonl::build_conversation_chain(&loaded, &target.to_string());
    let result = orch
        .restore_captured_session_history(
            target,
            &messages,
            &loaded.messages_in_order,
            false,
            permission::PermissionMode::Default,
            Some(
                session::JsonlWriter::fixed_session_mode_from_source(&source, &target.to_string())
                    .unwrap(),
            ),
            &source,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(orch.current_session_id().await, original_id);
    assert!(orch.snapshot_history().await.is_empty());
    assert_eq!(
        orch.conversation_mode().await.active,
        ConversationMode::Code
    );
}

#[tokio::test]
async fn initialization_persists_chat_once_before_any_turn() {
    let root = tempfile::tempdir().unwrap();
    let id = lingxi_core::types::SessionId::new();
    let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs));
    let orch = orchestrator(root.path())
        .with_conversation_mode(ConversationMode::Chat)
        .with_session_id(id)
        .with_jsonl_writer(writer);
    let initial = orch.initialize_conversation_mode().await.unwrap();
    let first = std::fs::read_to_string(&path).unwrap();
    assert_eq!(orch.initialize_conversation_mode().await.unwrap(), initial);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), first);
    assert_eq!(
        session::JsonlWriter::fixed_session_mode_from_source(&first, &id.as_uuid().to_string())
            .unwrap()
            .active,
        ConversationMode::Chat
    );
    assert!(orch.snapshot_history().await.is_empty());
}

async fn assert_prompt_state(orch: &ConversationOrchestrator, case: &str) {
    let expected = match case {
        "legacy" => Some("old prefix"),
        "current" => Some("current prefix"),
        _ => None,
    };
    let snapshot = orch.prompt_runtime.prompt_snapshot.lock().await.clone();
    assert_eq!(
        snapshot.as_ref().map(|s| s.system_prompt[0].as_str()),
        expected
    );
    assert!(orch.prompt_snapshot_resume());
    assert_eq!(
        *orch.prompt_runtime.context_rendering_hint.lock().await,
        expected.map(|_| InstructionRendering::Inline)
    );
    if expected.is_some() {
        assert_eq!(
            orch.current_context_rendering().await,
            InstructionRendering::Inline
        );
    } else {
        let rendered = orch.provider_system_prompt().await.unwrap();
        orch.record_prompt_snapshot_if_needed(Some(&rendered), &[])
            .await;
        assert!(orch.prompt_runtime.prompt_snapshot.lock().await.is_none());
    }
}

#[tokio::test]
async fn captured_cold_profile_preserves_only_its_epoch_prompt_state() {
    let root = tempfile::tempdir().unwrap();
    for case in ["legacy", "current", "wrong_epoch", "missing"] {
        let id = lingxi_core::types::SessionId::new();
        let source = mode_source(id.as_uuid(), case);
        let loaded = session::jsonl::route_lines(&source);
        let (messages, _) =
            session::jsonl::build_conversation_chain(&loaded, &id.as_uuid().to_string());
        let profile = session::JsonlWriter::session_mode_transition_from_source(
            &source,
            &id.as_uuid().to_string(),
        );
        let mode = if case == "legacy" {
            ConversationMode::Code
        } else {
            ConversationMode::Chat
        };
        let orch = orchestrator(root.path())
            .with_conversation_mode(mode)
            .with_session_id(id);
        orch.restore_captured_session_history(
            id.as_uuid(),
            &messages,
            &loaded.messages_in_order,
            false,
            permission::PermissionMode::Default,
            profile,
            &source,
        )
        .await
        .unwrap();
        assert_prompt_state(&orch, case).await;
    }
}

#[tokio::test]
async fn hot_resume_profile_preserves_only_its_epoch_prompt_state() {
    let root = tempfile::tempdir().unwrap();
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
    );
    for case in ["legacy", "current", "wrong_epoch", "missing"] {
        let id = lingxi_core::types::SessionId::new();
        let source = mode_source(id.as_uuid(), case);
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        std::fs::write(&path, &source).unwrap();
        let replayed = crate::resume::replay_captured_session(
            path.clone(),
            source.clone(),
            session::jsonl::route_lines(&source),
            id.as_uuid(),
        )
        .unwrap();
        let initial = root.path().join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let writer = Arc::new(session::JsonlWriter::new(initial, fs.clone()));
        let orch = ConversationOrchestrator::into_shared(
            orchestrator(root.path())
                .with_conversation_mode(if case == "legacy" {
                    ConversationMode::Code
                } else {
                    ConversationMode::Chat
                })
                .with_jsonl_writer(writer),
        );
        orch.attach_owned_session_switches();
        orch.resume_session(
            id,
            replayed.state.history.clone(),
            replayed.last_message_uuid.map(|v| v.to_string()),
            None,
            replayed.handle_runtime_snapshot(),
        )
        .await
        .unwrap();
        assert_prompt_state(&orch, case).await;
    }
}

#[tokio::test]
async fn public_with_resume_restores_chat_epoch_and_current_skill_from_captured_metadata() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cwd = root.path().join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let sid = uuid::Uuid::new_v4();
    let first = uuid::Uuid::new_v4();
    let old = uuid::Uuid::new_v4();
    let current = uuid::Uuid::new_v4();
    let tail = uuid::Uuid::new_v4();
    let rows = [
        serde_json::json!({"type":"user", "uuid":first, "sessionId":sid, "parentUuid":null,
            "message":{"role":"user", "content":"question"}}),
        serde_json::json!({"type":"user", "uuid":old, "sessionId":sid, "parentUuid":first, "isMeta":true,
            "origin":{"type":"skill_activation", "context_epoch":0, "skill_name":"old", "skill_path":"/skills/old"},
            "message":{"role":"user", "content":"old skill"}}),
        serde_json::json!({"type":"session-mode", "sessionId":sid, "sessionMode":"chat", "contextRevision":1,
            "profileEpoch":1, "admittedMode":"chat", "admittedEpoch":1, "pending":false}),
        serde_json::json!({"type":"user", "uuid":current, "sessionId":sid, "parentUuid":old, "isMeta":true,
            "origin":{"type":"skill_activation", "context_epoch":1, "skill_name":"current", "skill_path":"/skills/current"},
            "message":{"role":"user", "content":"current skill"}}),
        serde_json::json!({"type":"user", "uuid":tail, "sessionId":sid, "parentUuid":current,
            "message":{"role":"user", "content":"continue"}}),
        serde_json::json!({"type":"attachment", "uuid":uuid::Uuid::new_v4(), "sessionId":sid, "parentUuid":tail,
            "attachment":{"type":"prompt_snapshot", "systemPrompt":["chat prefix"], "tools":[], "contextRendering":"inline"}}),
    ];
    let path = session::jsonl::session_path(&home, &cwd.to_string_lossy(), &sid.to_string());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        rows.iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>(),
    )
    .unwrap();
    let fs: Arc<dyn lingxi_core::host::FileSystem> = Arc::new(
        platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
    );
    let orch = ConversationOrchestrator::with_resume(
        crate::OrchestratorConfig::default(),
        sid,
        home,
        cwd.to_string_lossy().into_owned(),
        fs,
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd,
        None,
    )
    .await
    .unwrap();
    let profile = orch.conversation_mode().await;
    assert_eq!(
        (
            profile.active,
            profile.selected,
            profile.revision,
            profile.pending
        ),
        (ConversationMode::Chat, ConversationMode::Chat, 1, false)
    );
    assert_eq!(orch.profile_epoch().await, 1);
    let session = orch.session.lock().await;
    let history = session.model_context_history();
    assert!(!history.iter().any(|row| row.id().as_uuid() == old));
    assert!(history.iter().any(|row| row.id().as_uuid() == current));
    drop(session);
    assert_eq!(
        orch.prompt_runtime
            .prompt_snapshot
            .lock()
            .await
            .as_ref()
            .unwrap()
            .system_prompt,
        vec!["chat prefix"]
    );
    assert_eq!(
        orch.current_context_rendering().await,
        InstructionRendering::Inline
    );
    assert!(orch.prompt_snapshot_resume());
}

fn durable_orchestrator(
    root: &std::path::Path,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<session::jsonl::DurableTranscriptWriter>,
    std::path::PathBuf,
) {
    let id = lingxi_core::types::SessionId::new();
    let path = root.join(format!("{}.jsonl", id.as_uuid()));
    let fs: Arc<dyn lingxi_core::host::FileSystem> =
        Arc::new(platform_posix::fs::PosixFileSystem::new(root.to_path_buf()));
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(root).unwrap());
    let writer = session::JsonlWriter::new(path.clone(), fs).with_durable_lock(durable.clone());
    writer
        .activate_session_target(id, path.clone(), root.to_path_buf())
        .unwrap();
    let orch = ConversationOrchestrator::into_shared(crate::test_support::with_scripted_compactor(
        orchestrator(root)
            .with_session_id(id)
            .with_jsonl_writer(Arc::new(writer)),
        "retained task context",
    ));
    (orch, durable, path)
}

#[tokio::test]
async fn cancelled_storage_waiter_keeps_input_and_fixed_mode() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, _) = durable_orchestrator(root.path());
    let storage = orch.transcript.storage_pauses.lock().await;
    let cancel = CancellationToken::new();
    let admitted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let observed = admitted.clone();
    let mut turn = orch.run_queued_prompt_batch_with_admission(
        vec![crate::QueuedPromptInput {
            text: "hold".into(),
            ..Default::default()
        }],
        cancel.clone(),
        Box::new(move || {
            observed.store(true, std::sync::atomic::Ordering::SeqCst);
        }),
    );
    assert!(futures::poll!(turn.as_mut()).is_pending());
    cancel.cancel();
    drop(storage);
    assert!(matches!(turn.await.unwrap(), TurnOutcome::Cancelled));
    assert!(!admitted.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        orch.conversation_mode().await.active,
        ConversationMode::Code
    );
    assert!(!orch.conversation_mode().await.pending);
    assert!(orch.snapshot_history().await.is_empty());
}

#[tokio::test]
async fn cancelled_direct_bash_admits_and_persists_its_source_rows() {
    let root = tempfile::tempdir().unwrap();
    let (orch, _, path) = durable_orchestrator(root.path());
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (admitted_tx, admitted_rx) = tokio::sync::oneshot::channel();
    let inputs = vec![
        crate::QueuedPromptInput {
            text: "<system-reminder>direct Bash result follows</system-reminder>".into(),
            is_meta: true,
            ..Default::default()
        },
        crate::QueuedPromptInput {
            text: "<bash-input>printf done</bash-input>".into(),
            prompt_ingress_kind: Some(crate::request_context::PromptIngressKind::DirectBash),
            ..Default::default()
        },
    ];
    let outcome = orch
        .run_queued_prompt_batch_with_admission(
            inputs,
            cancel,
            Box::new(move || {
                let _ = admitted_tx.send(());
            }),
        )
        .await
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Cancelled));
    admitted_rx.await.unwrap();
    assert_eq!(orch.snapshot_history().await.len(), 2);
    let source = std::fs::read_to_string(path).unwrap();
    assert!(source.contains("direct Bash result follows"));
    assert!(source.contains("printf done"));
}
