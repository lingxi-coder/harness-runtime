//! Workspace moves and warm resume must retain one authoritative transcript.
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::SessionId;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn transcript(home: &Path, cwd: &Path, id: SessionId) -> PathBuf {
    session::jsonl::session_path(home, &cwd.to_string_lossy(), &id.as_uuid().to_string())
}

async fn fixture(
    home: &Path,
    cwd: &Path,
    id: SessionId,
) -> (
    Arc<ConversationOrchestrator>,
    Arc<session::JsonlWriter>,
    Arc<tool_api::SessionCwd>,
    tool_api::WorktreeSessionCell,
) {
    std::fs::create_dir_all(cwd).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(cwd.to_path_buf()));
    let writer = Arc::new(session::JsonlWriter::new(transcript(home, cwd, id), fs));
    let cell = tool_api::SessionCwd::new(cwd.to_path_buf(), vec![cwd.to_path_buf()]);
    let worktree = tool_api::worktree_session::new_worktree_session_cell();
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tool_api::registry::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.to_path_buf(),
        )
        .with_config_home(home.to_path_buf())
        .with_jsonl_writer(writer.clone())
        .with_session_cwd(cell.clone())
        .with_worktree_session(worktree.clone()),
    );
    orch.session().lock().await.session_id = id;
    orch.attach_owned_session_switches();
    (orch, writer, cell, worktree)
}

fn write_source(path: &Path, cwd: &Path, id: SessionId) -> String {
    let uuid = uuid::Uuid::new_v4();
    let content = format!(
        "{}\n",
        serde_json::json!({
            "type":"user", "uuid":uuid, "parentUuid":null, "cwd":cwd,
            "sessionId":id.as_uuid(), "timestamp":"2026-10-21T12:00:00.000Z",
            "message":{"role":"user", "content":"source-history"}
        })
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, &content).unwrap();
    content
}

#[tokio::test]
async fn selected_transcript_path_wins_over_a_current_bucket_copy_of_the_same_uuid() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let current = root.path().join("current");
    let selected_cwd = root.path().join("selected");
    std::fs::create_dir_all(&selected_cwd).unwrap();
    let session_id = SessionId::new();
    let (orch, writer, cwd, _) = fixture(&home, &current, SessionId::new()).await;
    let current_path = transcript(&home, &current, session_id);
    let current_content = write_source(&current_path, &current, session_id);
    let selected_path = transcript(&home, &selected_cwd, session_id);
    let mut selected_content = write_source(&selected_path, &selected_cwd, session_id)
        .replace("source-history", "selected-copy-history");
    selected_content.push_str(&format!(
        "{}\n",
        serde_json::json!({"type":"custom-title","sessionId":session_id.as_uuid(),"customTitle":"selected-copy-title"})
    ));
    std::fs::write(&selected_path, selected_content).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(current.clone()));
    let by_uuid = orchestrator::resume::replay_session_state(
        &home,
        &current.to_string_lossy(),
        session_id.as_uuid(),
        fs.clone(),
    )
    .await
    .unwrap();
    assert_eq!(by_uuid.transcript_path, current_path);
    let selected = orchestrator::resume::replay_session_state_from_path(
        &selected_path,
        session_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    assert!(selected
        .display_history
        .iter()
        .any(|message| message.text_content().contains("selected-copy-history")));
    assert_eq!(selected.transcript_path, selected_path);
    assert!(selected
        .transcript_metadata
        .iter()
        .any(|row| { row.string_at("/customTitle") == Some("selected-copy-title") }));
    let runtime = selected.handle_runtime_snapshot();
    orch.resume_session(
        session_id,
        selected.state.history,
        selected.last_message_uuid.map(|id| id.to_string()),
        None,
        runtime,
    )
    .await
    .unwrap();
    orch.append_meta_user_message(&Utf16JsonProjection::from("selected-copy-append"))
        .await
        .unwrap();
    assert_eq!(writer.active_path(), selected_path);
    assert_eq!(cwd.cwd(), selected_cwd);
    assert_eq!(
        std::fs::read_to_string(current_path).unwrap(),
        current_content
    );
    assert!(std::fs::read_to_string(selected_path)
        .unwrap()
        .contains("selected-copy-append"));
}

