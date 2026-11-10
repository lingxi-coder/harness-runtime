use super::*;
use crate::test_support::{
    noop_hook_executor, with_scripted_compactor, MockApiClient, MockOutputStream,
    NoOpPermissionGate, StaticMemoryProvider,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tool_api::read_file_state::{self as reads, ReadFileStateMap};
use tool_api::tool_trait::{Tool, ToolError};

fn fixture(
    cwd: &Path,
    fs: Arc<dyn lingxi_core::host::FileSystem>,
) -> (
    ConversationOrchestrator,
    Arc<tool_file::FileReadTool>,
    ReadFileStateMap,
) {
    let map = reads::new_read_file_state_map();
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let mut context =
        tool_api::test_support::ctx_for_file_tools(fs, bus.clone(), vec![cwd.to_path_buf()]);
    context.read_file_state = map.clone();
    let reader = Arc::new(tool_file::FileReadTool::new(context));
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(reader.clone());
    let orch = with_scripted_compactor(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig {
                system_prompt_override: Some("Isolated file restoration fixture.".into()),
                ..Default::default()
            },
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.to_path_buf(),
        )
        .with_analytics_bus(bus)
        .with_read_state_map(map.clone())
        .without_host_environment_probes(),
        "FILE-RESTORATION-SUMMARY",
    );
    (orch, reader, map)
}

