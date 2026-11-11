use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use async_trait::async_trait;
use lingxi_core::host::filesystem::{FileContent, FileEvent, FlockGuard};
use lingxi_core::host::{CostSnapshot, FileSystem, FsError, OrchestratorHandle, OutputStream};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::ToolUseId;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct AppendFailureFs {
    inner: platform_posix::fs::PosixFileSystem,
    fail: AtomicBool,
}
#[async_trait]
impl FileSystem for AppendFailureFs {
    async fn read_file(
        &self,
        p: &str,
        o: Option<u64>,
        l: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file(p, o, l).await
    }
    async fn read_file_rooted_no_follow(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file_rooted_no_follow(root, relative).await
    }
    async fn write_file(&self, p: &str, c: &str) -> Result<(), FsError> {
        self.inner.write_file(p, c).await
    }
    fn is_within_workspace(&self, p: &str) -> bool {
        self.inner.is_within_workspace(p)
    }
    async fn watch(
        &self,
        p: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        self.inner.watch(p).await
    }
    async fn append_file(&self, p: &str, c: &str) -> Result<(), FsError> {
        if self.fail.load(Ordering::Acquire) {
            return Err(FsError::Io("ENOSPC: injected goal append failure".into()));
        }
        self.inner.append_file(p, c).await
    }
    async fn truncate(&self, p: &str, n: u64) -> Result<(), FsError> {
        self.inner.truncate(p, n).await
    }
    async fn file_mtime(&self, p: &str) -> Result<std::time::SystemTime, FsError> {
        self.inner.file_mtime(p).await
    }
    async fn file_size(&self, p: &str) -> Result<u64, FsError> {
        self.inner.file_size(p).await
    }
    async fn delete_file(&self, p: &str) -> Result<(), FsError> {
        self.inner.delete_file(p).await
    }
    async fn symlink(&self, t: &str, p: &str) -> Result<(), FsError> {
        self.inner.symlink(t, p).await
    }
    async fn flock_exclusive(&self, p: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        self.inner.flock_exclusive(p).await
    }
    async fn fsync(&self, p: &str) -> Result<(), FsError> {
        self.inner.fsync(p).await
    }
}

#[derive(Default)]
struct ReentrantStorageOutput {
    owner: std::sync::Mutex<Option<std::sync::Weak<ConversationOrchestrator>>>,
    paused: AtomicUsize,
    recovered: AtomicUsize,
    reenter_recovered: AtomicBool,
    notices: AtomicUsize,
}

fn bind_recovery_transcript(
    orch: &ConversationOrchestrator,
    root: &std::path::Path,
    id: SessionId,
) {
    // The legacy filesystem seam supplies the deterministic append failure.
    // Native recovery requires the same concrete file's durable session owner.
    let writer = orch.transcript.jsonl_writer.as_ref().unwrap();
    let path = writer.active_path();
    let state = root.join("recovery-state");
    std::fs::create_dir(&state).unwrap();
    let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(state).unwrap());
    let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
        root.to_path_buf(),
        path.strip_prefix(root).unwrap().to_path_buf(),
    )
    .unwrap();
    writer
        .activate_session_target_with_durable_lock_and_authority(
            id,
            path.clone(),
            root.to_path_buf(),
            durable,
            Some(authority),
        )
        .unwrap();
    assert_eq!(writer.session_target_path(id), Some(path));
    assert!(writer.active_transcript_authority().is_some());
}

