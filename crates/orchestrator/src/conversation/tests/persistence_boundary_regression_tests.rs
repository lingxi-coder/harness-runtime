use super::*;
use crate::test_support::{
    noop_hook_executor, with_scripted_compactor, MockApiClient, MockOutputStream,
    NoOpPermissionGate, StaticMemoryProvider,
};
use lingxi_core::types::{ContentBlock, MessageId, SessionId};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tool_api::tool_trait::Tool;

struct BoundaryEnospcFs {
    inner: platform_posix::fs::PosixFileSystem,
    fail_boundary: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl lingxi_core::host::FileSystem for BoundaryEnospcFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::FsError> {
        self.inner.read_file(path, offset, limit).await
    }
    async fn read_file_rooted_no_follow(
        &self,
        root: &Path,
        relative: &Path,
    ) -> Result<lingxi_core::host::filesystem::FileContent, lingxi_core::host::FsError> {
        self.inner.read_file_rooted_no_follow(root, relative).await
    }
    async fn write_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        self.inner.write_file(path, content).await
    }
    fn is_within_workspace(&self, path: &str) -> bool {
        self.inner.is_within_workspace(path)
    }
    async fn watch(
        &self,
        path: &str,
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = lingxi_core::host::filesystem::FileEvent> + Send>,
        >,
        lingxi_core::host::FsError,
    > {
        self.inner.watch(path).await
    }
    async fn append_file(
        &self,
        path: &str,
        content: &str,
    ) -> Result<(), lingxi_core::host::FsError> {
        if content.contains("compact_boundary")
            && self
                .fail_boundary
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return Err(lingxi_core::host::FsError::Io(
                "ENOSPC: injected compact-boundary append failure".into(),
            ));
        }
        self.inner.append_file(path, content).await
    }
    async fn truncate(&self, path: &str, size: u64) -> Result<(), lingxi_core::host::FsError> {
        self.inner.truncate(path, size).await
    }
    async fn file_mtime(
        &self,
        path: &str,
    ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
        self.inner.file_mtime(path).await
    }
    async fn file_size(&self, path: &str) -> Result<u64, lingxi_core::host::FsError> {
        self.inner.file_size(path).await
    }
    async fn delete_file(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.delete_file(path).await
    }
    async fn symlink(&self, target: &str, link: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.symlink(target, link).await
    }
    async fn flock_exclusive(
        &self,
        path: &str,
    ) -> Result<Box<dyn lingxi_core::host::filesystem::FlockGuard>, lingxi_core::host::FsError>
    {
        self.inner.flock_exclusive(path).await
    }
    async fn fsync(&self, path: &str) -> Result<(), lingxi_core::host::FsError> {
        self.inner.fsync(path).await
    }
}