async fn read(
    reader: &tool_file::FileReadTool,
    path: &Path,
) -> Result<tool_api::tool_trait::ToolCallResult, ToolError> {
    reader
        .call(
            CompactSource::from_serde_value(&serde_json::json!({"file_path": path})).unwrap(),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await
}

async fn seed_compactable_history(orch: &ConversationOrchestrator) {
    for index in 0..6 {
        let message = if index % 2 == 0 {
            ConversationMessage::user(MessageId::new(), format!("question-{index}"))
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![lingxi_core::types::ContentBlock::Text {
                    text: format!("answer-{index}"),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                resumable: None,
                per_turn_effort: None,
            }
        };
        orch.append_external_history_message(message).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn actual_read_retargeted_outside_read_roots_is_not_restored_by_compaction() {
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let allowed = repo.join("allowed.txt");
    let secret = root.path().join("secret.txt");
    let link = repo.join("working.txt");
    std::fs::write(&allowed, "ALLOWED-READ-CONTENT").unwrap();
    std::fs::write(&secret, "SECRET-MUST-NOT-ENTER-MODEL-CONTEXT").unwrap();
    std::os::unix::fs::symlink(&allowed, &link).unwrap();
    let (orch, reader, map) = fixture(
        &repo,
        Arc::new(platform_posix::fs::PosixFileSystem::new(repo.clone())),
    );
    assert_eq!(
        read(&reader, &link).await.unwrap().data.string_at("/type"),
        Some("text")
    );
    assert!(reads::get(&map, &link).unwrap().from_read);
    seed_compactable_history(&orch).await;
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&secret, &link).unwrap();
    assert!(matches!(
        read(&reader, &link).await,
        Err(ToolError::PathBlocked { .. })
    ));

    orch.force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();
    assert!(reads::get(&map, &link).is_none());
    assert!(orch.snapshot_history().await.iter().all(|message| {
        !message
            .text_content()
            .contains("SECRET-MUST-NOT-ENTER-MODEL-CONTEXT")
    }));
    assert!(!orch
        .transcript
        .model_reminder_attachments
        .lock()
        .unwrap()
        .values()
        .any(|data| { data.string_at("/filename") == Some(link.to_string_lossy().as_ref()) }));
}

#[tokio::test]
async fn compaction_restores_translated_guest_reads_with_bounded_current_contents() {
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let host_path = cwd.join("working.txt");
    let guest_root = format!("/compact-guest-{}", uuid::Uuid::new_v4());
    let guest_path = PathBuf::from(&guest_root).join("working.txt");
    let fs = tool_api::test_support::make_guest_alias_fs(&guest_root, &cwd, "/compact-fenced");
    let (orch, reader, map) = fixture(&cwd, fs);
    std::fs::write(&host_path, "OLD-GUEST-CONTENT").unwrap();
    read(&reader, &guest_path).await.unwrap();
    seed_compactable_history(&orch).await;
    std::fs::write(&host_path, "\u{feff}CURRENT-GUEST\r\nTAIL\r").unwrap();
    orch.force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();
    let entry = reads::get(&map, &guest_path).unwrap();
    assert_eq!(entry.content, "CURRENT-GUEST\nTAIL");
    assert_eq!(
        entry.mtime_ms,
        reads::mtime_ms_floor(std::fs::metadata(&host_path).unwrap().modified().unwrap())
    );
    assert!(entry.from_read);
    assert!(orch
        .snapshot_history()
        .await
        .iter()
        .any(|message| message.text_content().contains("CURRENT-GUEST")));

    // The namespace adapter panics on unbounded FileSystem::read_file. A large
    // translated file must instead use the approved handle's bounded prefix
    // and become a reference, without claiming a full model-visible Read.
    std::fs::write(
        &host_path,
        vec![b'x'; tool_file::read::MAX_FILE_READ_SIZE as usize * 4],
    )
    .unwrap();
    let restored = orch.restore_post_compact_attachments_against(&[]).await;
    assert_eq!(restored.len(), 1);
    assert!(reads::get(&map, &guest_path).is_none());
    let attachments = orch.transcript.model_reminder_attachments.lock().unwrap();
    assert_eq!(
        attachments[&restored[0].id()].string_at("/type"),
        Some("compact_file_reference")
    );
}

#[tokio::test]
async fn current_translation_fence_blocks_a_previously_cached_host_spelling() {
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let fenced = cwd.join("fenced.txt");
    std::fs::write(&fenced, "FENCED-MUST-NOT-ENTER-MODEL-CONTEXT").unwrap();
    let fs = tool_api::test_support::make_guest_alias_fs(
        "/compact-guest",
        &cwd,
        &fenced.to_string_lossy(),
    );
    let (orch, reader, map) = fixture(&cwd, fs);
    reads::set(
        &map,
        fenced.clone(),
        reads::ReadFileEntry {
            content: "previously permitted".into(),
            mtime_ms: 1,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
    assert!(matches!(
        read(&reader, &fenced).await,
        Err(ToolError::InvalidInput(_))
    ));
    seed_compactable_history(&orch).await;
    orch.force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();
    assert!(reads::get(&map, &fenced).is_none());
    assert!(orch.snapshot_history().await.iter().all(|message| {
        !message
            .text_content()
            .contains("FENCED-MUST-NOT-ENTER-MODEL-CONTEXT")
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn actual_read_replaced_by_fifo_does_not_block_compaction() {
    use std::os::unix::fs::OpenOptionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let path = cwd.join("working.txt");
    std::fs::write(&path, "PREVIOUS-REGULAR-FILE").unwrap();
    let (orch, reader, map) = fixture(
        &cwd,
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
    );
    read(&reader, &path).await.unwrap();
    seed_compactable_history(&orch).await;
    std::fs::remove_file(&path).unwrap();
    assert!(std::process::Command::new("mkfifo")
        .arg(&path)
        .status()
        .unwrap()
        .success());
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        orch.force_compact_with_cancel(CancellationToken::new()),
    )
    .await;
    if completed.is_err() {
        // Unblock the old raw-open implementation so a failing regression
        // terminates instead of leaving Tokio's blocking pool stuck at exit.
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&path);
    }
    completed
        .expect("special-file restoration must finish without a FIFO writer")
        .unwrap();
    assert!(reads::get(&map, &path).is_none());
}

#[tokio::test]
async fn absent_read_capability_drops_cached_file_instead_of_opening_raw_path() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("cached.txt");
    std::fs::write(&path, "NO-RAW-FALLBACK").unwrap();
    let map = reads::new_read_file_state_map();
    reads::set(
        &map,
        path,
        reads::ReadFileEntry {
            content: "previous".into(),
            mtime_ms: 1,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
    let orch = ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    )
    .with_read_state_map(map.clone())
    .without_host_environment_probes();
    assert!(orch
        .restore_post_compact_attachments_against(&[])
        .await
        .is_empty());
    assert!(map.lock().unwrap().is_empty());
}
