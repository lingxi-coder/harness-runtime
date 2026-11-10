use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use async_trait::async_trait;
use lingxi_core::host::filesystem::{FileContent, FileEvent, FlockGuard};
use lingxi_core::host::{FileSystem, FsError};
use lingxi_core::types::SessionId;
use lingxi_llm_client::realtime::*;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::*;

struct StorageFs {
    inner: platform_posix::fs::PosixFileSystem,
    fail: AtomicBool,
}

#[async_trait]
impl FileSystem for StorageFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file(path, offset, limit).await
    }
    async fn read_file_rooted_no_follow(
        &self,
        root: &std::path::Path,
        relative: &std::path::Path,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file_rooted_no_follow(root, relative).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.inner.write_file(path, content).await
    }
    fn is_within_workspace(&self, path: &str) -> bool {
        self.inner.is_within_workspace(path)
    }
    async fn watch(
        &self,
        path: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        self.inner.watch(path).await
    }
    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        if self.fail.load(Ordering::Acquire) {
            return Err(FsError::Io(
                "ENOSPC: injected realtime append failure".into(),
            ));
        }
        self.inner.append_file(path, content).await
    }
    async fn truncate(&self, path: &str, length: u64) -> Result<(), FsError> {
        self.inner.truncate(path, length).await
    }
    async fn file_mtime(&self, path: &str) -> Result<std::time::SystemTime, FsError> {
        self.inner.file_mtime(path).await
    }
    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        self.inner.file_size(path).await
    }
    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.inner.delete_file(path).await
    }
    async fn symlink(&self, target: &str, path: &str) -> Result<(), FsError> {
        self.inner.symlink(target, path).await
    }
    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        self.inner.flock_exclusive(path).await
    }
    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        self.inner.fsync(path).await
    }
}

#[derive(Default)]
struct ToolState {
    calls: AtomicUsize,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct BlockTool(Arc<ToolState>);

#[async_trait]
impl Tool for BlockTool {
    fn name(&self) -> &str {
        "storage_fixture"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            Utf16JsonProjection::from_serde_value(&serde_json::json!({"type":"object"})).unwrap()
        })
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        ToolInputRepairMetadata::Known {
            schema: ToolInputRepairSchema::Advertised,
            schema_provenance: ToolInputSchemaProvenance::GeneratedBuiltin,
            exempt_top_level_keys: None,
        }
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn is_read_only(&self, _: &Utf16JsonProjection) -> bool {
        false
    }
    fn interrupt_behavior(&self, _: &Utf16JsonProjection) -> InterruptBehavior {
        InterruptBehavior::Block
    }
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "realtime storage fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "realtime storage fixture".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "realtime storage fixture".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        self.0.started.notify_one();
        self.0.release.notified().await;
        Ok(ToolCallResult {
            data: Utf16JsonProjection::from_serde_value(&serde_json::json!({
                "content":"accepted mutation completed"
            }))
            .unwrap(),
            model_content: None,
            new_messages: Vec::new(),
            context_modifier: None,
            mcp_meta: None,
            is_error: false,
            subagent_hooks_fired: false,
        })
    }
}

struct Fixture {
    root: tempfile::TempDir,
    orch: Arc<ConversationOrchestrator>,
    fs: Arc<StorageFs>,
    tool: Arc<ToolState>,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let id = SessionId::new();
        let fs = Arc::new(StorageFs {
            inner: platform_posix::fs::PosixFileSystem::new(root.path().to_path_buf()),
            fail: AtomicBool::new(false),
        });
        let writer = Arc::new(session::jsonl::JsonlWriter::new(
            root.path().join(format!("{}.jsonl", id.as_uuid())),
            fs.clone(),
        ));
        let tool = Arc::new(ToolState::default());
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(BlockTool(tool.clone())));
        let orch = ConversationOrchestrator::into_shared(
            ConversationOrchestrator::new(
                crate::OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(registry),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                root.path().to_path_buf(),
            )
            .with_session_id(id)
            .with_jsonl_writer(writer),
        );
        orch.initialize_conversation_mode().await.unwrap();
        orch.session.lock().await.model_profile = Some("realtime-storage-profile".into());
        Self {
            root,
            orch,
            fs,
            tool,
        }
    }

    async fn prepared(&self) -> RealtimeAgentContext {
        let mut prepared = self.orch.prepare_realtime_agent().await.unwrap();
        prepared.account_scope = Some("explicit-service.realtime-storage".into());
        prepared
    }

    async fn recover(&self) {
        self.fs.fail.store(false, Ordering::Release);
        let writer = self.orch.transcript.jsonl_writer.as_ref().unwrap();
        let path = writer.active_path();
        let state = self.root.path().join("recovery-state");
        std::fs::create_dir(&state).unwrap();
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(state).unwrap());
        let (authority, _) = session::jsonl::writer::TranscriptFileAuthority::capture(
            self.root.path().to_path_buf(),
            path.strip_prefix(self.root.path()).unwrap().to_path_buf(),
        )
        .unwrap();
        let id = self.orch.session.lock().await.session_id;
        writer
            .activate_session_target_with_durable_lock_and_authority(
                id,
                path,
                self.root.path().to_path_buf(),
                durable,
                Some(authority),
            )
            .unwrap();
        self.orch.recover_session_storage().await.unwrap();
    }
}