#[tokio::test]
async fn selected_transcript_with_a_different_filename_uuid_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let current = root.path().join("current");
    let selected_id = SessionId::new();
    let selected_path = transcript(&home, &current, selected_id);
    write_source(&selected_path, &current, selected_id);
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(current));
    let error = orchestrator::resume::replay_session_state_from_path(
        &selected_path,
        SessionId::new().as_uuid(),
        fs,
    )
    .await
    .unwrap_err();
    assert!(matches!(
        error,
        orchestrator::resume::ResumeError::Loader(
            session::jsonl::LoaderError::SessionNotFound { .. }
        )
    ));
}

#[tokio::test]
async fn failed_resume_identity_import_keeps_the_previous_session_and_writer_active() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let source = root.path().join("source");
    std::fs::create_dir(&source).unwrap();
    let previous_id = SessionId::new();
    let (orch, writer, cwd, _) = fixture(&home, &boot, previous_id).await;
    let previous_path = writer.active_path();
    let selected_id = SessionId::new();
    let selected_path = transcript(&home, &source, selected_id);
    let original = write_source(&selected_path, &source, selected_id);
    let identity_log = selected_path.with_file_name(format!(
        "{}{}",
        selected_path.file_name().unwrap().to_string_lossy(),
        branding::SESSION_MESSAGE_IDENTITY_LOG_SUFFIX
    ));
    std::fs::write(identity_log, "invalid identity record\n").unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(boot.clone()));
    let selected = orchestrator::resume::replay_session_state_from_path(
        &selected_path,
        selected_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    let snapshot = selected.handle_runtime_snapshot();
    let error = orch
        .resume_session(selected_id, selected.state.history, None, None, snapshot)
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("Transcript identity import failed"));
    assert_eq!(orch.current_session_id().await, previous_id);
    assert_eq!(writer.active_path(), previous_path);
    assert_eq!(cwd.cwd(), boot);
    assert_eq!(std::fs::read_to_string(selected_path).unwrap(), original);
}

#[tokio::test]
async fn warm_resume_appends_to_selected_source_and_restores_existing_workspace() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let source = root.path().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let source_id = SessionId::new();
    let selected = transcript(&home, &source, source_id);
    write_source(&selected, &source, source_id);
    let (orch, writer, cwd, _) = fixture(&home, &boot, SessionId::new()).await;
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(boot.clone()));
    let replayed = orchestrator::resume::replay_session_state(
        &home,
        &boot.to_string_lossy(),
        source_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    let runtime = replayed.handle_runtime_snapshot();
    assert_eq!(runtime.transcript_path.as_ref(), Some(&selected));
    orch.resume_session(
        source_id,
        replayed.state.history,
        replayed.last_message_uuid.map(|id| id.to_string()),
        None,
        runtime,
    )
    .await
    .unwrap();
    orch.append_meta_user_message(&Utf16JsonProjection::from("after-warm-resume"))
        .await
        .unwrap();
    assert_eq!(writer.active_path(), selected);
    assert!(std::fs::read_to_string(&selected)
        .unwrap()
        .contains("after-warm-resume"));
    assert!(!transcript(&home, &boot, source_id).exists());
    assert_eq!(cwd.cwd(), source);
    assert_eq!(orch.current_cwd(), source);
    assert_eq!(
        orch.project_root(),
        boot,
        "hook project root remains the original composition root"
    );
}

