//! Exercise guest cache maintenance, frozen instruction seeds, and live Read
//! restrictions through the normal streaming and batched model-step drivers.

use async_trait::async_trait;
use lingxi_core::host::FileSystem;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ContentBlock as HistoryBlock, ConversationMessage, MessageId};
use llm_runtime::{ContentBlock, HistoryEvent, HistoryResponse};
use orchestrator::prompt::{memory_block::InstructionFilesMode, MemoryHierarchyProvider};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, with_scripted_compactor, MockApiClient, MockOutputStream, MockStreamingApiClient,
    NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, MemoryFile, OrchestratorConfig};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tool_api::read_file_state::{self as reads, ReadFileStateMap};
use tool_api::tool_trait::Tool;
use tool_api::{ToolCallResult, ToolError, ToolProgressSender, ToolUseContext};

enum Step {
    Tool(&'static str, serde_json::Value),
    Done,
}

fn scripts(steps: &[Step]) -> (Vec<HistoryResponse>, Vec<Vec<HistoryEvent>>) {
    let mut responses = Vec::new();
    let mut streams = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let id = format!("authority-step-{index}");
        let mut events = vec![message_start(&id, "gpt-6.1-sol")];
        let (blocks, stop) = match step {
            Step::Tool(name, input) => {
                let call = format!("authority-call-{index}");
                let json = input.to_string();
                events.extend([
                    content_block_start_tool_use(0, call.clone().into(), name),
                    input_json_delta(0, &json),
                ]);
                (
                    vec![ContentBlock::ToolCall {
                        id: call,
                        name: (*name).into(),
                        input: lingxi_llm_client::protocol::JsonPayload::parse(&json).unwrap(),
                    }],
                    "tool_use",
                )
            }
            Step::Done => {
                events.extend([content_block_start_text(0), text_delta(0, "done")]);
                (
                    vec![ContentBlock::Text {
                        text: "done".into(),
                        cache_control: None,
                        citations: None,
                    }],
                    "end_turn",
                )
            }
        };
        events.extend([
            content_block_stop(0),
            message_delta_stop(stop),
            message_stop(),
        ]);
        let mut response = mock_message_response(blocks, Some(stop));
        response.id = id;
        response.model = "gpt-6.1-sol".into();
        responses.push(response);
        streams.push(events);
    }
    (responses, streams)
}

struct Fixture {
    orch: Arc<ConversationOrchestrator>,
    reader: Arc<tool_file::FileReadTool>,
    writer: Arc<tool_file::FileWriteTool>,
    map: ReadFileStateMap,
    api: Arc<MockApiClient>,
    streaming_api: Arc<MockStreamingApiClient>,
}

impl Fixture {
    async fn new(
        cwd: &Path,
        fs: Arc<dyn FileSystem>,
        memory: Arc<dyn MemoryHierarchyProvider>,
        gate: Arc<dyn lingxi_core::host::permission_gate::PermissionGate>,
        steps: Vec<Step>,
    ) -> Self {
        Self::with_reader(cwd, fs, memory, gate, steps, |reader| reader).await
    }

