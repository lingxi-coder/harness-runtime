use super::*;
use crate::conversation::{CostSessionSwitcher, PreparedSessionSwitch};
use crate::test_support::{
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use lingxi_core::host::{CostSnapshot, CurrentUsageSnapshot, OutputStream, PromptSnapshot};
use lingxi_core::types::{ConversationMessage, HookId, MessageId, SessionId, ToolUseId};
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::time::Duration;

struct TargetPreparer {
    destination_lock: Arc<session::jsonl::DurableTranscriptWriter>,
}

#[async_trait]
impl CostSessionSwitcher for TargetPreparer {
    async fn prepare_session(
        &self,
        tracker: Arc<cost::CostTracker>,
        session: SessionId,
    ) -> Result<PreparedSessionSwitch, cost::CostPersistError> {
        let prepared = tracker.prepare_session(session).await?;
        Ok(PreparedSessionSwitch::new(
            prepared,
            Some(self.destination_lock.clone()),
        ))
    }
}

struct ReadyHook;

#[async_trait]
impl hooks::HookPromptRunner for ReadyHook {
    async fn run(
        &self,
        _: hooks::PromptHookRequest,
    ) -> Result<Utf16JsonProjection, hooks::PromptHookError> {
        Ok(r#"{"ok":true}"#.into())
    }
}

struct Fixture {
    _home: tempfile::TempDir,
    _cost_records: tokio::sync::mpsc::Receiver<cost::CostState>,
    home: PathBuf,
    cwd: PathBuf,
    orch: Arc<ConversationOrchestrator>,
    tracker: Arc<cost::CostTracker>,
    api: Arc<MockApiClient>,
    writer: Arc<session::JsonlWriter>,
    a: SessionId,
    b: SessionId,
    path_a: PathBuf,
    path_b: PathBuf,
    source_b: String,
    pinned_b: lingxi_core::host::PinnedResumeSource,
    history_a: Vec<ConversationMessage>,
    history_b: Vec<ConversationMessage>,
    snapshot_a: PromptSnapshot,
    read_path: PathBuf,
    read_entry: tool_api::read_file_state::ReadFileEntry,
    original_a: String,
}

impl Fixture {
    async fn new(observer: Option<Arc<dyn OutputStream>>, reuse_old_authority: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(temp.path()).unwrap();
        let cwd = home.join("workspace");
        std::fs::create_dir_all(&cwd).unwrap();
        let a = SessionId::new();
        let b = SessionId::new();
        let (cost_records, cost_rx) = tokio::sync::mpsc::channel(32);
        let tracker = Arc::new(cost::CostTracker::new(
            a,
            Arc::new(cost::PricingCatalog::empty()),
            cost_records,
        ));
        let old_state = home.join("authority-a");
        let new_state = home.join("authority-b");
        std::fs::create_dir(&old_state).unwrap();
        std::fs::create_dir(&new_state).unwrap();
        let old_lock = Arc::new(session::jsonl::DurableTranscriptWriter::open(old_state).unwrap());
        let new_lock = Arc::new(session::jsonl::DurableTranscriptWriter::open(new_state).unwrap());
        let path_a = session::jsonl::path::session_path(
            &home,
            &cwd.to_string_lossy(),
            &a.as_uuid().to_string(),
        );
        let path_b = session::jsonl::path::session_path(
            &home,
            &cwd.to_string_lossy(),
            &b.as_uuid().to_string(),
        );
        let writer = Arc::new(
            session::JsonlWriter::new(
                path_a.clone(),
                Arc::new(platform_posix::fs::PosixFileSystem::new(home.clone())),
            )
            .with_durable_lock(old_lock.clone()),
        );
        writer
            .activate_session_target(a, path_a.clone(), cwd.clone())
            .unwrap();
        let mut executor = Arc::try_unwrap(noop_hook_executor())
            .ok()
            .unwrap()
            .with_prompt_runner(Arc::new(ReadyHook));
        if let Some(observer) = observer {
            executor = executor.with_hook_observer(observer);
        }
        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "next A answer".into(),
                cache_control: None,
                citations: None,
            }],
            Some("end_turn"),
        )]));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                api.clone(),
                Arc::new(tool_api::ToolRegistry::new()),
                Arc::new(executor),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                cwd.clone(),
            )
            .with_session_id(a)
            .with_jsonl_writer(writer.clone())
            .with_config_home(home.clone())
            .with_cost_tracker(tracker.clone())
            .with_cost_session_switcher(Arc::new(TargetPreparer {
                // Reusing the already-bound A authority for a fresh clear
                // target forces activation itself to reject the target.
                destination_lock: if reuse_old_authority {
                    old_lock.clone()
                } else {
                    new_lock
                },
            }))
            .without_host_environment_probes(),
        );
        orch.attach_owned_session_switches();
        orch.initialize_conversation_mode().await.unwrap();
        let history_a = vec![ConversationMessage::user(
            MessageId::new(),
            "A retained history".into(),
        )];
        orch.session.lock().await.history = history_a.clone();
        orch.persist_message_to_jsonl(&history_a[0]).await;
        let (authority_a, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
            home.clone(),
            path_a.strip_prefix(&home).unwrap().to_path_buf(),
        )
        .unwrap();
        writer
            .activate_session_target_with_durable_lock_and_authority(
                a,
                path_a.clone(),
                cwd.clone(),
                old_lock,
                Some(authority_a),
            )
            .unwrap();
        let prompt_policy = orch.current_prompt_policy().await.unwrap().metadata();
        let snapshot_a = PromptSnapshot {
            system_prompt: vec!["A frozen prompt marker".into()],
            prompt_policy: Some(prompt_policy),
            ..Default::default()
        };
        *orch.prompt_runtime.prompt_snapshot.lock().await = Some(snapshot_a.clone());
        *orch.prompt_runtime.prompt_snapshot_epoch.lock().await = Some(0);
        orch.prompt_runtime
            .prompt_snapshot_resume
            .store(true, Ordering::Release);
        orch.restore_response_usage(Some(CurrentUsageSnapshot {
            input_tokens: 31,
            output_tokens: 7,
            cache_read_input_tokens: 11,
            cache_creation_input_tokens: 3,
        }));
        {
            let mut session = orch.session.lock().await;
            session.usage.0.input_tokens = 31;
            session.usage.0.output_tokens = 7;
        }
        let read_path = cwd.join("retained.txt");
        std::fs::write(&read_path, "A retained read state").unwrap();
        let read_entry = tool_api::read_file_state::ReadFileEntry {
            content: "A retained read state".into(),
            mtime_ms: 123,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        };
        tool_api::read_file_state::set(
            &orch.prompt_runtime.read_state_map,
            read_path.clone(),
            read_entry.clone(),
        );
        let prompt = orch.provider_system_prompt().await.unwrap();
        assert!(ConversationOrchestrator::system_prompt_source_text(&prompt)
            .string_at("")
            .unwrap()
            .contains("A frozen prompt marker"));
        let history_b = vec![ConversationMessage::user(
            MessageId::new(),
            "B target history".into(),
        )];
        let mode_b = crate::test_support::code_resume_runtime_snapshot(b)
            .transcript_metadata
            .unwrap()
            .remove(0);
        let source_b = format!(
            "{}\n{}\n",
            mode_b.to_json_string().unwrap(),
            serde_json::json!({
                "type":"user", "uuid":history_b[0].id().as_uuid(), "parentUuid":null,
                "sessionId":b.as_uuid(), "cwd":cwd,
                "message":{"role":"user","content":"B target history"},
            })
        );
        std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();
        std::fs::write(&path_b, &source_b).unwrap();
        let (authority_b, captured) = session::jsonl::writer::TranscriptFileAuthority::capture(
            home.clone(),
            path_b.strip_prefix(&home).unwrap().to_path_buf(),
        )
        .unwrap();
        assert_eq!(captured, source_b.as_bytes());
        let pinned_b = authority_b.pinned_resume_source(source_b.clone());
        let original_a = std::fs::read_to_string(&path_a).unwrap();
        Self {
            _home: temp,
            _cost_records: cost_rx,
            home,
            cwd,
            orch,
            tracker,
            api,
            writer,
            a,
            b,
            path_a,
            path_b,
            source_b,
            pinned_b,
            history_a,
            history_b,
            snapshot_a,
            read_path,
            read_entry,
            original_a,
        }
    }

    fn runtime_b(&self) -> lingxi_core::host::ResumeRuntimeSnapshot {
        lingxi_core::host::ResumeRuntimeSnapshot {
            transcript_path: Some(self.path_b.clone()),
            transcript_source: Some(self.pinned_b.clone()),
            working_directory: Some(self.cwd.clone()),
            current_usage: Some(CurrentUsageSnapshot {
                input_tokens: 900,
                output_tokens: 90,
                ..Default::default()
            }),
            prompt_snapshot: Some(PromptSnapshot {
                system_prompt: vec!["B frozen prompt must not leak".into()],
                prompt_policy: self.snapshot_a.prompt_policy.clone(),
                ..Default::default()
            }),
            prompt_snapshot_epoch: Some(0),
            ..crate::test_support::code_resume_runtime_snapshot(self.b)
        }
    }

    async fn assert_a_unchanged(&self) {
        let session = self.orch.session.lock().await;
        assert_eq!(session.session_id, self.a);
        assert_eq!(
            serde_json::to_value(&session.history).unwrap(),
            serde_json::to_value(&self.history_a).unwrap()
        );
        assert_eq!(session.usage.0.input_tokens, 31);
        assert_eq!(session.usage.0.output_tokens, 7);
        drop(session);
        assert_eq!(self.tracker.session_id().await, self.a);
        assert_eq!(self.writer.active_path(), self.path_a);
        assert_eq!(
            self.writer.session_target_path(self.a),
            Some(self.path_a.clone())
        );
        assert!(self.writer.session_target_path(self.b).is_none());
        assert_eq!(
            std::fs::read_to_string(&self.path_a).unwrap(),
            self.original_a
        );
        assert_eq!(
            *self.orch.prompt_runtime.prompt_snapshot.lock().await,
            Some(self.snapshot_a.clone())
        );
        assert_eq!(
            *self.orch.prompt_runtime.prompt_snapshot_epoch.lock().await,
            Some(0)
        );
        assert!(self
            .orch
            .prompt_runtime
            .prompt_snapshot_resume
            .load(Ordering::Acquire));
        assert_eq!(
            self.orch
                .compaction_runtime
                .last_response_input_tokens
                .load(Ordering::Relaxed),
            45
        );
        assert_eq!(
            self.orch
                .compaction_runtime
                .last_response_output_tokens
                .load(Ordering::Relaxed),
            7
        );
        assert_eq!(
            self.orch
                .compaction_runtime
                .total_tokens_reminder_usage
                .load(Ordering::Relaxed),
            52
        );
        assert_eq!(
            tool_api::read_file_state::get(
                &self.orch.prompt_runtime.read_state_map,
                &self.read_path
            ),
            Some(self.read_entry.clone())
        );
        assert_eq!(
            *self.orch.transcript.last_jsonl_uuid.lock().await,
            Some(self.history_a[0].id().as_uuid().to_string())
        );
    }
}