fn fixture(cwd: &Path) -> ConversationOrchestrator {
    let context = tool_api::test_support::ctx_for_file_tools(
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.to_path_buf())),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.to_path_buf()],
    );
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(Arc::new(tool_file::FileReadTool::new(context)));
    ConversationOrchestrator::new(
        crate::OrchestratorConfig {
            system_prompt_override: Some("Isolated persistence fixture.".into()),
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
    .without_host_environment_probes()
}

fn shape(history: &[ConversationMessage]) -> Vec<(&'static str, String)> {
    history
        .iter()
        .map(|message| {
            let role = match message {
                ConversationMessage::User { .. } => "user",
                ConversationMessage::Assistant { .. } => "assistant",
                ConversationMessage::System { .. } => "system",
            };
            (role, message.text_content())
        })
        .collect()
}

async fn assert_mounted_history_matches_transcript(
    orch: &ConversationOrchestrator,
    path: &Path,
    session: SessionId,
) {
    let source = std::fs::read_to_string(path).unwrap();
    let replayed = crate::resume::replay_captured_session(
        path.to_path_buf(),
        source.clone(),
        session::jsonl::route_lines(&source),
        session.as_uuid(),
    )
    .unwrap();
    assert_eq!(
        shape(&replayed.state.history),
        shape(&orch.snapshot_history().await)
    );
    assert_eq!(
        replayed.last_message_uuid.map(|id| id.to_string()),
        *orch.transcript.last_jsonl_uuid.lock().await,
        "the next append must extend the same post-compact chain"
    );
}

#[tokio::test]
async fn selector_compaction_preserves_order_and_tip_across_cold_resume_and_next_append() {
    for direction in [
        compaction::prompt::SummarizeDirection::From,
        compaction::prompt::SummarizeDirection::UpTo,
    ] {
        let root = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(root.path()).unwrap();
        let id = SessionId::new();
        let path = cwd.join(format!("{}.jsonl", id.as_uuid()));
        let writer = Arc::new(session::JsonlWriter::new(
            path.clone(),
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        ));
        let orch = with_scripted_compactor(
            fixture(&cwd).with_session_id(id).with_jsonl_writer(writer),
            "SELECTED-HALF-SUMMARY",
        );
        orch.initialize_conversation_mode().await.unwrap();
        let mut ids = Vec::new();
        for index in 0..6 {
            let message_id = MessageId::new();
            ids.push(message_id);
            if index % 2 == 0 {
                orch.append_external_history_message(ConversationMessage::user(
                    message_id,
                    format!("USER-{index}"),
                ))
                .await;
            } else {
                // Exercise logical-to-physical preserved endpoints too: each
                // assistant is durably split into multiple content-block rows.
                let assistant = ConversationMessage::Assistant {
                    id: message_id,
                    content: vec![
                        ContentBlock::Text {
                            text: format!("ASSISTANT-{index}"),
                            citations: None,
                        },
                        ContentBlock::Text {
                            text: " ordered tail 🙂".into(),
                            citations: None,
                        },
                    ],
                    stop_reason: Some("end_turn".into()),
                    resumable: None,
                    per_turn_effort: None,
                };
                orch.session.lock().await.history.push(assistant.clone());
                orch.persist_assistant_per_block(&assistant, None, None, None)
                    .await;
            }
        }
        orch.summarize_at(
            &ids[3].to_string(),
            None,
            direction,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_mounted_history_matches_transcript(&orch, &path, id).await;

        orch.append_external_history_message(ConversationMessage::user(
            MessageId::new(),
            "NEXT-PROMPT".into(),
        ))
        .await;
        assert_mounted_history_matches_transcript(&orch, &path, id).await;
    }
}

#[tokio::test]
async fn ordinary_compaction_enospc_retains_boundary_for_recovery_and_next_append() {
    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let session = SessionId::new();
    let path = cwd.join(format!("{}.jsonl", session.as_uuid()));
    let fs = Arc::new(BoundaryEnospcFs {
        inner: platform_posix::fs::PosixFileSystem::new(cwd.clone()),
        fail_boundary: std::sync::atomic::AtomicBool::new(false),
    });
    let writer = Arc::new(session::JsonlWriter::new(path.clone(), fs.clone()));
    let orch = with_scripted_compactor(
        fixture(&cwd)
            .with_session_id(session)
            .with_jsonl_writer(writer.clone()),
        "RECOVERED-COMPACT-SUMMARY",
    );
    orch.initialize_conversation_mode().await.unwrap();
    for index in 0..6 {
        let message = if index % 2 == 0 {
            ConversationMessage::user(MessageId::new(), format!("ORIGINAL-USER-{index}"))
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![ContentBlock::Text {
                    text: format!("ORIGINAL-ANSWER-{index}"),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                resumable: None,
                per_turn_effort: None,
            }
        };
        orch.append_external_history_message(message).await;
    }
    let kept_suffix_tip = orch.transcript.last_jsonl_uuid.lock().await.clone();
    assert!(kept_suffix_tip.is_some());
    fs.fail_boundary
        .store(true, std::sync::atomic::Ordering::Release);
    orch.force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();
    assert!(orch.check_storage_pause().await.is_err());
    assert!(!fs.fail_boundary.load(std::sync::atomic::Ordering::Acquire));
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("compact_boundary"));
    assert_eq!(
        *orch.transcript.last_jsonl_uuid.lock().await,
        kept_suffix_tip,
        "ordinary compaction relinks its logical tip to the preserved suffix"
    );

    // Inject failure through the compatibility filesystem, then bind recovery
    // to the same existing file's durable authority, as production retry uses.
    let state = cwd.join("recovery-state");
    std::fs::create_dir(&state).unwrap();
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(state).unwrap());
    let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
        cwd.clone(),
        path.strip_prefix(&cwd).unwrap().to_path_buf(),
    )
    .unwrap();
    writer
        .activate_session_target_with_durable_lock_and_authority(
            session,
            path.clone(),
            cwd.clone(),
            durable,
            Some(authority),
        )
        .unwrap();
    orch.recover_session_storage().await.unwrap();
    orch.check_storage_pause().await.unwrap();
    assert_eq!(
        *orch.transcript.last_jsonl_uuid.lock().await,
        kept_suffix_tip,
        "recovery must retain the compacted logical tip rather than the physical summary tip"
    );
    assert_mounted_history_matches_transcript(&orch, &path, session).await;
    let saved = std::fs::read_to_string(&path).unwrap();
    assert_eq!(saved.matches("\"subtype\":\"compact_boundary\"").count(), 1);
    orch.recover_session_storage().await.unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);

    orch.append_external_history_message(ConversationMessage::user(
        MessageId::new(),
        "NEXT-AFTER-RECOVERY".into(),
    ))
    .await;
    assert_mounted_history_matches_transcript(&orch, &path, session).await;
}