    async fn with_reader(
        cwd: &Path,
        fs: Arc<dyn FileSystem>,
        memory: Arc<dyn MemoryHierarchyProvider>,
        gate: Arc<dyn lingxi_core::host::permission_gate::PermissionGate>,
        steps: Vec<Step>,
        wrap: impl FnOnce(Arc<tool_file::FileReadTool>) -> Arc<dyn Tool>,
    ) -> Self {
        let map = reads::new_read_file_state_map();
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let mut context =
            tool_api::test_support::ctx_for_file_tools(fs, bus.clone(), vec![cwd.to_path_buf()]);
        context.read_file_state = map.clone();
        context.permission_gate = Some(gate.clone());
        let reader = Arc::new(tool_file::FileReadTool::new(context.clone()));
        let writer = Arc::new(tool_file::FileWriteTool::new(context));
        let mut tools = tool_api::ToolRegistry::new();
        tools.register_builtin(wrap(reader.clone()));
        tools.register_builtin(writer.clone());
        let (responses, streams) = scripts(&steps);
        let api = Arc::new(MockApiClient::new(responses));
        let streaming_api = Arc::new(MockStreamingApiClient::with_turns(streams));
        let orch = ConversationOrchestrator::into_shared(with_scripted_compactor(
            ConversationOrchestrator::new_with_streaming(
                OrchestratorConfig {
                    model: "gpt-6.1-sol".into(),
                    max_turns: 8,
                    system_prompt_override: Some("Isolated file authority fixture.".into()),
                    ..Default::default()
                },
                api.clone(),
                streaming_api.clone(),
                Arc::new(tools),
                noop_hook_executor(),
                gate,
                Arc::new(MockOutputStream::new()),
                memory,
                cwd.to_path_buf(),
            )
            .with_analytics_bus(bus)
            .with_read_state_map(map.clone())
            .with_config_home(cwd.join("config"))
            .without_host_environment_probes(),
            "file authority summary",
        ));
        orch.initialize_conversation_mode().await.unwrap();
        Self {
            orch,
            reader,
            writer,
            map,
            api,
            streaming_api,
        }
    }

    async fn run(&self, streaming: bool) {
        let outcome = if streaming {
            self.orch
                .run_turn_streaming("Inspect the file and finish.")
                .await
        } else {
            self.orch.run_turn("Inspect the file and finish.").await
        }
        .unwrap();
        assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
    }

    async fn requests(&self, streaming: bool) -> Vec<Vec<ConversationMessage>> {
        if streaming {
            assert!(
                self.api.captured_msgs().await.is_empty(),
                "no batched fallback"
            );
            self.streaming_api
                .captured_calls()
                .await
                .into_iter()
                .map(|call| call.messages)
                .collect()
        } else {
            assert!(self.streaming_api.captured_calls().await.is_empty());
            self.api.captured_msgs().await
        }
    }

    async fn read(&self, path: &Path) {
        self.reader
            .call(
                source(serde_json::json!({"file_path": path})),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
    }
}

fn source(value: serde_json::Value) -> Utf16JsonProjection {
    Utf16JsonProjection::from_serde_value(&value).unwrap()
}

fn save(path: &Path, contents: &str, seconds: u64) {
    std::fs::write(path, contents).unwrap();
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .unwrap();
}

async fn guest_read_then_write(streaming: bool) {
    for guest in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let host = cwd.join("working.txt");
        save(&host, "before\n", 1_000);
        let guest_root = format!("/file-authority-guest-{}", uuid::Uuid::new_v4());
        let requested = if guest {
            PathBuf::from(&guest_root).join("working.txt")
        } else {
            host.clone()
        };
        let fixture = Fixture::new(
            &cwd,
            tool_api::test_support::make_guest_alias_fs(&guest_root, &cwd, "/fenced"),
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![
                Step::Tool("Read", serde_json::json!({"file_path": requested})),
                Step::Tool(
                    "Write",
                    serde_json::json!({"file_path": requested, "content": "after\n"}),
                ),
                Step::Done,
            ],
        )
        .await;
        fixture.run(streaming).await;
        assert_eq!(
            std::fs::read_to_string(&host).unwrap(),
            "after\n",
            "guest={guest}"
        );
        assert_eq!(
            reads::get(&fixture.map, &requested).unwrap().content,
            "after\n"
        );
        let requests = fixture.requests(streaming).await;
        assert_eq!(requests.len(), 3);
        assert!(requests.last().unwrap().iter().all(|message| {
            !message
                .text_content()
                .contains(tool_file::FILE_NOT_READ_ERROR)
        }));
    }
}

#[tokio::test]
async fn batched_guest_read_retains_write_authority_across_model_steps() {
    guest_read_then_write(false).await;
}