#[tokio::test]
async fn warm_resume_missing_workspace_keeps_current_cwd_and_source_transcript() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let missing = root.path().join("missing");
    let source_id = SessionId::new();
    let selected = transcript(&home, &missing, source_id);
    write_source(&selected, &missing, source_id);
    let (orch, writer, cwd, _) = fixture(&home, &boot, SessionId::new()).await;
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(boot.clone()));
    let replayed = orchestrator::resume::replay_session_state(
        &home,
        &boot.to_string_lossy(),
        source_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    let runtime = replayed.handle_runtime_snapshot();
    assert_eq!(runtime.working_directory, None);
    orch.resume_session(source_id, replayed.state.history, None, None, runtime)
        .await
        .unwrap();
    orch.append_meta_user_message(&Utf16JsonProjection::from("missing-workspace-resume"))
        .await
        .unwrap();
    assert_eq!(writer.active_path(), selected);
    assert!(std::fs::read_to_string(&selected)
        .unwrap()
        .contains("missing-workspace-resume"));
    assert_eq!(cwd.cwd(), boot);
}

#[tokio::test]
async fn change_working_directory_moves_complete_jsonl_and_sidecars_before_publishing_cwd() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let id = SessionId::new();
    let (orch, writer, cwd, _) = fixture(&home, &boot, id).await;
    let old = transcript(&home, &boot, id);
    let original = write_source(&old, &boot, id);
    let old_sidecar = old.parent().unwrap().join(id.as_uuid().to_string());
    std::fs::create_dir_all(old_sidecar.join("nested")).unwrap();
    std::fs::write(old_sidecar.join("nested/output"), "complete-artifact").unwrap();
    let additional = root.path().join("explicit");
    orch.change_working_directory(target.clone(), vec![target.clone(), additional.clone()])
        .await
        .unwrap();
    let moved = transcript(&home, &target, id);
    assert_eq!(writer.active_path(), moved);
    assert!(!old.exists());
    assert!(std::fs::read_to_string(&moved)
        .unwrap()
        .starts_with(&original));
    let routed = session::jsonl::route_lines(&std::fs::read_to_string(&moved).unwrap());
    assert_eq!(
        routed.relocated_cwds[&id.as_uuid().to_string()],
        target.to_string_lossy()
    );
    assert_eq!(
        std::fs::read_to_string(
            moved
                .parent()
                .unwrap()
                .join(id.as_uuid().to_string())
                .join("nested/output")
        )
        .unwrap(),
        "complete-artifact"
    );
    assert!(!old_sidecar.exists());
    assert_eq!(
        cwd.snapshot(),
        (target.clone(), vec![target.clone(), additional])
    );
    assert_eq!(orch.current_cwd(), target);
    assert_eq!(orch.project_root(), boot);
}

#[tokio::test]
async fn failed_transcript_move_rolls_back_sidecars_and_does_not_publish_cwd() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let target = root.path().join("target");
    std::fs::create_dir_all(&target).unwrap();
    let id = SessionId::new();
    let (orch, writer, cwd, _) = fixture(&home, &boot, id).await;
    let old = transcript(&home, &boot, id);
    std::fs::create_dir_all(&old).unwrap(); // Invalid transcript source forces writer rollback.
    let new = transcript(&home, &target, id);
    let old_sidecar = old.parent().unwrap().join(id.as_uuid().to_string());
    let new_sidecar = new.parent().unwrap().join(id.as_uuid().to_string());
    std::fs::create_dir_all(&old_sidecar).unwrap();
    std::fs::create_dir_all(&new_sidecar).unwrap();
    std::fs::write(old_sidecar.join("source"), "source-artifact").unwrap();
    std::fs::write(new_sidecar.join("destination"), "prior-artifact").unwrap();
    assert!(orch
        .change_working_directory(target.clone(), vec![target])
        .await
        .is_err());
    assert_eq!(writer.active_path(), old);
    assert_eq!(cwd.cwd(), boot);
    assert_eq!(orch.current_cwd(), boot);
    assert_eq!(
        std::fs::read_to_string(old_sidecar.join("source")).unwrap(),
        "source-artifact"
    );
    assert_eq!(
        std::fs::read_to_string(new_sidecar.join("destination")).unwrap(),
        "prior-artifact"
    );
}