struct Codec;

impl RealtimeCodec for Codec {
    fn capabilities(&self) -> RealtimeCapabilities {
        RealtimeCapabilities {
            history_import: true,
            tools: true,
            input_transcription: true,
            output_transcription: true,
            interruption: true,
            ..Default::default()
        }
    }
    fn encode(&self, input: &RealtimeInput) -> Result<Vec<RealtimeFrame>, RealtimeError> {
        Ok(vec![RealtimeFrame::text(format!("{input:?}"))])
    }
    fn decode(&self, frame: RealtimeFrame) -> Result<Vec<RealtimeEvent>, RealtimeError> {
        let RealtimeFrame::Text(bytes) = frame else {
            panic!("text fixture")
        };
        let event = match bytes.as_ref() {
            b"committed" => RealtimeEvent::ProviderEvent {
                name: "input_audio_buffer.committed".into(),
                native: serde_json::json!({"item_id":"input-1"}),
            },
            b"tool" | b"tool-second" => RealtimeEvent::ToolCall {
                call_id: String::from_utf8(bytes.to_vec()).unwrap(),
                name: "storage_fixture".into(),
                arguments: serde_json::json!({}),
            },
            b"input" => RealtimeEvent::Transcript {
                direction: RealtimeTranscriptDirection::Input,
                update: RealtimeTranscriptUpdate::Replace,
                text: "accepted spoken input".into(),
                item_id: Some("input-1".into()),
                turn_id: Some("response-1".into()),
                final_chunk: true,
            },
            _ => RealtimeEvent::SessionReady,
        };
        Ok(vec![event])
    }
}

#[derive(Default)]
struct Wire {
    sent: std::sync::Mutex<Vec<String>>,
    closed: tokio::sync::Notify,
}

struct Sink(Arc<Wire>);

#[async_trait]
impl RealtimeSink for Sink {
    async fn send(&mut self, frame: RealtimeFrame) -> Result<(), RealtimeError> {
        if let RealtimeFrame::Text(bytes) = frame {
            self.0
                .sent
                .lock()
                .unwrap()
                .push(String::from_utf8(bytes.to_vec()).unwrap());
        }
        Ok(())
    }
    async fn ping(&mut self, _: bytes::Bytes) -> Result<(), RealtimeError> {
        Ok(())
    }
    fn abort(&mut self) {
        self.0.closed.notify_one();
    }
    async fn close(&mut self, _: RealtimeClose) -> Result<(), RealtimeError> {
        self.0.closed.notify_one();
        Ok(())
    }
}

struct Transport {
    inbound: std::sync::Mutex<
        Option<futures::channel::mpsc::UnboundedReceiver<Result<RealtimeFrame, RealtimeError>>>,
    >,
    wire: Arc<Wire>,
}

#[async_trait]
impl RealtimeTransport for Transport {
    async fn connect(
        &self,
        _: RealtimeConnectRequest,
    ) -> Result<RealtimeConnection, RealtimeError> {
        Ok(RealtimeConnection {
            outbound: Box::new(Sink(self.wire.clone())),
            inbound: self.inbound.lock().unwrap().take().unwrap().boxed(),
        })
    }
}

async fn connection() -> (
    RealtimeControl,
    RealtimeEvents,
    tokio::task::JoinHandle<Result<(), RealtimeError>>,
    futures::channel::mpsc::UnboundedSender<Result<RealtimeFrame, RealtimeError>>,
    Arc<Wire>,
) {
    let (sender, receiver) = futures::channel::mpsc::unbounded();
    let wire = Arc::new(Wire::default());
    let (session, driver) = RealtimeSession::connect(
        &Transport {
            inbound: std::sync::Mutex::new(Some(receiver)),
            wire: wire.clone(),
        },
        RealtimeConnectRequest {
            endpoint: "wss://fixture".into(),
            headers: Vec::new(),
            max_frame_bytes: 1024,
        },
        Arc::new(Codec),
        RealtimeLimits::default(),
    )
    .await
    .unwrap();
    let (control, events) = session.into_parts();
    (control, events, tokio::spawn(driver.run()), sender, wire)
}