#[async_trait]
impl OutputStream for ReentrantStorageOutput {
    async fn emit_text(&self, _: &Utf16JsonProjection) {}
    async fn emit_tool_call(&self, _: &ToolUseId, _: &str, _: &Utf16JsonProjection) {}
    async fn emit_tool_result(
        &self,
        _: &ToolUseId,
        _: &str,
        _: &Utf16JsonProjection,
        _: &Utf16JsonProjection,
        _: Option<&Utf16JsonProjection>,
    ) {
    }
    async fn emit_end_turn(&self, _: &str, _: &CostSnapshot) {}
    async fn emit_session_storage_state(&self, paused: bool) {
        if !paused {
            self.recovered.fetch_add(1, Ordering::SeqCst);
            if !self.reenter_recovered.load(Ordering::Acquire) {
                return;
            }
        } else {
            self.paused.fetch_add(1, Ordering::SeqCst);
        }
        let owner = self
            .owner
            .lock()
            .unwrap()
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
            .unwrap();
        // This must acquire both goal_transition and storage_append_lock again.
        // It also appends another retained goal row while storage remains full.
        owner
            .set_active_goal_last_reason(Some(if paused {
                "storage callback reentered".into()
            } else {
                "recovery callback reentered".into()
            }))
            .await;
    }
    async fn emit_system_notice(&self, text: &Utf16JsonProjection, error: bool) {
        if error {
            assert!(text.string_at("").unwrap().contains("ENOSPC"));
            self.notices.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[tokio::test]
async fn goal_enospc_notification_reenters_after_transition_and_retains_recoverable_rows() {
    let root = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
    let fs = Arc::new(AppendFailureFs {
        inner: platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
        fail: AtomicBool::new(false),
    });
    let writer = Arc::new(session::jsonl::JsonlWriter::new(path.clone(), fs.clone()));
    let output = Arc::new(ReentrantStorageOutput::default());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_session_id(id)
        .with_jsonl_writer(writer),
    );
    *output.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
    orch.initialize_conversation_mode().await.unwrap();
    fs.fail.store(true, Ordering::Release);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        orch.set_active_goal(&"finish work".into()),
    )
    .await
    .expect("ENOSPC callbacks must reenter goal controls without a lock cycle");
    assert_eq!(output.paused.load(Ordering::SeqCst), 1);
    assert_eq!(output.notices.load(Ordering::SeqCst), 1);
    assert_eq!(
        orch.session
            .lock()
            .await
            .active_goal
            .as_ref()
            .unwrap()
            .last_reason
            .as_ref()
            .unwrap()
            .string_at(""),
        Some("storage callback reentered")
    );
    let pending = orch.transcript.storage_pauses.lock().await[&id.as_uuid().to_string()]
        .rows
        .clone();
    assert_eq!(
        pending.len(),
        2,
        "both accepted goal snapshots remain recoverable"
    );
    assert_ne!(pending[0].uuid, pending[1].uuid);
    assert_eq!(
        *orch.transcript.last_jsonl_uuid.lock().await,
        Some(pending[1].uuid.clone())
    );
    fs.fail.store(false, Ordering::Release);
    bind_recovery_transcript(&orch, root.path(), id);
    orch.recover_session_storage().await.unwrap();
    let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
    for row in pending {
        assert!(loaded.contains_message(&row.uuid));
    }
}

#[tokio::test]
async fn nested_storage_notification_scope_reuses_outer_owner_and_drops_stale_session_notice() {
    let root = tempfile::tempdir().unwrap();
    let output = Arc::new(ReentrantStorageOutput::default());
    let orch = ConversationOrchestrator::into_shared(ConversationOrchestrator::new(
        crate::OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        root.path().to_path_buf(),
    ));
    *output.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
    let session = orch.session.lock().await.session_id.as_uuid().to_string();
    let ((), notices) = orch
        .with_deferred_storage_notifications(async {
            let ((), nested) = orch
                .with_deferred_storage_notifications(orch.mark_storage_paused(session.clone()))
                .await;
            assert!(nested.is_empty());
            assert_eq!(output.paused.load(Ordering::SeqCst), 0);
            assert!(orch
                .transcript
                .storage_pauses
                .lock()
                .await
                .contains_key(&session));
        })
        .await;
    assert_eq!(notices.len(), 1);
    orch.session.lock().await.session_id = SessionId::new();
    orch.emit_deferred_storage_notifications(notices).await;
    assert_eq!(output.paused.load(Ordering::SeqCst), 0);
    assert_eq!(output.notices.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn recovered_session_drops_held_enospc_notification_before_first_callback() {
    let root = tempfile::tempdir().unwrap();
    let id = SessionId::new();
    let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
    let fs = Arc::new(AppendFailureFs {
        inner: platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
        fail: AtomicBool::new(false),
    });
    let writer = Arc::new(session::jsonl::JsonlWriter::new(path, fs.clone()));
    let output = Arc::new(ReentrantStorageOutput::default());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_session_id(id)
        .with_jsonl_writer(writer),
    );
    *output.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
    orch.initialize_conversation_mode().await.unwrap();
    fs.fail.store(true, Ordering::Release);
    let ((), notices) = orch
        .with_deferred_storage_notifications(orch.set_active_goal(&"finish work".into()))
        .await;
    assert_eq!(notices.len(), 1);
    assert_eq!(output.paused.load(Ordering::SeqCst), 0);
    fs.fail.store(false, Ordering::Release);
    bind_recovery_transcript(&orch, root.path(), id);
    orch.recover_session_storage().await.unwrap();
    assert_eq!(orch.session.lock().await.session_id, id);
    assert_eq!(output.recovered.load(Ordering::SeqCst), 1);
    assert!(!orch
        .transcript
        .storage_pauses
        .lock()
        .await
        .contains_key(&id.as_uuid().to_string()));
    orch.emit_deferred_storage_notifications(notices).await;
    assert_eq!(output.paused.load(Ordering::SeqCst), 0);
    assert_eq!(output.notices.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn human_batched_admission_recovers_retained_storage_before_accepting_input() {
    for queued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        let fs = Arc::new(AppendFailureFs {
            inner: platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
            fail: AtomicBool::new(false),
        });
        let writer = Arc::new(session::jsonl::JsonlWriter::new(path.clone(), fs.clone()));
        let output = Arc::new(ReentrantStorageOutput::default());
        let api = Arc::new(MockApiClient::new(vec![
            crate::test_support::mock_message_response(
                vec![llm_runtime::ContentBlock::Text {
                    text: "recovered answer".into(),
                    cache_control: None,
                    citations: None,
                }],
                Some("end_turn"),
            ),
        ]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_session_id(id)
            .with_jsonl_writer(writer),
        );
        *output.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
        orch.initialize_conversation_mode().await.unwrap();
        fs.fail.store(true, Ordering::Release);
        orch.set_active_goal(&"retain goal rows".into()).await;
        let retained = orch.transcript.storage_pauses.lock().await[&id.as_uuid().to_string()]
            .rows
            .clone();
        assert!(!retained.is_empty());
        fs.fail.store(false, Ordering::Release);
        bind_recovery_transcript(&orch, root.path(), id);

        let meta = orch
            .run_queued_prompt_batch_batched(
                vec![crate::QueuedPromptInput {
                    text: "background work must not recover storage".into(),
                    is_meta: true,
                    ..Default::default()
                }],
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        assert!(meta.is_err());
        assert!(orch.snapshot_history().await.is_empty());
        assert!(api.captured_msgs().await.is_empty());
        assert_eq!(output.recovered.load(Ordering::SeqCst), 0);

        let result = if queued {
            orch.run_queued_prompt_batch_batched(
                vec![crate::QueuedPromptInput {
                    text: "human retry after freeing storage".into(),
                    ..Default::default()
                }],
                tokio_util::sync::CancellationToken::new(),
            )
            .await
        } else {
            orch.run_turn_with_cancel(
                "human retry after freeing storage",
                tokio_util::sync::CancellationToken::new(),
            )
            .await
        };
        assert_eq!(result.unwrap(), crate::conversation::TurnOutcome::EndTurn);
        assert_eq!(output.recovered.load(Ordering::SeqCst), 1);
        assert!(orch.transcript.storage_pauses.lock().await.is_empty());
        let calls = api.captured_msgs().await;
        assert_eq!(calls.len(), 1);
        assert!(calls[0]
            .iter()
            .any(|message| { message.text_content() == "human retry after freeing storage" }));
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        for row in retained {
            assert_eq!(
                loaded
                    .messages_in_order
                    .iter()
                    .filter(|persisted| persisted.uuid == row.uuid)
                    .count(),
                1,
            );
            assert_eq!(
                loaded.message(&row.uuid).unwrap().parent_uuid,
                row.parent_uuid
            );
        }
    }
}

#[derive(Default)]
struct SwitchingUsageProvider {
    owner: std::sync::Mutex<Option<std::sync::Weak<ConversationOrchestrator>>>,
    calls: AtomicUsize,
}
#[async_trait]
impl lingxi_core::host::LoopUsageProvider for SwitchingUsageProvider {
    async fn usage_rows(&self) -> Vec<lingxi_core::host::LoopUsageRow> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            let owner = self
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .unwrap();
            owner.clear_session().await.unwrap();
        }
        Vec::new()
    }
}

#[tokio::test]
async fn goal_set_retries_cost_capture_after_usage_provider_switches_session() {
    let root = tempfile::tempdir().unwrap();
    let usage = Arc::new(SwitchingUsageProvider::default());
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new(
            crate::OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![])),
            Arc::new(tool_api::ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(crate::test_support::MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            root.path().to_path_buf(),
        )
        .with_loop_usage(usage.clone()),
    );
    *usage.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
    let before = orch.session.lock().await.session_id;
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        orch.set_active_goal(&"new owner goal".into()),
    )
    .await
    .expect("usage callbacks can switch sessions without owning the goal transition");
    let current = orch.session.lock().await;
    assert_ne!(current.session_id, before);
    assert_eq!(
        current
            .active_goal
            .as_ref()
            .unwrap()
            .condition
            .string_at(""),
        Some("new owner goal")
    );
    assert_eq!(
        usage.calls.load(Ordering::SeqCst),
        2,
        "the retired session's cost capture is discarded"
    );
}

#[tokio::test]
async fn ordinary_and_hook_appends_release_storage_locks_before_reentrant_pause_and_recovery() {
    for hook_attachment in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let path = root.path().join(format!("{}.jsonl", id.as_uuid()));
        let fs = Arc::new(AppendFailureFs {
            inner: platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
            fail: AtomicBool::new(false),
        });
        let writer = Arc::new(session::jsonl::JsonlWriter::new(path.clone(), fs.clone()));
        let output = Arc::new(ReentrantStorageOutput::default());
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![])),
                Arc::new(tool_api::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                output.clone(),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_session_id(id)
            .with_jsonl_writer(writer),
        );
        *output.owner.lock().unwrap() = Some(Arc::downgrade(&orch));
        orch.initialize_conversation_mode().await.unwrap();
        orch.set_active_goal(&"finish work".into()).await;
        fs.fail.store(true, Ordering::Release);
        let row_id = MessageId::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            if hook_attachment {
                orch.persist_hook_attachment_to_jsonl_with_id(
                    row_id,
                    Utf16JsonProjection::parse(
                        r#"{"type":"hook_additional_context","content":"retained hook"}"#,
                    )
                    .unwrap(),
                )
                .await;
            } else {
                orch.persist_message_to_jsonl(&ConversationMessage::user(
                    row_id,
                    "retained ordinary user".into(),
                ))
                .await;
            }
        })
        .await
        .expect("ordinary ENOSPC callback reenters the append path after all append locks drop");
        let pending = orch.transcript.storage_pauses.lock().await[&id.as_uuid().to_string()]
            .rows
            .clone();
        assert_eq!(
            pending.len(),
            2,
            "original append and reentrant goal snapshot are retained"
        );
        assert_eq!(pending[0].uuid, row_id.as_uuid().to_string());
        assert_eq!(pending[1].parent_uuid.as_ref(), Some(&pending[0].uuid));
        assert_eq!(output.paused.load(Ordering::SeqCst), 1);
        fs.fail.store(false, Ordering::Release);
        bind_recovery_transcript(&orch, root.path(), id);
        output.reenter_recovered.store(true, Ordering::Release);
        let (recovery, notices) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            orch.with_deferred_storage_notifications(async {
                let _goal = orch.lifecycle_runtime.goal_transition.lock().await;
                let result = orch.recover_session_storage().await;
                assert_eq!(
                    output.recovered.load(Ordering::SeqCst),
                    0,
                    "recovery reuses the outer state transaction's collector"
                );
                result
            }),
        )
        .await
        .expect("recovery releases all append locks before callbacks");
        recovery.unwrap();
        orch.emit_deferred_storage_notifications(notices).await;
        assert_eq!(output.recovered.load(Ordering::SeqCst), 1);
        assert!(orch.check_storage_pause().await.is_ok());
        assert_eq!(
            orch.session
                .lock()
                .await
                .active_goal
                .as_ref()
                .unwrap()
                .last_reason
                .as_ref()
                .unwrap()
                .string_at(""),
            Some("recovery callback reentered")
        );
        let loaded = session::jsonl::reader::route_lines(&std::fs::read_to_string(path).unwrap());
        for row in &pending {
            assert!(loaded.contains_message(&row.uuid));
        }
        let tip = orch
            .transcript
            .last_jsonl_uuid
            .lock()
            .await
            .clone()
            .unwrap();
        assert_ne!(tip, pending[1].uuid);
        assert_eq!(
            loaded.message(&tip).unwrap().parent_uuid.as_ref(),
            Some(&pending[1].uuid)
        );
    }
}