#[tokio::test]
async fn warm_resume_replaces_shared_worktree_state_and_preserves_explicit_dirs() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let worktree = root.path().join("worktree");
    let extra = root.path().join("explicit");
    std::fs::create_dir_all(&worktree).unwrap();
    let source_id = SessionId::new();
    let selected = transcript(&home, &boot, source_id);
    let mut content = write_source(&selected, &boot, source_id);
    content.push_str(&format!(
        "{}\n",
        serde_json::json!({
            "type":"worktree-state", "sessionId":source_id.as_uuid(),
            "worktreeSession":{"originalCwd":boot,"worktreePath":worktree,"worktreeBranch":"branch"}
        })
    ));
    std::fs::write(&selected, content).unwrap();
    let (orch, _, cwd, state) = fixture(&home, &boot, SessionId::new()).await;
    cwd.add_trusted_dir(extra.clone());
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(boot.clone()));
    let replayed = orchestrator::resume::replay_session_state(
        &home,
        &boot.to_string_lossy(),
        source_id.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    let runtime = replayed.handle_runtime_snapshot();
    orch.resume_session(source_id, replayed.state.history, None, None, runtime)
        .await
        .unwrap();
    assert_eq!(
        state.lock().unwrap().as_ref().unwrap().worktree_path,
        worktree
    );
    assert_eq!(cwd.cwd(), worktree);
    assert!(cwd.trusted_dirs().contains(&extra));
    orch.resume_session(
        SessionId::new(),
        Vec::new(),
        None,
        None,
        lingxi_core::host::ResumeRuntimeSnapshot {
            working_directory: Some(boot.clone()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        state.lock().unwrap().is_none(),
        "a session without worktree state cannot inherit the previous worktree"
    );
    assert_eq!(cwd.cwd(), boot);
    assert!(cwd.trusted_dirs().contains(&extra));
}

#[tokio::test]
async fn warm_resume_after_cd_cannot_reappend_the_previous_sessions_metadata() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let boot = root.path().join("boot");
    let moved = root.path().join("moved-a");
    let source = root.path().join("source-b");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::create_dir_all(&source).unwrap();
    let id_a = SessionId::new();
    let id_b = SessionId::new();
    let (orch, writer, _, _) = fixture(&home, &boot, id_a).await;
    write_source(&transcript(&home, &boot, id_a), &boot, id_a);
    writer
        .append_custom_title(&id_a.as_uuid().to_string(), "A title".into())
        .await
        .unwrap();
    writer.append_permission_mode("plan").await.unwrap();
    writer.append_session_mode("A mode").await.unwrap();
    orch.change_working_directory(moved.clone(), vec![moved.clone()])
        .await
        .unwrap();
    let path_b = transcript(&home, &source, id_b);
    let mut raw = write_source(&path_b, &source, id_b);
    raw.push_str(&format!("{}\n", serde_json::json!({"type":"custom-title","customTitle":"B preserved title","sessionId":id_b.as_uuid()})));
    raw.push_str(&format!("{}\n", serde_json::json!({"type":"system","uuid":uuid::Uuid::new_v4(),"sessionId":id_b.as_uuid(),"cwd":source,"message":{"content":"padding".repeat(12_000)}})));
    std::fs::write(&path_b, raw).unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::PosixFileSystem::new(moved.clone()));
    let replayed = orchestrator::resume::replay_session_state(
        &home,
        &moved.to_string_lossy(),
        id_b.as_uuid(),
        fs,
    )
    .await
    .unwrap();
    let runtime = replayed.handle_runtime_snapshot();
    orch.resume_session(id_b, replayed.state.history, None, None, runtime)
        .await
        .unwrap();
    orch.append_meta_user_message(&Utf16JsonProjection::from("after-resume".repeat(4000)))
        .await
        .unwrap();
    let tail = session::jsonl::read_tail(&path_b);
    assert!(tail.contains("B preserved title"));
    assert!(!tail.contains("A title"));
    assert!(!tail.contains("A mode"));
    assert!(!tail.contains(r#""permissionMode":"plan""#));
    assert!(!tail.contains(&moved.to_string_lossy().to_string()));
}
