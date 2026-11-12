use super::*;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, StaticMemoryProvider,
};
use lingxi_core::host::{PermissionDecision, PermissionGate};
use std::sync::atomic::{AtomicUsize, Ordering};
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, Tool, ToolStaticContext};

#[test]
fn deferred_tool_origin_requires_the_exact_captured_assistant_row() {
    let session = Uuid::new_v4().to_string();
    let logical = Uuid::new_v4().to_string();
    let physical = Uuid::new_v4().to_string();
    let row = |value: serde_json::Value| {
        JsonlMessage::from_projection(&Utf16JsonProjection::from_serde_value(&value).unwrap())
            .unwrap()
    };
    let assistant = row(
        serde_json::json!({"type":"assistant","uuid":physical,"parentUuid":null,"sessionId":session,
        "message":{"id":logical,"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"LocalApp","input":{}}]}}),
    );
    let attachment = row(
        serde_json::json!({"type":"attachment","uuid":Uuid::new_v4().to_string(),"parentUuid":physical,"sessionId":session,
        "attachment":{"type":"hook_deferred_tool","toolUseID":"call-1","toolName":"LocalApp","toolInput":{},"assistantMessageId":logical}}),
    );
    let loaded = deferred_tool_replays_from_messages(&[assistant.clone(), attachment.clone()]);
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded[0]
            .assistant_message_id
            .unwrap()
            .as_uuid()
            .to_string(),
        physical
    );
    let missing = deferred_tool_replays_from_messages(std::slice::from_ref(&attachment));
    assert_eq!(missing[0].assistant_message_id, None);
    let duplicated =
        deferred_tool_replays_from_messages(&[assistant.clone(), assistant, attachment]);
    assert_eq!(duplicated[0].assistant_message_id, None);
}

struct CountingGate(Arc<AtomicUsize>);

#[async_trait::async_trait]
impl PermissionGate for CountingGate {
    async fn check(&self, _: &str, _: &Utf16JsonProjection) -> PermissionDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        PermissionDecision::Allow
    }
}

struct CountingTool {
    calls: Arc<AtomicUsize>,
    replace_after_first: Option<PathBuf>,
    modifier_failure: bool,
    read_state: Option<tool_api::read_file_state::ReadFileStateMap>,
    read_path: Option<PathBuf>,
}

#[async_trait::async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "ReplayAuthorityTest"
    }

    fn input_schema(&self) -> &Utf16JsonProjection {
        static SCHEMA: std::sync::OnceLock<Utf16JsonProjection> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| Utf16JsonProjection::parse(r#"{"type":"object"}"#).unwrap())
    }

    fn input_repair_metadata(&self) -> tool_api::tool_trait::ToolInputRepairMetadata {
        tool_api::tool_trait::ToolInputRepairMetadata::Known {
            schema: tool_api::tool_trait::ToolInputRepairSchema::Advertised,
            schema_provenance: tool_api::tool_trait::ToolInputSchemaProvenance::GeneratedBuiltin,
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

    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &tool_api::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "replay authority test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: Default::default(),
        }
    }

    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "replay authority test".into()
    }

    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "".into()
    }

    async fn call(
        &self,
        input: Utf16JsonProjection,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        let previous = self.calls.fetch_add(1, Ordering::SeqCst);
        if previous == 0 {
            if let Some(path) = &self.replace_after_first {
                replace_leaf(path);
            }
        }
        if let (Some(map), Some(path)) = (&self.read_state, &self.read_path) {
            if input.at_pointer("/read").and_then(JsonValueView::as_bool) == Some(true) {
                tool_api::read_file_state::set(
                    map,
                    path.clone(),
                    tool_api::read_file_state::ReadFileEntry {
                        content: "known read bytes".into(),
                        mtime_ms: 17,
                        offset: None,
                        limit: None,
                        from_read: true,
                        seeded_from_context: false,
                        is_partial_view: false,
                    },
                );
            }
            if input
                .at_pointer("/check_cached_read")
                .and_then(JsonValueView::as_bool)
                == Some(true)
            {
                tool_file::check_read_before_write(map, path, 17, "known read bytes", false)
                    .map_err(|error| tool_api::ToolError::Internal(error.to_string()))?;
            }
        }
        let mut result = tool_api::ToolCallResult::from_data("called".into());
        if self.modifier_failure {
            result.context_modifier = Some(Box::new(|mut context| {
                context.options.main_loop_model = "unresolvable-modifier-model".into();
                context
            }));
        }
        Ok(result)
    }
}