async fn wait_for_suspension(gate: &crate::autonomous_tool_scheduler::TransientPublicationGate) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while !gate.is_suspended() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("switch reaches activation after importing B identity");
}

#[tokio::test]
async fn failed_pinned_hot_resume_preserves_a_state_and_reopens_publication() {
    let fixture = Fixture::new(None, false).await;
    let native = fixture
        .orch
        .expansion_hook_context()
        .await
        .publication_guard
        .unwrap();
    let (old_root, publication_lock) = fixture
        .orch
        .lifecycle_runtime
        .session_tool_hook_generation
        .current();
    let mod_lease = fixture
        .orch
        .lifecycle_runtime
        .mod_session_jobs
        .capture()
        .unwrap();
    let old_stop = fixture.orch.hooks.subagent_stop_firer(fixture.a).unwrap();
    let gate = fixture
        .orch
        .lifecycle_runtime
        .session_tool_hook_generation
        .transient_publications();
    let old_authority = fixture.writer.active_transcript_authority().unwrap();
    let held = publication_lock.lock_owned().await;
    let runtime = fixture.runtime_b();
    let history = fixture.history_b.clone();
    let tail = history[0].id().as_uuid().to_string();
    let b = fixture.b;
    let resuming = tokio::spawn({
        let orch = fixture.orch.clone();
        async move {
            orch.resume_session(b, history, Some(tail), None, runtime)
                .await
        }
    });
    wait_for_suspension(&gate).await;
    assert!(!old_root.is_cancelled());
    assert!(!mod_lease.is_cancelled());
    let replacement = fixture.home.join("replacement.jsonl");
    std::fs::write(&replacement, &fixture.source_b).unwrap();
    std::fs::rename(replacement, &fixture.path_b).unwrap();
    drop(held);
    let error = tokio::time::timeout(Duration::from_secs(2), resuming)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        error.to_string().contains("Transcript activation failed"),
        "{error}"
    );
    fixture.assert_a_unchanged().await;
    assert!(old_authority.same_source(&fixture.writer.active_transcript_authority().unwrap()));
    assert!(!gate.is_suspended());
    assert!(!old_root.is_cancelled());
    assert!(!mod_lease.is_cancelled());
    assert!(native.is_current());
    assert!(old_stop.is_current());
    assert!(Arc::ptr_eq(
        &old_stop,
        &fixture.orch.hooks.subagent_stop_firer(fixture.a).unwrap()
    ));
    let published = AtomicBool::new(false);
    assert!(
        native
            .publish_if_current(Box::pin(async {
                published.store(true, Ordering::SeqCst);
            }))
            .await
    );
    assert!(published.load(Ordering::SeqCst));
    fixture
        .orch
        .run_turn_with_cancel(
            "next request stays in A",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let systems = fixture.api.captured_systems().await;
    assert_eq!(systems.len(), 1);
    let system = systems[0].as_deref().unwrap();
    assert!(system.contains("A frozen prompt marker"), "{system}");
    assert!(!system.contains("B frozen prompt must not leak"));
    assert_eq!(fixture.orch.current_session_id().await, fixture.a);
    assert_eq!(fixture.writer.active_path(), fixture.path_a);
    assert!(old_stop.is_current());
    assert!(fixture
        .orch
        .close_and_drain_session_switches()
        .await
        .is_empty());
}