struct EditAtRestoreEvent(PathBuf);

#[async_trait::async_trait]
impl telemetry::AnalyticsSink for EditAtRestoreEvent {
    async fn log_event(&self, name: &str, _: telemetry::LogEventMetadata) {
        if name == "tengu_post_compact_file_restore_success" {
            // This existing awaited event is after the bytes were read. Model
            // an editor/linter update before restoration publishes its cache.
            std::fs::write(&self.0, "EXTERNAL-EDIT-B").unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(&self.0)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(
                        std::time::SystemTime::now() + std::time::Duration::from_secs(5),
                    ),
                )
                .unwrap();
        }
    }

    async fn log_event_async(&self, name: &str, metadata: telemetry::LogEventMetadata) {
        self.log_event(name, metadata).await;
    }

    fn name(&self) -> &str {
        "local-restore-editor"
    }
}

#[tokio::test]
async fn restored_file_does_not_bless_an_editor_update_with_the_old_contents() {
    use tool_api::read_file_state as reads;

    let root = tempfile::tempdir().unwrap();
    let cwd = std::fs::canonicalize(root.path()).unwrap();
    let path = cwd.join("working.txt");
    std::fs::write(&path, "READ-CONTENT-A").unwrap();
    let map = reads::new_read_file_state_map();
    reads::set(
        &map,
        path.clone(),
        reads::ReadFileEntry {
            content: "READ-CONTENT-A".into(),
            mtime_ms: 1,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        },
    );
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    bus.attach_sink(Arc::new(EditAtRestoreEvent(path.clone())))
        .await;
    let orch = with_scripted_compactor(
        fixture(&cwd)
            .with_read_state_map(map.clone())
            .with_analytics_bus(bus),
        "summary",
    );
    for index in 0..6 {
        let id = MessageId::new();
        let message = if index % 2 == 0 {
            ConversationMessage::user(id, format!("user-{index}"))
        } else {
            ConversationMessage::Assistant {
                id,
                content: vec![ContentBlock::Text {
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
    orch.force_compact_with_cancel(CancellationToken::new())
        .await
        .unwrap();

    let current = std::fs::read_to_string(&path).unwrap();
    let mtime = reads::mtime_ms_floor(std::fs::metadata(&path).unwrap().modified().unwrap());
    let cached = reads::get(&map, &path).unwrap();
    assert_eq!(cached.content, "READ-CONTENT-A");
    assert_eq!(current, "EXTERNAL-EDIT-B");
    assert!(cached.mtime_ms < mtime);
    assert!(tool_file::check_read_before_write(&map, &path, mtime, &current, false).is_err());

    let fs = Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
    let mut context = tool_api::test_support::ctx_for_file_tools(
        fs,
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd],
    );
    context.read_file_state = map;
    let reread = tool_file::FileReadTool::new(context)
        .call(
            CompactSource::from_serde_value(&serde_json::json!({"file_path": path})).unwrap(),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await
        .unwrap();
    assert_eq!(reread.data.string_at("/type"), Some("text"));
    assert_eq!(
        reread.data.string_at("/file/content"),
        Some("EXTERNAL-EDIT-B")
    );
}