fn replace_leaf(path: &Path) {
    let bytes = std::fs::read(path).unwrap();
    std::fs::rename(path, path.with_extension("saved")).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn deferred(count: usize) -> Vec<lingxi_core::host::DeferredToolReplay> {
    (0..count)
        .map(|i| lingxi_core::host::DeferredToolReplay {
            tool_use_id: format!("replay-{i}"),
            assistant_message_id: None,
            tool_name: "ReplayAuthorityTest".into(),
            tool_input: Utf16JsonProjection::parse("{}").unwrap(),
            permission_mode: None,
            traceparent: None,
        })
        .collect()
}

fn orchestrator(
    id: SessionId,
    cwd: &Path,
    writer: Option<Arc<JsonlWriter>>,
    replace_after_first: Option<PathBuf>,
) -> (ConversationOrchestrator, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    orchestrator_options(id, cwd, writer, replace_after_first, false, None)
}

fn orchestrator_options(
    id: SessionId,
    cwd: &Path,
    writer: Option<Arc<JsonlWriter>>,
    replace_after_first: Option<PathBuf>,
    modifier_failure: bool,
    read_path: Option<PathBuf>,
) -> (ConversationOrchestrator, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let permissions = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let read_state = tool_api::read_file_state::new_read_file_state_map();
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(CountingTool {
        calls: calls.clone(),
        replace_after_first,
        modifier_failure,
        read_state: read_path.as_ref().map(|_| read_state.clone()),
        read_path,
    }));
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(Vec::new())),
        Arc::new(tools),
        noop_hook_executor(),
        Arc::new(CountingGate(permissions.clone())),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_owned(),
    )
    .with_session_id(id)
    .with_read_state_map(read_state);
    if let Some(writer) = writer {
        orch = orch.with_jsonl_writer(writer);
    }
    (orch, permissions, calls)
}

struct Fixture {
    _root: tempfile::TempDir,
    source_root: PathBuf,
    path: PathBuf,
    id: SessionId,
    writer: Arc<JsonlWriter>,
}

impl Fixture {
    fn new(bind_authority: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let state_root = root.path().join("state");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();
        let id = SessionId::new();
        let filename = format!("{}.jsonl", id.as_uuid());
        let path = source_root.join(&filename);
        std::fs::write(
            &path,
            format!(
                "{}\n",
                serde_json::json!({"type":"fixed-session-mode", "sessionId":id.as_uuid(), "sessionMode":"code", "instructionEpoch":0})
            ),
        )
        .unwrap();
        let authority = bind_authority.then(|| {
            session::jsonl::writer::TranscriptFileAuthority::capture(
                source_root.clone(),
                filename.into(),
            )
            .unwrap()
            .0
        });
        let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(
            root.path().to_path_buf(),
        ));
        let durable = Arc::new(session::jsonl::DurableTranscriptWriter::open(state_root).unwrap());
        let writer = Arc::new(JsonlWriter::new(path.clone(), fs).with_durable_lock(durable));
        writer
            .activate_session_target_with_authority(
                id,
                path.clone(),
                source_root.clone(),
                authority,
            )
            .unwrap();
        Self {
            _root: root,
            source_root,
            path,
            id,
            writer,
        }
    }

    fn orchestrator(
        &self,
        replace_after_first: Option<PathBuf>,
    ) -> (ConversationOrchestrator, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        orchestrator(
            self.id,
            &self.source_root,
            Some(self.writer.clone()),
            replace_after_first,
        )
    }

    fn recaptured_writer(&self) -> Arc<JsonlWriter> {
        let authority = session::jsonl::writer::TranscriptFileAuthority::capture(
            self.source_root.clone(),
            self.path.file_name().unwrap().into(),
        )
        .unwrap()
        .0;
        let state = self
            ._root
            .path()
            .join(format!("cold-state-{}", Uuid::new_v4()));
        std::fs::create_dir(&state).unwrap();
        let writer = Arc::new(
            JsonlWriter::new(
                self.path.clone(),
                Arc::new(platform_posix::fs::PosixFileSystem::new(
                    self._root.path().to_owned(),
                )),
            )
            .with_durable_lock(Arc::new(
                session::jsonl::DurableTranscriptWriter::open(state).unwrap(),
            )),
        );
        writer
            .activate_session_target_with_authority(
                self.id,
                self.path.clone(),
                self.source_root.clone(),
                Some(authority),
            )
            .unwrap();
        writer
    }

    fn checkpoint_path(&self) -> PathBuf {
        self.source_root
            .join("deferred-tool-replay")
            .join(self.id.as_uuid().to_string())
            .join(session::jsonl::JOURNAL_FILE_NAME)
    }
}