#[tokio::test]
async fn streaming_guest_read_retains_write_authority_across_model_steps() {
    guest_read_then_write(true).await;
}

async fn translated_deletion(streaming: bool) {
    for guest in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let host = cwd.join("working.txt");
        save(&host, "before\n", 1_000);
        let guest_root = format!("/deleted-guest-{}", uuid::Uuid::new_v4());
        let requested = if guest {
            PathBuf::from(&guest_root).join("working.txt")
        } else {
            host.clone()
        };
        let fixture = Fixture::new(
            &cwd,
            tool_api::test_support::make_guest_alias_fs(&guest_root, &cwd, "/fenced"),
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![Step::Done],
        )
        .await;
        fixture.read(&requested).await;
        assert!(reads::get(&fixture.map, &requested).is_some());
        std::fs::remove_file(&host).unwrap();
        fixture.run(streaming).await;
        assert!(
            reads::get(&fixture.map, &requested).is_none(),
            "guest={guest}"
        );
    }
}

#[tokio::test]
async fn batched_deleted_host_and_guest_targets_drop_read_state() {
    translated_deletion(false).await;
}

#[tokio::test]
async fn streaming_deleted_host_and_guest_targets_drop_read_state() {
    translated_deletion(true).await;
}

async fn translated_changed_file_policy(streaming: bool) {
    for case in [
        "allowed",
        "allowed_unrelated_deny",
        "allowed_restricted",
        "translated",
        "landing",
    ] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let landing_root = cwd.join("landing");
        std::fs::create_dir(&landing_root).unwrap();
        let host_root = cwd.join("host-backed");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&landing_root, &host_root).unwrap();
        #[cfg(not(unix))]
        let host_root = landing_root.clone();
        let host = landing_root.join("working.txt");
        save(&host, "original\n", 1_000);
        let guest_root = format!("/changed-guest-{}", uuid::Uuid::new_v4());
        let requested = PathBuf::from(&guest_root).join("working.txt");
        let mut policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
            .with_roots(permission::FsRoots {
                cwd: cwd.clone(),
                home: Some(cwd.join("home")),
                lingxi_home: cwd.join(".lingxi"),
            });
        policy.restricted = case == "allowed_restricted";
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(NoOpPermissionGate),
        ));
        let fixture = Fixture::new(
            &cwd,
            tool_api::test_support::make_guest_alias_fs(&guest_root, &host_root, "/fenced"),
            Arc::new(StaticMemoryProvider::empty()),
            gate.clone(),
            vec![Step::Done],
        )
        .await;
        fixture.read(&requested).await;
        fixture
            .writer
            .call(
                source(
                    serde_json::json!({"file_path": requested, "content": "model-observed A\n"}),
                ),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap();
        let before = reads::get(&fixture.map, &requested).unwrap();
        let deny_pattern = match case {
            "allowed_unrelated_deny" => Some("./unrelated/**".to_owned()),
            "translated" => Some(format!(
                "./{}/**",
                host_root.file_name().unwrap().to_string_lossy()
            )),
            "landing" => Some("./landing/**".to_owned()),
            _ => None,
        };
        if let Some(pattern) = deny_pattern {
            gate.apply_permission_update(&source(serde_json::json!({
                "type":"addRules", "rules":[{"toolName":"Read", "ruleContent":pattern}],
                "behavior":"deny", "destination":"session",
            })));
        }
        save(
            &host,
            "external B\n",
            u64::try_from(before.mtime_ms / 1_000).unwrap() + 2,
        );
        fixture.run(streaming).await;
        let requests = fixture.requests(streaming).await;
        assert_eq!(requests.len(), 1);
        let exposed = requests[0]
            .iter()
            .any(|message| message.text_content().contains("external B"));
        let allowed = case.starts_with("allowed");
        assert_eq!(exposed, allowed, "case={case}");
        let current = reads::get(&fixture.map, &requested).unwrap();
        assert_eq!(
            current.content,
            if allowed {
                "external B\n"
            } else {
                "model-observed A\n"
            }
        );
        assert!(
            reads::get(&fixture.map, &host).is_none(),
            "the model key must stay in guest coordinates"
        );
    }
}