struct BackpressuredHookOutput {
    queue: tokio::sync::mpsc::Sender<()>,
    entered: tokio::sync::Notify,
    completed: AtomicUsize,
}

#[async_trait]
impl OutputStream for BackpressuredHookOutput {
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
    async fn emit_hook_started(&self, _: &str, _: &str, _: &str) {
        self.entered.notify_one();
        self.queue.send(()).await.unwrap();
        self.completed.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn hot_resume_suspends_real_native_hook_ui_before_waiting_for_publication() {
    let (queue, mut receiver) = tokio::sync::mpsc::channel(1);
    queue.send(()).await.unwrap();
    let output = Arc::new(BackpressuredHookOutput {
        queue,
        entered: tokio::sync::Notify::new(),
        completed: AtomicUsize::new(0),
    });
    let fixture = Fixture::new(Some(output.clone()), false).await;
    fixture
        .orch
        .hooks
        .upsert_session_named_hook(
            fixture.a,
            "held native lifecycle".into(),
            hooks::HookDefinition {
                id: HookId::new(),
                name: "held native lifecycle".into(),
                events: vec![hooks::HookEventType::ConfigChange],
                if_condition: None,
                executor: hooks::HookExecutor::Prompt {
                    prompt: "check config $ARGUMENTS".into(),
                    model: None,
                    continue_on_block: false,
                },
                source: hooks::HookSource::Session,
                blocking: true,
                timeout: None,
                priority: 0,
                once: false,
                status_message: None,
                async_rewake: false,
                async_timeout: None,
                rewake_message: None,
                on_failure: None,
            },
        )
        .await;
    let (old_root, _) = fixture
        .orch
        .lifecycle_runtime
        .session_tool_hook_generation
        .current();
    let old_mod = fixture
        .orch
        .lifecycle_runtime
        .mod_session_jobs
        .capture()
        .unwrap();
    let old_stop = fixture.orch.hooks.subagent_stop_firer(fixture.a).unwrap();
    let hook = tokio::spawn({
        let orch = fixture.orch.clone();
        async move {
            orch.fire_config_change(hooks::events::ConfigChangeSource::UserSettings, None)
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), output.entered.notified())
        .await
        .unwrap();
    let runtime = fixture.runtime_b();
    let tail = fixture.history_b[0].id().as_uuid().to_string();
    tokio::time::timeout(
        Duration::from_secs(2),
        fixture.orch.resume_session(
            fixture.b,
            fixture.history_b.clone(),
            Some(tail),
            None,
            runtime,
        ),
    )
    .await
    .expect("a backpressured native UI publication must not deadlock activation")
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), hook)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.orch.current_session_id().await, fixture.b);
    assert_eq!(fixture.writer.active_path(), fixture.path_b);
    assert!(old_root.is_cancelled());
    assert!(old_mod.is_cancelled());
    assert!(!old_stop.is_current());
    assert_eq!(output.completed.load(Ordering::SeqCst), 0);
    assert!(
        receiver.try_recv().is_ok(),
        "the deliberately prefilled UI frame remains"
    );
    assert!(
        receiver.try_recv().is_err(),
        "the suspended hook frame was dropped"
    );
    let current = fixture
        .orch
        .expansion_hook_context()
        .await
        .publication_guard
        .unwrap();
    assert!(current.is_current());
    assert!(current.publish_if_current(Box::pin(async {})).await);
    assert!(fixture
        .orch
        .close_and_drain_session_switches()
        .await
        .is_empty());
}

#[tokio::test]
async fn failed_hot_clear_target_preserves_a_before_runtime_reset() {
    let fixture = Fixture::new(None, true).await;
    let native = fixture
        .orch
        .expansion_hook_context()
        .await
        .publication_guard
        .unwrap();
    let (old_root, _) = fixture
        .orch
        .lifecycle_runtime
        .session_tool_hook_generation
        .current();
    let old_mod = fixture
        .orch
        .lifecycle_runtime
        .mod_session_jobs
        .capture()
        .unwrap();
    let old_stop = fixture.orch.hooks.subagent_stop_firer(fixture.a).unwrap();
    let error = tokio::time::timeout(Duration::from_secs(2), fixture.orch.clear_session())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error.to_string().contains("Transcript activation failed"),
        "{error}"
    );
    fixture.assert_a_unchanged().await;
    assert!(!old_root.is_cancelled());
    assert!(!old_mod.is_cancelled());
    assert!(old_stop.is_current());
    assert!(native.is_current());
    assert!(native.publish_if_current(Box::pin(async {})).await);
    assert!(fixture
        .orch
        .close_and_drain_session_switches()
        .await
        .is_empty());
}