#[tokio::test]
async fn realtime_prepare_keeps_paused_storage_for_human_retry() {
    let fixture = Fixture::new().await;
    fixture.orch.pause_for_disk_full().await;
    let _profile = fixture.orch.conversation_context.lock().await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.orch.prepare_realtime_agent(),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("ENOSPC"));
    assert!(fixture.orch.check_storage_pause().await.is_err());
    assert!(fixture.orch.session.lock().await.history.is_empty());
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn realtime_run_rejects_storage_paused_after_preparation_and_closes_io() {
    let fixture = Fixture::new().await;
    let prepared = fixture.prepared().await;
    let (control, events, driver, _provider, wire) = connection().await;
    let (sender, receiver) = mpsc::channel(8);
    sender
        .try_send(RealtimeAgentInput::Provider(RealtimeInput::Text(
            "unadmitted input".into(),
        )))
        .unwrap();
    let (output, _output_rx) = mpsc::channel(8);
    fixture.orch.pause_for_disk_full().await;
    let _profile = fixture.orch.conversation_context.lock().await;
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.orch.run_realtime_agent(
            prepared,
            control,
            events,
            receiver,
            output,
            RealtimeAgentLimits::default(),
            false,
            CancellationToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.to_string().contains("ENOSPC"));
    tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(wire.sent.lock().unwrap().is_empty());
    assert!(fixture.orch.check_storage_pause().await.is_err());
    assert!(fixture.orch.session.lock().await.history.is_empty());
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn realtime_enospc_stops_new_work_and_drains_accepted_block_tool() {
    let fixture = Fixture::new().await;
    let prepared = fixture.prepared().await;
    let (control, events, driver, provider, wire) = connection().await;
    let (sender, receiver) = mpsc::channel(8);
    sender
        .send(RealtimeAgentInput::Provider(RealtimeInput::CommitAudio))
        .await
        .unwrap();
    let (output, mut output_rx) = mpsc::channel(16);
    let task = tokio::spawn({
        let orch = fixture.orch.clone();
        async move {
            orch.run_realtime_agent(
                prepared,
                control,
                events,
                receiver,
                output,
                RealtimeAgentLimits::default(),
                false,
                CancellationToken::new(),
            )
            .await
        }
    });
    let started = tokio::time::timeout(Duration::from_secs(2), output_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        started,
        RealtimeAgentOutput::AgentResponseStarted { .. }
    ));
    provider
        .unbounded_send(Ok(RealtimeFrame::text("committed")))
        .unwrap();
    provider
        .unbounded_send(Ok(RealtimeFrame::text("tool")))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), fixture.tool.started.notified())
        .await
        .unwrap();
    fixture.fs.fail.store(true, Ordering::Release);
    provider
        .unbounded_send(Ok(RealtimeFrame::text("input")))
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), wire.closed.notified())
        .await
        .unwrap();
    assert!(fixture.orch.check_storage_pause().await.is_err());
    assert!(
        !task.is_finished(),
        "the accepted Block tool must retain its owner until settlement"
    );
    let _ = sender.try_send(RealtimeAgentInput::Provider(RealtimeInput::Text(
        "new input after pause".into(),
    )));
    let _ = provider.unbounded_send(Ok(RealtimeFrame::text("tool-second")));
    fixture.tool.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("ENOSPC"));
    tokio::time::timeout(Duration::from_secs(2), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(fixture.tool.calls.load(Ordering::SeqCst), 1);
    let history = fixture.orch.session.lock().await.history.clone();
    assert_eq!(
        history.len(),
        3,
        "one accepted tool use, input transcript, and tool result"
    );
    assert_eq!(history[1].text_content(), "accepted spoken input");
    assert!(
        matches!(&history[2], ConversationMessage::User { content, .. }
        if content.iter().any(|block| matches!(block, ContentBlock::ToolResult { is_error, .. }
            if *is_error != Some(true))))
    );
    let sent = wire.sent.lock().unwrap().clone();
    assert_eq!(
        sent.len(),
        1,
        "only the admitted audio commit may reach the provider"
    );
    assert!(sent[0].starts_with("CommitAudio"));
    fixture.recover().await;
    let path = fixture
        .orch
        .transcript
        .jsonl_writer
        .as_ref()
        .unwrap()
        .active_path();
    let rows = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    for message in &history {
        let id = message.id().as_uuid().to_string();
        assert_eq!(
            rows.iter()
                .filter(|row| row["uuid"].as_str() == Some(id.as_str()))
                .count(),
            1,
            "accepted realtime rows must survive recovery exactly once"
        );
    }
    assert!(fixture.orch.check_storage_pause().await.is_ok());
}