#[tokio::test]
async fn batched_guest_changed_file_refresh_uses_the_current_reader_and_captured_deny() {
    translated_changed_file_policy(false).await;
}

#[tokio::test]
async fn streaming_guest_changed_file_refresh_uses_the_current_reader_and_captured_deny() {
    translated_changed_file_policy(true).await;
}

#[cfg(unix)]
struct RetargetRead {
    reader: Arc<tool_file::FileReadTool>,
    route: PathBuf,
    denied: PathBuf,
    armed: std::sync::atomic::AtomicBool,
}
#[cfg(unix)]
#[async_trait::async_trait]
impl Tool for RetargetRead {
    fn name(&self) -> &str {
        "Read"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        self.reader.input_schema()
    }
    fn input_repair_metadata(&self) -> tool_api::ToolInputRepairMetadata {
        self.reader.input_repair_metadata()
    }
    fn is_enabled(&self, ctx: &tool_api::ToolStaticContext) -> bool {
        self.reader.is_enabled(ctx)
    }
    fn max_result_size_chars(&self) -> usize {
        self.reader.max_result_size_chars()
    }
    fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool {
        self.reader.is_concurrency_safe(input)
    }
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool {
        self.reader.is_read_only(input)
    }
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        self.reader.check_permissions(input, ctx).await
    }
    async fn description(
        &self,
        input: &Utf16JsonProjection,
        opts: &tool_api::DescriptionOptions,
    ) -> String {
        self.reader.description(input, opts).await
    }
    async fn prompt(&self, opts: &tool_api::PromptOptions) -> Utf16JsonProjection {
        self.reader.prompt(opts).await
    }
    fn read_state_io_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        self.reader.read_state_io_path(path)
    }
    async fn validate_input(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> Result<(), tool_api::ValidationError> {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            std::fs::remove_file(&self.route).unwrap();
            std::os::unix::fs::symlink(&self.denied, &self.route).unwrap();
        }
        Ok(())
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        self.reader.call(input, ctx, tx).await
    }
}