#[tokio::test]
async fn replay_rejects_replaced_leaf_before_permission_or_tool() {
    let fixture = Fixture::new(true);
    let (orch, permissions, calls) = fixture.orchestrator(None);
    replace_leaf(&fixture.path);
    assert!(replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .is_err());
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replay_rejects_replaced_root_before_permission_or_tool() {
    let fixture = Fixture::new(true);
    let (orch, permissions, calls) = fixture.orchestrator(None);
    let bytes = std::fs::read(&fixture.path).unwrap();
    std::fs::rename(
        &fixture.source_root,
        fixture.source_root.with_extension("saved"),
    )
    .unwrap();
    std::fs::create_dir(&fixture.source_root).unwrap();
    std::fs::write(&fixture.path, bytes).unwrap();
    assert!(replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .is_err());
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replay_accepts_writer_owned_append() {
    let fixture = Fixture::new(true);
    let authority = fixture.writer.active_transcript_authority().unwrap();
    let source = authority.read_checked().unwrap();
    fixture
        .writer
        .bootstrap_session_message_identity_snapshot_with_source_authority(
            &fixture.path,
            &authority,
            &source,
        )
        .await
        .unwrap();
    let appended = serde_json::json!({
        "type": "system",
        "subtype": "local_command",
        "uuid": uuid::Uuid::new_v4(),
        "sessionId": fixture.id.as_uuid(),
        "parentUuid": null,
        "content": "startup"
    });
    fixture
        .writer
        .append_json_once_durable(
            "accepted-append",
            Utf16JsonProjection::from_serde_value(&appended).unwrap(),
        )
        .await
        .unwrap();
    let (orch, permissions, calls) = fixture.orchestrator(None);
    replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .unwrap();
    assert_eq!(permissions.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let source = std::fs::read_to_string(&fixture.path).unwrap();
    let loaded = session::jsonl::route_lines(&source);
    assert!(
        loaded.messages_in_order.iter().any(|row| row
            .tool_use_result_projection
            .as_ref()
            .is_some_and(|result| result.string_at("") == Some("called"))),
        "accepted deferred results must publish raw sidecars before persistence"
    );
}

#[tokio::test]
async fn replay_rechecks_source_between_deferred_tools() {
    let fixture = Fixture::new(true);
    let (orch, permissions, calls) = fixture.orchestrator(Some(fixture.path.clone()));
    assert!(replay_deferred_tools_after_resume(&orch, deferred(2))
        .await
        .is_err());
    assert_eq!(permissions.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn replay_rejects_missing_durable_authority() {
    let fixture = Fixture::new(false);
    let (orch, permissions, calls) = fixture.orchestrator(None);
    assert!(replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .is_err());
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn replay_without_durable_authority_fails_before_permission_or_effect() {
    let root = tempfile::tempdir().unwrap();
    let (orch, permissions, calls) = orchestrator(SessionId::new(), root.path(), None, None);
    for _ in 0..2 {
        assert!(replay_deferred_tools_after_resume(&orch, deferred(1))
            .await
            .is_err());
    }
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn successful_a_survives_b_authority_failure_and_identical_inode_replacement() {
    let fixture = Fixture::new(true);
    let (first, _, first_calls) = fixture.orchestrator(Some(fixture.path.clone()));
    assert!(replay_deferred_tools_after_resume(&first, deferred(2))
        .await
        .is_err());
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert!(
        first.session.lock().await.history.is_empty(),
        "model-visible rows stay batched"
    );
    let (cold, permissions, calls) = orchestrator(
        fixture.id,
        &fixture.source_root,
        Some(fixture.recaptured_writer()),
        None,
    );
    replay_deferred_tools_after_resume(&cold, deferred(2))
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only B executes in the cold owner"
    );
    assert_eq!(
        permissions.load(Ordering::SeqCst),
        1,
        "A does not reenter permission/dispatch"
    );
    let history = cold.session.lock().await.history.clone();
    assert_eq!(history.len(), 1);
    assert!(matches!(&history[0], ConversationMessage::User { content, .. } if content.len() == 2));
}

#[tokio::test]
async fn started_unknown_replay_never_authorizes_a_second_effect() {
    let fixture = Fixture::new(true);
    let (orch, permissions, calls) = fixture.orchestrator(None);
    let checkpoints = deferred_checkpoint::ReplayCheckpoints::open(&orch, fixture.id)
        .await
        .unwrap();
    let descriptor =
        deferred_checkpoint::ReplayDescriptor::new(fixture.id, &deferred(1).remove(0)).unwrap();
    checkpoints.start(&descriptor).await.unwrap();
    let error = replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown outcome"), "{error}");
    assert_eq!(permissions.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn malformed_or_mismatched_completion_is_rejected_before_dispatch() {
    for malformed in [false, true] {
        let fixture = Fixture::new(true);
        let (orch, permissions, calls) = fixture.orchestrator(None);
        replay_deferred_tools_after_resume(&orch, deferred(1))
            .await
            .unwrap();
        let mut retry = deferred(1);
        if malformed {
            use std::io::Write as _;
            let mut wal = std::fs::OpenOptions::new()
                .append(true)
                .open(fixture.checkpoint_path())
                .unwrap();
            wal.write_all(b"{").unwrap();
            wal.sync_all().unwrap();
        } else {
            retry[0].tool_input = Utf16JsonProjection::parse(r#"{"changed":true}"#).unwrap();
        }
        assert!(replay_deferred_tools_after_resume(&orch, retry)
            .await
            .is_err());
        assert_eq!(permissions.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        if malformed {
            assert!(
                std::fs::read(fixture.checkpoint_path())
                    .unwrap()
                    .ends_with(b"{"),
                "a torn checkpoint must not be repaired"
            );
        }
    }
}

#[tokio::test]
async fn modifier_failure_leaves_unknown_attempt_instead_of_repeating_side_effect() {
    let fixture = Fixture::new(true);
    let (orch, _, calls) = orchestrator_options(
        fixture.id,
        &fixture.source_root,
        Some(fixture.writer.clone()),
        None,
        true,
        None,
    );
    let first = replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .unwrap_err();
    assert!(first.to_string().contains("model preference"), "{first}");
    let second = replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .unwrap_err();
    assert!(second.to_string().contains("unknown outcome"), "{second}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recovered_read_completion_restores_cache_before_later_write_guard() {
    let fixture = Fixture::new(true);
    let path = fixture.source_root.join("read.txt");
    let mut work = deferred(2);
    work[0].tool_input = Utf16JsonProjection::parse(r#"{"read":true}"#).unwrap();
    work[1].tool_input = Utf16JsonProjection::parse(r#"{"check_cached_read":true}"#).unwrap();
    let (first, _, calls) = orchestrator_options(
        fixture.id,
        &fixture.source_root,
        Some(fixture.writer.clone()),
        Some(fixture.path.clone()),
        false,
        Some(path.clone()),
    );
    assert!(replay_deferred_tools_after_resume(&first, work.clone())
        .await
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let (cold, _, cold_calls) = orchestrator_options(
        fixture.id,
        &fixture.source_root,
        Some(fixture.recaptured_writer()),
        None,
        false,
        Some(path.clone()),
    );
    replay_deferred_tools_after_resume(&cold, work)
        .await
        .unwrap();
    assert_eq!(cold_calls.load(Ordering::SeqCst), 1);
    let entry = tool_api::read_file_state::get(&cold.prompt_runtime.read_state_map, &path).unwrap();
    assert_eq!(
        entry.mtime_ms,
        i64::MIN,
        "recovered snapshots must recheck present disk bytes"
    );
    let history = cold.session.lock().await.history.clone();
    assert!(
        matches!(&history[0], ConversationMessage::User { content, .. } if content.iter().all(|block| matches!(block, ContentBlock::ToolResult { is_error, .. } if *is_error != Some(true))))
    );
}
