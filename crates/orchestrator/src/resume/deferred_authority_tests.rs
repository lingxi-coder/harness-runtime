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
    let row = |value: serde_json::Value| JsonlMessage::from_projection(
        &Utf16JsonProjection::from_serde_value(&value).unwrap()).unwrap();
    let assistant = row(serde_json::json!({"type":"assistant","uuid":physical,"parentUuid":null,"sessionId":session,
        "message":{"id":logical,"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"LocalApp","input":{}}]}}));
    let attachment = row(serde_json::json!({"type":"attachment","uuid":Uuid::new_v4().to_string(),"parentUuid":physical,"sessionId":session,
        "attachment":{"type":"hook_deferred_tool","toolUseID":"call-1","toolName":"LocalApp","toolInput":{},"assistantMessageId":logical}}));
    let loaded = deferred_tool_replays_from_messages(&[assistant.clone(), attachment.clone()]);
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].assistant_message_id.unwrap().as_uuid().to_string(), physical);
    let missing = deferred_tool_replays_from_messages(&[attachment.clone()]);
    assert_eq!(missing[0].assistant_message_id, None);
    let duplicated = deferred_tool_replays_from_messages(&[assistant.clone(), assistant, attachment]);
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
        _: Utf16JsonProjection,
        _: tool_api::ToolUseContext,
        _: tool_api::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        let previous = self.calls.fetch_add(1, Ordering::SeqCst);
        if previous == 0 {
            if let Some(path) = &self.replace_after_first {
                replace_leaf(path);
            }
        }
        Ok(tool_api::ToolCallResult::from_data("called".into()))
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
    let permissions = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register_builtin(Arc::new(CountingTool {
        calls: calls.clone(),
        replace_after_first,
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
    .with_session_id(id);
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
async fn replay_without_durable_writer_retains_in_memory_behavior() {
    let root = tempfile::tempdir().unwrap();
    let (orch, permissions, calls) = orchestrator(SessionId::new(), root.path(), None, None);
    replay_deferred_tools_after_resume(&orch, deferred(1))
        .await
        .unwrap();
    assert_eq!(permissions.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