#[cfg(unix)]
async fn changed_file_retarget_cannot_acquire_denied_bytes(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let allowed = cwd.join("allowed.txt");
    let denied = cwd.join("secret.txt");
    let route = cwd.join("route.txt");
    save(&allowed, "old allowed\n", 1_000);
    save(&denied, "DENIED_SECRET_BYTES\n", 2_000);
    std::os::unix::fs::symlink(&allowed, &route).unwrap();
    let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default).with_roots(
        permission::FsRoots {
            cwd: cwd.clone(),
            home: Some(cwd.join("home")),
            lingxi_home: cwd.join(".lingxi"),
        },
    );
    let gate = Arc::new(permission::PolicyPermissionGate::new(
        Arc::new(policy),
        Arc::new(NoOpPermissionGate),
    ));
    gate.apply_permission_update(&source(serde_json::json!({
        "type":"addRules", "rules":[{"toolName":"Read","ruleContent":"./secret.txt"}],
        "behavior":"deny", "destination":"session",
    })));
    let fixture = Fixture::with_reader(
        &cwd,
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        Arc::new(StaticMemoryProvider::empty()),
        gate,
        vec![Step::Done, Step::Done, Step::Done],
        |reader| {
            Arc::new(RetargetRead {
                reader,
                route: route.clone(),
                denied: denied.clone(),
                armed: std::sync::atomic::AtomicBool::new(true),
            })
        },
    )
    .await;
    fixture.read(&route).await;
    // A model-visible complete snapshot triggers the internal scan. Keep the
    // original symlink route so the barrier can replace its landing.
    let mut entry = reads::get(&fixture.map, &route).unwrap();
    entry.offset = None;
    entry.limit = None;
    reads::set(&fixture.map, route.clone(), entry);
    save(&allowed, "NORMAL_EXTERNAL_UPDATE\n", 3_000);
    fixture.run(streaming).await;
    let calls = fixture.requests(streaming).await;
    assert!(calls[0]
        .iter()
        .all(|row| !row.text_content().contains("DENIED_SECRET_BYTES")));
    assert!(reads::get(&fixture.map, &denied).is_none());
    assert!(
        fixture
            .writer
            .call(
                source(serde_json::json!({"file_path":denied,"content":"overwrite"})),
                tool_api::test_support::fresh_ctx(),
                tool_api::progress_channel().0
            )
            .await
            .is_err(),
        "a rejected internal Read cannot grant Write authority"
    );
    assert_eq!(
        std::fs::read_to_string(&denied).unwrap(),
        "DENIED_SECRET_BYTES\n"
    );
    std::fs::remove_file(&route).unwrap();
    std::os::unix::fs::symlink(&allowed, &route).unwrap();
    fixture.run(streaming).await;
    fixture.run(streaming).await;
    let calls = fixture.requests(streaming).await;
    assert!(calls[1]
        .iter()
        .any(|row| row.text_content().contains("NORMAL_EXTERNAL_UPDATE")));
    let notices = |rows: &[ConversationMessage]| {
        rows.iter()
            .filter(|row| row.text_content().contains("NORMAL_EXTERNAL_UPDATE"))
            .map(|row| (row.id(), serde_json::to_vec(row).unwrap()))
            .collect::<Vec<_>>()
    };
    assert_eq!(notices(&calls[1]).len(), 1);
    assert_eq!(
        notices(&calls[1]),
        notices(&calls[2]),
        "the durable notice retains one unchanged identity and source on the next request"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn batched_changed_file_retarget_preserves_deny_write_protection_and_normal_notifications() {
    changed_file_retarget_cannot_acquire_denied_bytes(false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn streaming_changed_file_retarget_preserves_deny_write_protection_and_normal_notifications()
{
    changed_file_retarget_cannot_acquire_denied_bytes(true).await;
}

struct ChangedDuringAcquisition {
    path: PathBuf,
    acquired: AtomicBool,
}

#[async_trait]
impl MemoryHierarchyProvider for ChangedDuringAcquisition {
    async fn load(&self, _: &Path) -> Vec<MemoryFile> {
        let loaded = memory::lingxi_md::loader::load_file(&self.path, None).unwrap();
        assert!(
            !self.acquired.swap(true, Ordering::SeqCst),
            "eager files are acquired once"
        );
        save(&self.path, "UNSEEN-EXTERNAL-B\n", 2_000);
        vec![MemoryFile {
            path: loaded.path,
            parent: None,
            source_content: Some(loaded.body.clone()),
            body: loaded.body.clone(),
            raw_content: loaded.body,
            is_local_override: false,
            tier: memory::lingxi_md::LingxiMdTier::Project,
            globs: None,
            content_differs_from_disk: false,
        }]
    }

    async fn load_conditional_rules(
        &self,
        _: &Path,
        _: &Path,
        _: InstructionFilesMode,
    ) -> Vec<MemoryFile> {
        Vec::new()
    }
}

async fn frozen_instruction_seed(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let path = cwd.join("LINGXI.md");
    save(&path, "MODEL-OBSERVATION-A\n", 1_000);
    let fixture = Fixture::new(
        &cwd,
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        Arc::new(ChangedDuringAcquisition {
            path: path.clone(),
            acquired: AtomicBool::new(false),
        }),
        Arc::new(NoOpPermissionGate),
        vec![Step::Done],
    )
    .await;
    fixture.orch.fire_instructions_loaded().await;
    let entry = reads::get(&fixture.map, &path).unwrap();
    assert_eq!(entry.content, "MODEL-OBSERVATION-A\n");
    assert_eq!(entry.mtime_ms, i64::MIN, "B's mtime must not bless A");
    let write = fixture
        .writer
        .call(
            source(serde_json::json!({"file_path": path, "content": "replacement C\n"})),
            tool_api::test_support::fresh_ctx(),
            tool_api::test_support::fresh_tx(),
        )
        .await;
    assert!(
        matches!(write, Err(tool_api::ToolError::InvalidInput(message))
        if message.string_at("") == Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)),
        "the model has not yet received B, so the stale-content guard must reject it"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "UNSEEN-EXTERNAL-B\n"
    );
    fixture.run(streaming).await;
    let requests = fixture.requests(streaming).await;
    assert_eq!(requests.len(), 1);
    let text = requests[0]
        .iter()
        .map(ConversationMessage::text_content)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("MODEL-OBSERVATION-A"),
        "the frozen instructions still contain A"
    );
    assert!(
        text.contains("UNSEEN-EXTERNAL-B"),
        "the changed-file notice must expose B before a model write"
    );
    assert!(text.contains("changed on disk since you last read it."));
    assert_eq!(
        reads::get(&fixture.map, &path).unwrap().content,
        "UNSEEN-EXTERNAL-B\n"
    );
}

#[tokio::test]
async fn batched_frozen_instruction_seed_does_not_authorize_unseen_edits() {
    frozen_instruction_seed(false).await;
}

#[tokio::test]
async fn streaming_frozen_instruction_seed_does_not_authorize_unseen_edits() {
    frozen_instruction_seed(true).await;
}

async fn live_deny_compaction(streaming: bool) {
    for denied_spelling in [
        "requested",
        "translated",
        "landing",
        "all_reads",
        "allowed",
        "allowed_restricted",
    ] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        std::fs::create_dir(cwd.join("landing")).unwrap();
        let host = cwd.join("landing/working.txt");
        save(&host, "ORIGINAL-READ-A\n", 1_000);
        let translated_root = cwd.join("host-route");
        #[cfg(unix)]
        std::os::unix::fs::symlink(cwd.join("landing"), &translated_root).unwrap();
        #[cfg(not(unix))]
        let translated_root = cwd.join("landing");
        let guest_root = format!("/compact-deny-guest-{}", uuid::Uuid::new_v4());
        let requested = PathBuf::from(&guest_root).join("working.txt");
        let mut policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
            .with_roots(permission::FsRoots {
                cwd: cwd.clone(),
                home: Some(cwd.join("home")),
                lingxi_home: cwd.join(".lingxi"),
            });
        policy.restricted = denied_spelling == "allowed_restricted";
        let gate = Arc::new(permission::PolicyPermissionGate::new(
            Arc::new(policy),
            Arc::new(NoOpPermissionGate),
        ));
        let fixture = Fixture::new(
            &cwd,
            tool_api::test_support::make_guest_alias_fs(&guest_root, &translated_root, "/fenced"),
            Arc::new(StaticMemoryProvider::empty()),
            gate.clone(),
            vec![Step::Done],
        )
        .await;
        fixture.read(&requested).await;
        for index in 0..6 {
            let message = if index % 2 == 0 {
                ConversationMessage::user(MessageId::new(), format!("question-{index}"))
            } else {
                ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content: vec![HistoryBlock::Text {
                        text: format!("answer-{index}"),
                        citations: None,
                    }],
                    stop_reason: Some("end_turn".into()),
                    resumable: None,
                    per_turn_effort: None,
                }
            };
            fixture.orch.append_external_history_message(message).await;
        }
        let allowed = denied_spelling.starts_with("allowed");
        if !allowed {
            let rule = match denied_spelling {
                "requested" => {
                    serde_json::json!({"toolName":"Read", "ruleContent":format!("/{}/**", guest_root)})
                }
                "translated" => serde_json::json!({
                    "toolName":"Read", "ruleContent":format!("./{}/**", translated_root.file_name().unwrap().to_string_lossy()),
                }),
                "landing" => serde_json::json!({"toolName":"Read", "ruleContent":"./landing/**"}),
                "all_reads" => serde_json::json!({"toolName":"Read"}),
                _ => unreachable!(),
            };
            gate.apply_permission_update(&source(serde_json::json!({
                "type":"addRules", "rules":[rule], "behavior":"deny", "destination":"session",
            })));
        }
        save(&host, "FRESH-COMPACTION-B\n", 2_000);
        fixture
            .orch
            .force_compact_with_cancel(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        if allowed {
            // Restore deliberately retains only the guest spelling. A later
            // external edit must still resolve through the reader's host map.
            save(&host, "POST-RESTORE-EXTERNAL-C\n", 3_000);
        }
        fixture.run(streaming).await;
        let requests = fixture.requests(streaming).await;
        assert_eq!(requests.len(), 1);
        let exposed = requests[0].iter().any(|message| {
            message.text_content().contains(if allowed {
                "POST-RESTORE-EXTERNAL-C"
            } else {
                "FRESH-COMPACTION-B"
            })
        });
        assert_eq!(exposed, allowed, "deny={denied_spelling}");
        assert_eq!(
            reads::get(&fixture.map, &requested).is_some(),
            allowed,
            "deny={denied_spelling}"
        );
    }
}

#[tokio::test]
async fn batched_compaction_observes_live_guest_host_and_landing_read_denies() {
    live_deny_compaction(false).await;
}

#[tokio::test]
async fn streaming_compaction_observes_live_guest_host_and_landing_read_denies() {
    live_deny_compaction(true).await;
}

async fn utf16_compaction_restores_exact_read_content(streaming: bool) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let path = cwd.join("utf16.txt");
    let exact = vec![97, 13, 10, 0xd800, 98];
    let raw = std::iter::once(0xfeffu16)
        .chain(exact.iter().copied())
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(&path, raw).unwrap();
    let fixture = Fixture::new(
        &cwd,
        Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
        Arc::new(StaticMemoryProvider::empty()),
        Arc::new(NoOpPermissionGate),
        vec![Step::Done],
    )
    .await;
    fixture.read(&path).await;
    for index in 0..8 {
        let message = if index % 2 == 0 {
            ConversationMessage::user(MessageId::new(), format!("prior row {index}"))
        } else {
            ConversationMessage::Assistant {
                id: MessageId::new(),
                content: vec![HistoryBlock::Text {
                    text: format!("prior answer {index}"),
                    citations: None,
                }],
                stop_reason: Some("end_turn".into()),
                per_turn_effort: None,
                resumable: None,
            }
        };
        fixture.orch.append_external_history_message(message).await;
    }
    fixture
        .orch
        .force_compact_with_cancel(tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        reads::get_content_units(&fixture.map, &path),
        Some(vec![97, 10, 0xd800, 98])
    );
    fixture.run(streaming).await;
    let calls = fixture.requests(streaming).await;
    let expected_result = "<system-reminder>\nResult of calling the Read tool:\n1\ta\n2\t"
        .encode_utf16()
        .chain([0xd800, 98])
        .chain("\n</system-reminder>".encode_utf16())
        .collect::<Vec<_>>();
    assert!(
        calls[0].iter().any(|row| {
            match row {
        ConversationMessage::User {content, ..} => content.iter().any(|block| matches!(block,
            HistoryBlock::TextJsUtf16 {utf16_code_units, ..} if utf16_code_units == &expected_result
        )), _ => false,
    }
        }),
        "the restored model attachment must retain exact decoded code units"
    );
}
#[tokio::test]
async fn batched_utf16_compaction_uses_read_decoding_and_exact_attachment_units() {
    utf16_compaction_restores_exact_read_content(false).await;
}
#[tokio::test]
async fn streaming_utf16_compaction_uses_read_decoding_and_exact_attachment_units() {
    utf16_compaction_restores_exact_read_content(true).await;
}
