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
    scripts_for_model(steps, "gpt-6.1-sol")
}

fn scripts_for_model(
    steps: &[Step],
    model: &str,
) -> (Vec<HistoryResponse>, Vec<Vec<HistoryEvent>>) {
    let mut responses = Vec::new();
    let mut streams = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let id = format!("authority-step-{index}");
        let mut events = vec![message_start(&id, model)];
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
        response.model = model.into();
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
        Self::with_recording(cwd, fs, memory, gate, steps, wrap, None).await
    }

    async fn with_recording(
        cwd: &Path,
        fs: Arc<dyn FileSystem>,
        memory: Arc<dyn MemoryHierarchyProvider>,
        gate: Arc<dyn lingxi_core::host::permission_gate::PermissionGate>,
        steps: Vec<Step>,
        wrap: impl FnOnce(Arc<tool_file::FileReadTool>) -> Arc<dyn Tool>,
        transcript_writer: Option<Arc<session::JsonlWriter>>,
    ) -> Self {
        let map = reads::new_read_file_state_map();
        let bus = Arc::new(telemetry::AnalyticsBus::new());
        let mut context =
            tool_api::test_support::ctx_for_file_tools(fs, bus.clone(), vec![cwd.to_path_buf()]);
        context.read_file_state = map.clone();
        context.permission_gate = Some(gate.clone());
        let reader = Arc::new(tool_file::FileReadTool::new(context.clone()));
        let editor = Arc::new(tool_file::FileEditTool::new(context.clone()));
        let writer = Arc::new(tool_file::FileWriteTool::new(context));
        let mut tools = tool_api::ToolRegistry::new();
        tools.register_builtin(wrap(reader.clone()));
        tools.register_builtin(writer.clone());
        tools.register_builtin(editor);
        let (responses, streams) = scripts(&steps);
        let api = Arc::new(MockApiClient::new(responses));
        let streaming_api = Arc::new(MockStreamingApiClient::with_turns(streams));
        let mut orch = with_scripted_compactor(
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
        );
        if let Some(writer) = transcript_writer {
            let id =
                uuid::Uuid::parse_str(writer.active_path().file_stem().unwrap().to_str().unwrap())
                    .unwrap();
            orch = orch
                .with_session_id(lingxi_core::types::SessionId::from_uuid(id))
                .with_jsonl_writer(writer);
        }
        let orch = ConversationOrchestrator::into_shared(orch);
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
    fn read_state_request_path(&self, path: &Path) -> PathBuf {
        self.reader.read_state_request_path(path)
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
async fn changed_file_retarget_cannot_acquire_denied_bytes(
    streaming: bool,
    noncanonical_seed: bool,
) {
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
    if noncanonical_seed {
        // The public SDK seed preserves the host's original spelling. Read
        // trims it later, so the request binding must survive that difference.
        let raw_route = cwd.join("route.txt ");
        save(&raw_route, "HOST_SEED_OLD\n", 1_000);
        assert!(
            fixture
                .orch
                .seed_read_state_from_host(raw_route.to_str().unwrap(), 1_000_000.0)
                .await
        );
        assert!(reads::get(&fixture.map, &raw_route).is_some());
    } else {
        fixture.read(&route).await;
        // A model-visible complete snapshot triggers the internal scan. Keep
        // the original symlink route so the barrier can replace its landing.
        let mut entry = reads::get(&fixture.map, &route).unwrap();
        entry.offset = None;
        entry.limit = None;
        reads::set(&fixture.map, route.clone(), entry);
    }
    save(&allowed, "NORMAL_EXTERNAL_UPDATE\n", 3_000);
    fixture.run(streaming).await;
    let calls = fixture.requests(streaming).await;
    assert!(calls[0]
        .iter()
        .all(|row| !row.text_content().contains("DENIED_SECRET_BYTES")));
    assert!(reads::get(&fixture.map, &denied).is_none());
    assert!(reads::get(&fixture.map, &route)
        .is_none_or(|entry| !entry.content.contains("DENIED_SECRET_BYTES")));
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
    changed_file_retarget_cannot_acquire_denied_bytes(false, false).await;
}
#[cfg(unix)]
#[tokio::test]
async fn streaming_changed_file_retarget_preserves_deny_write_protection_and_normal_notifications()
{
    changed_file_retarget_cannot_acquire_denied_bytes(true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn batched_noncanonical_sdk_seed_preserves_read_binding_and_notice_dedup() {
    changed_file_retarget_cannot_acquire_denied_bytes(false, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn streaming_noncanonical_sdk_seed_preserves_read_binding_and_notice_dedup() {
    changed_file_retarget_cannot_acquire_denied_bytes(true, true).await;
}

#[cfg(unix)]
async fn noncanonical_seed_keeps_existing_logical_observation(streaming: bool) {
    for newer_alias in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let path = cwd.join("known.txt");
        let raw = cwd.join("known.txt ");
        let fixture = Fixture::new(
            &cwd,
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![
                Step::Tool(
                    "Write",
                    serde_json::json!({"file_path":path,"content":"OLD\n"}),
                ),
                Step::Done,
                Step::Done,
                Step::Done,
            ],
        )
        .await;
        fixture.run(streaming).await;
        let known = reads::get(&fixture.map, &path).unwrap();
        let seconds = known.mtime_ms as u64 / 1000;
        save(&raw, "NEW\n", if newer_alias { seconds + 100 } else { 1 });
        assert!(
            fixture
                .orch
                .seed_read_state_from_host(raw.to_str().unwrap(), f64::MAX)
                .await
        );
        save(&path, "NEW\n", seconds + 10);
        fixture.run(streaming).await;
        fixture.run(streaming).await;
        let requests = fixture.requests(streaming).await;
        let notices = |messages: &[ConversationMessage]| {
            messages
                .iter()
                .filter(|message| {
                    message
                        .text_content()
                        .contains("changed on disk since you last read it")
                })
                .map(|message| (message.id(), message.text_content()))
                .collect::<Vec<_>>()
        };
        let first = notices(&requests[2]);
        assert_eq!(
            first.len(),
            1,
            "the MRU host alias cannot hide a model-visible change"
        );
        assert!(first[0].1.contains("NEW"));
        assert_eq!(
            notices(&requests[3]),
            first,
            "the notice keeps one stable identity"
        );
        assert!(reads::get(&fixture.map, &raw).is_none());
        assert_eq!(reads::get(&fixture.map, &path).unwrap().content, "NEW\n");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn batched_noncanonical_seed_cannot_hide_or_duplicate_a_logical_file_change() {
    noncanonical_seed_keeps_existing_logical_observation(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn streaming_noncanonical_seed_cannot_hide_or_duplicate_a_logical_file_change() {
    noncanonical_seed_keeps_existing_logical_observation(true).await;
}

async fn cold_written_file_keeps_normalized_guard(streaming: bool) {
    use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};
    for original in [
        "before\ntail\n",
        "before\r\ntail\r\n",
        "\u{feff}before\r\ntail\r\n",
    ] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let path = cwd.join("created.txt");
        let transcript = cwd.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
        let live = Fixture::with_recording(
            &cwd,
            fs.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![
                Step::Tool(
                    "Write",
                    serde_json::json!({"file_path":path,"content":original}),
                ),
                Step::Done,
            ],
            |reader| reader,
            Some(Arc::new(session::JsonlWriter::new(
                transcript.clone(),
                fs.clone(),
            ))),
        )
        .await;
        live.run(streaming).await;
        assert_eq!(
            reads::get(&live.map, &path).unwrap().content,
            "before\ntail\n"
        );
        let id = live.orch.current_session_id().await.as_uuid();
        let source_text = std::fs::read_to_string(&transcript).unwrap();
        let replayed = orchestrator::resume::replay_captured_session(
            transcript,
            source_text.clone(),
            session::jsonl::route_lines(&source_text),
            id,
        )
        .unwrap();
        let cold = Fixture::new(
            &cwd,
            fs,
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![
                Step::Tool(
                    "Write",
                    serde_json::json!({"file_path":path,"content":"after\n"}),
                ),
                Step::Done,
            ],
        )
        .await;
        cold.orch
            .restore_captured_session_history(
                id,
                &replayed.messages,
                &session::jsonl::route_lines(&source_text).messages_in_order,
                true,
                permission::PermissionMode::Default,
                ConversationMode::Code,
                &source_text,
            )
            .await
            .unwrap();
        let restored = reads::get(&cold.map, &path).unwrap();
        assert_eq!(restored.mtime_ms, i64::MIN);
        assert_eq!(restored.content, "before\ntail\n");
        assert!(!restored.is_partial_view);

        // Check the recovered authority before a changed-file scan can read a
        // later edit on the model's behalf. A rollback mtime must not waive it.
        let disk = std::fs::read(&path).unwrap();
        save(&path, "external edit\r\n", 1);
        let error = cold
            .writer
            .call(
                source(serde_json::json!({"file_path":path,"content":"overwrite"})),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            error.model_facing_message().string_at(""),
            Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external edit\r\n");
        std::fs::write(&path, disk).unwrap();
        cold.writer
            .call(
                source(serde_json::json!({"file_path":path,"content":original})),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("an unchanged CRLF file must remain writable immediately after cold restore");
        cold.run(streaming).await;
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            if original.starts_with('\u{feff}') {
                "\u{feff}after\n"
            } else {
                "after\n"
            },
        );
        assert_eq!(cold.requests(streaming).await.len(), 2);
    }
}

#[tokio::test]
async fn batched_crlf_write_cold_restore_preserves_write_guard_and_next_turn() {
    cold_written_file_keeps_normalized_guard(false).await;
}

#[tokio::test]
async fn streaming_crlf_write_cold_restore_preserves_write_guard_and_next_turn() {
    cold_written_file_keeps_normalized_guard(true).await;
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

async fn screenshot_fallback_observes_target_policy(streaming: bool) {
    use base64::Engine;

    let links: &[bool] = if cfg!(unix) { &[false, true] } else { &[false] };
    for &linked in links {
        for guest in [false, true] {
            for denied in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let cwd = root.path().canonicalize().unwrap();
                let guest_root = format!("/file-fallback-{}", uuid::Uuid::new_v4());
                let requested_root = if guest {
                    PathBuf::from(&guest_root)
                } else {
                    cwd.clone()
                };
                let missing = requested_root.join("private AM.png");
                let alternate = requested_root.join("private\u{202f}AM.png");
                let host = cwd.join("private\u{202f}AM.png");
                let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAACCAIAAAD91JpzAAAAC0lEQVR4nGNgQAYAAA4AAamRc7EAAAAASUVORK5CYII=";
                std::fs::write(
                    &host,
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .unwrap(),
                )
                .unwrap();
                if linked {
                    let landing = cwd.join("secret.png");
                    std::fs::rename(&host, &landing).unwrap();
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&landing, &host).unwrap();
                }
                let policy = permission::PermissionPolicy::new(permission::PermissionMode::Default)
                    .with_roots(permission::FsRoots {
                        cwd: cwd.clone(),
                        home: Some(cwd.join("home")),
                        lingxi_home: cwd.join(".lingxi"),
                    });
                let gate = Arc::new(permission::PolicyPermissionGate::new(
                    Arc::new(policy),
                    Arc::new(NoOpPermissionGate),
                ));
                if denied {
                    gate.apply_permission_update(&source(serde_json::json!({
                        "type": "addRules",
                        "rules": [{"toolName": "Read", "ruleContent": if linked {
                            "./secret.png"
                        } else { "./private\u{202f}AM.png" }}],
                        "behavior": "deny", "destination": "session",
                    })));
                }
                let fixture = Fixture::new(
                    &cwd,
                    tool_api::test_support::make_guest_alias_fs(&guest_root, &cwd, "/fenced"),
                    Arc::new(StaticMemoryProvider::empty()),
                    gate,
                    vec![
                        Step::Tool("Read", serde_json::json!({"file_path": missing})),
                        Step::Done,
                    ],
                )
                .await;
                fixture.run(streaming).await;
                let requests = fixture.requests(streaming).await;
                let result = requests[1]
                    .iter()
                    .find_map(|message| match message {
                        ConversationMessage::User { content, .. } => {
                            content.iter().find_map(|block| match block {
                                HistoryBlock::ToolResult { is_error, .. } => {
                                    Some(*is_error == Some(true))
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .expect("the Read produces its actual tool result");
                assert_eq!(
                    result, denied,
                    "linked={linked}, guest={guest}, denied={denied}"
                );
                if denied {
                    assert!(!serde_json::to_string(&requests[1])
                        .unwrap()
                        .contains(encoded));
                    assert!(reads::get(&fixture.map, &alternate).is_none());
                }
            }
        }
    }
}

#[tokio::test]
async fn batched_screenshot_fallback_rechecks_target_read_policy() {
    screenshot_fallback_observes_target_policy(false).await;
}

#[tokio::test]
async fn streaming_screenshot_fallback_rechecks_target_read_policy() {
    screenshot_fallback_observes_target_policy(true).await;
}

async fn complete_read_raw_alias_does_not_invent_change(streaming: bool) {
    for canonical_mru in [false, true] {
        for seed_alias in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let cwd = root.path().canonicalize().unwrap();
            let path = cwd.join("known.txt");
            let raw = cwd.join("known.txt ");
            save(&path, "ALREADY_SEEN_CONTENT\n", 2_000);
            save(&raw, "UNRELATED_HOST_SEED_OLD\n", 1_000);
            let fixture = Fixture::new(
                &cwd,
                Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone())),
                Arc::new(StaticMemoryProvider::empty()),
                Arc::new(NoOpPermissionGate),
                vec![
                    Step::Tool("Read", serde_json::json!({"file_path": path})),
                    Step::Done,
                    Step::Done,
                    Step::Done,
                ],
            )
            .await;
            fixture.run(streaming).await;
            let complete = reads::get(&fixture.map, &path).unwrap();
            assert_eq!(complete.offset, Some(1));
            assert!(complete.limit.is_none() && !complete.is_partial_view);
            if seed_alias {
                assert!(
                    fixture
                        .orch
                        .seed_read_state_from_host(raw.to_str().unwrap(), f64::MAX)
                        .await
                );
            }
            if canonical_mru {
                reads::get(&fixture.map, &path).unwrap();
            }
            fixture.run(streaming).await;
            fixture.run(streaming).await;
            let requests = fixture.requests(streaming).await;
            for request in &requests[2..] {
                assert!(
                    request.iter().all(|message| !message
                        .text_content()
                        .contains("changed on disk since you last read it")),
                    "canonical_mru={canonical_mru}, seed_alias={seed_alias}"
                );
            }
            assert_eq!(reads::get(&fixture.map, &path).unwrap(), complete);
            if seed_alias {
                assert!(reads::get(&fixture.map, &raw).is_none());
            }
        }
    }
}

#[tokio::test]
async fn batched_complete_read_raw_alias_does_not_invent_change() {
    complete_read_raw_alias_does_not_invent_change(false).await;
}

#[tokio::test]
async fn streaming_complete_read_raw_alias_does_not_invent_change() {
    complete_read_raw_alias_does_not_invent_change(true).await;
}

async fn partial_edit_restore_fixture(
    cwd: &Path,
    fs: Arc<dyn FileSystem>,
    gate: Arc<dyn lingxi_core::host::permission_gate::PermissionGate>,
    steps: Vec<Step>,
    transcript: Option<Arc<session::JsonlWriter>>,
) -> Fixture {
    let map = reads::new_read_file_state_map();
    let bus = Arc::new(telemetry::AnalyticsBus::new());
    let mut context =
        tool_api::test_support::ctx_for_file_tools(fs, bus.clone(), vec![cwd.to_path_buf()]);
    context.read_file_state = map.clone();
    context.permission_gate = Some(gate.clone());
    let reader = Arc::new(tool_file::FileReadTool::new(context.clone()));
    let writer = Arc::new(tool_file::FileWriteTool::new(context.clone()));
    let mut tools = tool_api::ToolRegistry::new();
    tools.register_builtin(reader.clone());
    tools.register_builtin(writer.clone());
    tools.register_builtin(Arc::new(tool_file::FileEditTool::new(context)));
    let (responses, streams) = scripts_for_model(&steps, "claude-sonnet-4-6");
    let api = Arc::new(MockApiClient::new(responses));
    let streaming_api = Arc::new(MockStreamingApiClient::with_turns(streams));
    let mut orch = ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig {
            model: "claude-sonnet-4-6".into(),
            max_turns: 8,
            system_prompt_override: Some("Partial Edit restoration fixture.".into()),
            ..Default::default()
        },
        api.clone(),
        streaming_api.clone(),
        Arc::new(tools),
        noop_hook_executor(),
        gate,
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        cwd.to_path_buf(),
    )
    .with_analytics_bus(bus)
    .with_read_state_map(map.clone())
    .with_config_home(cwd.join("config"))
    .without_host_environment_probes();
    if let Some(writer) = transcript {
        let id = uuid::Uuid::parse_str(writer.active_path().file_stem().unwrap().to_str().unwrap())
            .unwrap();
        orch = orch
            .with_session_id(lingxi_core::types::SessionId::from_uuid(id))
            .with_jsonl_writer(writer);
    }
    let orch = ConversationOrchestrator::into_shared(orch);
    orch.initialize_conversation_mode().await.unwrap();
    Fixture {
        orch,
        reader,
        writer,
        map,
        api,
        streaming_api,
    }
}

#[test]
fn cold_partial_edit_replay_preserves_write_fence_both_drivers() {
    // The supported Read limit and production's once-published permission
    // probe belong to this process. Isolate them from parallel integration tests.
    const CHILD: &str = "HARNESS_PARTIAL_EDIT_REPLAY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cold_partial_edit_replay_preserves_write_fence_both_drivers",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env(tool_file::read::MAX_OUTPUT_TOKENS_ENV, "100")
            .env("RUST_MIN_STACK", "33554432")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};

        let common = tempfile::tempdir().unwrap();
        let root = common.path().canonicalize().unwrap();
        let policy = Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            [permission::PermissionRule::allow_tool_session("Read")],
        ).with_roots(permission::FsRoots {
            cwd: root.clone(), home: Some(root.join("home")), lingxi_home: root.join(".lingxi"),
        }));
        lingxi_core::host::read_auto_allow::set_read_auto_allow_probe(Arc::new(
            permission::read_auto_allow::PolicyReadAutoAllow::new(
                policy.clone(), vec!["Read".into(), "Edit".into(), "Write".into()],
            ),
        ));
        let gate = Arc::new(permission::PolicyPermissionGate::new(policy, Arc::new(NoOpPermissionGate)));
        for streaming in [false, true] {
            let temp = tempfile::tempdir_in(&root).unwrap();
            let cwd = temp.path().canonicalize().unwrap();
            let path = cwd.join("dense.txt");
            let original = (0..220u32).map(|index| format!("line {index:05}\n"))
                .collect::<String>();
            std::fs::write(&path, &original).unwrap();
            assert!(tool_file::read_requirement_waived(Some("claude-sonnet-4-6"), &path));
            let transcript = cwd.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
            let fs: Arc<dyn FileSystem> =
                Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
            let live = partial_edit_restore_fixture(&cwd, fs.clone(), gate.clone(), vec![
                Step::Tool("Read", serde_json::json!({"file_path": path})),
                Step::Tool("Edit", serde_json::json!({"file_path": path,
                    "old_string": "line 00000", "new_string": "EDITED FIRST LINE"})),
                Step::Done,
            ], Some(Arc::new(session::JsonlWriter::new(transcript.clone(), fs.clone())))).await;
            live.run(streaming).await;
            assert!(reads::get(&live.map, &path).unwrap().is_partial_view);
            let before_restore = std::fs::read(&path).unwrap();
            assert!(String::from_utf8_lossy(&before_restore).starts_with("EDITED FIRST LINE\n"));
            let replacement = source(serde_json::json!({"file_path": path, "content": "overwrite"}));
            let error = live.writer.call(replacement.clone(),
                tool_api::test_support::fresh_ctx(), tool_api::test_support::fresh_tx()).await.unwrap_err();
            assert_eq!(error.model_facing_message().string_at(""), Some(tool_file::FILE_NOT_READ_ERROR));

            let id = live.orch.current_session_id().await.as_uuid();
            let raw = std::fs::read_to_string(&transcript).unwrap();
            assert!(raw.contains("contentNotInModelContext"));
            let replay = orchestrator::resume::replay_captured_session(
                transcript, raw.clone(), session::jsonl::route_lines(&raw), id,
            ).unwrap();
            let cold = partial_edit_restore_fixture(&cwd, fs, gate.clone(), vec![Step::Done], None).await;
            cold.orch.restore_captured_session_history(
                id, &replay.messages, &session::jsonl::route_lines(&raw).messages_in_order,
                true, permission::PermissionMode::Default, ConversationMode::Code, &raw,
            ).await.unwrap();
            let restored = reads::get(&cold.map, &path).unwrap();
            assert!(restored.is_partial_view);
            assert_eq!(restored.mtime_ms, i64::MIN);
            let error = cold.writer.call(replacement,
                tool_api::test_support::fresh_ctx(), tool_api::test_support::fresh_tx()).await.unwrap_err();
            assert_eq!(error.model_facing_message().string_at(""), Some(tool_file::FILE_NOT_READ_ERROR));
            assert_eq!(std::fs::read(&path).unwrap(), before_restore);
        }
    });
}

const EXACT_CHANGED_NOTE: &str = "changed on disk since you last read it.";

#[test]
fn stale_edit_keeps_unseen_content_write_protected_across_recovery_both_drivers() {
    const CHILD: &str = "HARNESS_STALE_EDIT_WRITE_FENCE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stale_edit_keeps_unseen_content_write_protected_across_recovery_both_drivers",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .env("RUST_MIN_STACK", "33554432")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let policy = Arc::new(permission::PermissionPolicy::from_rules(
            permission::PermissionMode::Default,
            [permission::PermissionRule::allow_tool_session("Read")],
        ).with_roots(permission::FsRoots {
            cwd: root.clone(), home: Some(root.join("home")), lingxi_home: root.join(".lingxi"),
        }));
        lingxi_core::host::read_auto_allow::set_read_auto_allow_probe(Arc::new(
            permission::read_auto_allow::PolicyReadAutoAllow::new(
                policy.clone(), vec!["Read".into(), "Edit".into(), "Write".into()],
            ),
        ));
        let gate = Arc::new(permission::PolicyPermissionGate::new(policy, Arc::new(NoOpPermissionGate)));
        for streaming in [false, true] { for utf16 in [false, true] {
            let temp = tempfile::tempdir_in(&root).unwrap();
            let cwd = temp.path().canonicalize().unwrap();
            let path = cwd.join("file.txt");
            let initial = "alpha\nold line 雪\n";
            let external = "alpha\nUSER-EXTERNAL-未展示\n";
            let replacement = "beta\nold line 雪\n";
            let save_body = |body: &str, seconds| {
                if utf16 {
                    save_exact_utf16(&path, &body.encode_utf16().collect::<Vec<_>>(), seconds);
                } else { save(&path, body, seconds); }
            };
            save_body(initial, 1_000_000);
            let fs: Arc<dyn FileSystem> = Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
            let transcript = cwd.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
            let live = Fixture::with_recording(&cwd, fs.clone(), Arc::new(StaticMemoryProvider::empty()),
                gate.clone(), vec![
                    Step::Tool("Read", serde_json::json!({"file_path":path})), Step::Done,
                    Step::Tool("Edit", serde_json::json!({"file_path":path,"old_string":"alpha","new_string":"beta"})),
                    Step::Tool("Write", serde_json::json!({"file_path":path,"content":replacement})), Step::Done,
                ], |reader| reader,
                Some(Arc::new(session::JsonlWriter::new(transcript.clone(), fs.clone())))).await;
            live.run(streaming).await;
            save_body(external, 2_000_000);
            live.run(streaming).await;
            let requests = live.requests(streaming).await;
            assert_eq!(requests.len(), 5);
            let before_write = serde_json::to_string(&requests[3]).unwrap();
            assert!(before_write.contains("other changes not in your context"));
            assert!(!before_write.contains("USER-EXTERNAL"), "UI sidecars must not enter model history");
            assert!(serde_json::to_string(&requests[4]).unwrap().contains(tool_file::FILE_NOT_READ_ERROR));
            let expected = external.replacen("alpha", "beta", 1);
            let units = tool_file::file_meta::decode_native_text_units(&std::fs::read(&path).unwrap());
            assert_eq!(String::from_utf16_lossy(units.strip_prefix(&[0xfeff]).unwrap_or(&units)), expected);
            assert!(reads::get(&live.map, &path).unwrap().is_partial_view);

            let id = live.orch.current_session_id().await.as_uuid();
            let raw = std::fs::read_to_string(&transcript).unwrap();
            assert!(raw.contains("contentNotInModelContext"));
            let replay = orchestrator::resume::replay_captured_session(
                transcript, raw.clone(), session::jsonl::route_lines(&raw), id).unwrap();
            // Same-instance and new-instance replay must retain the same fence.
            for orch in [&live.orch] {
                orch.restore_captured_session_history(id, &replay.messages,
                    &session::jsonl::route_lines(&raw).messages_in_order, true,
                    permission::PermissionMode::Default, ConversationMode::Code, &raw).await.unwrap();
            }
            assert!(reads::get(&live.map, &path).unwrap().is_partial_view);
            let cold = Fixture::new(&cwd, fs, Arc::new(StaticMemoryProvider::empty()), gate.clone(), vec![
                Step::Tool("Read", serde_json::json!({"file_path":path})),
                Step::Tool("Write", serde_json::json!({"file_path":path,"content":replacement})), Step::Done,
            ]).await;
            cold.orch.restore_captured_session_history(id, &replay.messages,
                &session::jsonl::route_lines(&raw).messages_in_order, true,
                permission::PermissionMode::Default, ConversationMode::Code, &raw).await.unwrap();
            assert!(reads::get(&cold.map, &path).unwrap().is_partial_view);
            let blocked = source(serde_json::json!({"file_path":path,"content":replacement}));
            for fixture in [&live, &cold] {
                let error = fixture.writer.call(blocked.clone(), tool_api::test_support::fresh_ctx(),
                    tool_api::test_support::fresh_tx()).await.unwrap_err();
                assert_eq!(error.model_facing_message().string_at(""), Some(tool_file::FILE_NOT_READ_ERROR));
            }
            // A complete model-dispatched Read, and only then, permits Write.
            cold.run(streaming).await;
            assert!(!reads::get(&cold.map, &path).unwrap().is_partial_view);
            let units = tool_file::file_meta::decode_native_text_units(&std::fs::read(&path).unwrap());
            assert_eq!(String::from_utf16_lossy(units.strip_prefix(&[0xfeff]).unwrap_or(&units)), replacement);
        }}
    });
}

fn save_exact_utf16(path: &Path, units: &[u16], seconds: u64) {
    let bytes = std::iter::once(0xfeffu16)
        .chain(units.iter().copied())
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(path, bytes).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
        )
        .unwrap();
}

fn exact_changed_rows(rows: &[ConversationMessage]) -> Vec<(MessageId, Vec<u16>)> {
    rows.iter()
        .filter(|row| row.text_content().contains(EXACT_CHANGED_NOTE))
        .map(|row| {
            let ConversationMessage::User { content, .. } = row else {
                panic!("meta user notice")
            };
            let units = match &content[0] {
                HistoryBlock::Text { text, .. } => text.encode_utf16().collect(),
                HistoryBlock::TextJsUtf16 {
                    utf16_code_units, ..
                } => utf16_code_units.clone(),
                _ => panic!("text notice"),
            };
            (row.id(), units)
        })
        .collect()
}

struct ExactReadCancellation {
    reader: Arc<tool_file::FileReadTool>,
    armed: Arc<AtomicBool>,
    cancel: tokio_util::sync::CancellationToken,
}

#[async_trait]
impl Tool for ExactReadCancellation {
    fn name(&self) -> &str {
        self.reader.name()
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
    fn read_state_request_path(&self, path: &Path) -> PathBuf {
        self.reader.read_state_request_path(path)
    }
    fn read_state_io_path(&self, path: &Path) -> Result<PathBuf, ToolError> {
        self.reader.read_state_io_path(path)
    }
    async fn open_file_for_compaction(
        &self,
        path: &Path,
    ) -> Result<Option<std::fs::File>, ToolError> {
        self.reader.open_file_for_compaction(path).await
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
        options: &tool_api::DescriptionOptions,
    ) -> String {
        self.reader.description(input, options).await
    }
    async fn prompt(&self, options: &tool_api::PromptOptions) -> Utf16JsonProjection {
        self.reader.prompt(options).await
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let result = self.reader.call(input, ctx, progress).await?;
        if self.armed.swap(false, Ordering::SeqCst) {
            self.cancel.cancel();
        }
        Ok(result)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExactDelivery {
    Normal,
    Cancel,
    Steering,
}

struct ExactSteering {
    path: PathBuf,
    units: Vec<u16>,
    api: Arc<MockApiClient>,
    stream: Arc<MockStreamingApiClient>,
    streaming: bool,
    armed: AtomicBool,
}

#[async_trait]
impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for ExactSteering {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        if !self.armed.load(Ordering::SeqCst) {
            return None;
        }
        let calls = if self.streaming {
            self.stream.captured_calls().await.len()
        } else {
            self.api.captured_msgs().await.len()
        };
        if calls < 3 || !self.armed.swap(false, Ordering::SeqCst) {
            return None;
        }
        save_exact_utf16(&self.path, &self.units, 2_000_000);
        Some("human steering after external UTF16 save".into())
    }
}

async fn exact_file_change_delivery(streaming: bool, delivery: ExactDelivery) {
    use lingxi_core::host::{session_context::ConversationMode, OrchestratorHandle};
    let cases: &[(&[u16], &[u16], usize)] = if delivery == ExactDelivery::Normal {
        &[
            (&[97, 10, 0xd800, 98], &[97, 10, 0xdc00, 98], 1),
            (&[97, 10, 0xd800, 98], &[97, 10, 0xfffd, 98], 1),
            (&[97, 10, 65, 98], &[97, 10, 66, 98], 1),
            (&[97, 10, 0xd800, 98], &[97, 10, 0xd800, 98], 0),
        ]
    } else {
        &[
            (&[97, 10, 0xd800, 98], &[97, 10, 0xdc00, 98], 1),
            (&[97, 10, 0xd800, 98], &[97, 10, 0xfffd, 98], 1),
        ]
    };
    for &(before, after, expected_notices) in cases {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let path = cwd.join("exact-utf16.txt");
        let other = cwd.join("other.txt");
        save_exact_utf16(&path, before, 1_000_000);
        std::fs::write(&other, "continue real tool execution\n").unwrap();
        let transcript = cwd.join(format!("{}.jsonl", uuid::Uuid::new_v4()));
        let fs: Arc<dyn FileSystem> =
            Arc::new(platform_posix::fs::PosixFileSystem::new(cwd.clone()));
        let cancel = tokio_util::sync::CancellationToken::new();
        let armed = Arc::new(AtomicBool::new(false));
        let mut steps = vec![
            Step::Tool("Read", serde_json::json!({"file_path":path})),
            Step::Done,
        ];
        if delivery == ExactDelivery::Steering {
            steps.push(Step::Tool("Read", serde_json::json!({"file_path":other})));
        }
        steps.extend([Step::Done, Step::Done, Step::Done]);
        let fixture = Fixture::with_recording(
            &cwd,
            fs.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            steps,
            |reader| {
                Arc::new(ExactReadCancellation {
                    reader,
                    armed: armed.clone(),
                    cancel: cancel.clone(),
                })
            },
            Some(Arc::new(session::JsonlWriter::new(
                transcript.clone(),
                fs.clone(),
            ))),
        )
        .await;
        // The ordinary model driver invokes the real Read before compaction.
        fixture.run(streaming).await;
        assert_eq!(fixture.requests(streaming).await.len(), 2);
        assert_eq!(
            reads::get_content_units(&fixture.map, &path).as_deref(),
            Some(before)
        );
        for index in 0..8 {
            let row = if index % 2 == 0 {
                ConversationMessage::user(MessageId::new(), format!("prior question {index}"))
            } else {
                ConversationMessage::Assistant {
                    id: MessageId::new(),
                    content: vec![HistoryBlock::Text {
                        text: format!("prior answer {index}"),
                        citations: None,
                    }],
                    stop_reason: Some("end_turn".into()),
                    resumable: None,
                    per_turn_effort: None,
                }
            };
            fixture.orch.append_external_history_message(row).await;
        }
        fixture
            .orch
            .force_compact_with_cancel(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        let baseline = reads::get(&fixture.map, &path).unwrap();
        assert_eq!((baseline.offset, baseline.limit), (None, None));
        assert!(!baseline.is_partial_view);
        assert_eq!(
            reads::get_content_units(&fixture.map, &path).as_deref(),
            Some(before)
        );
        if delivery == ExactDelivery::Steering {
            fixture.orch.set_mid_turn_input(Arc::new(ExactSteering {
                path: path.clone(),
                units: after.to_vec(),
                api: fixture.api.clone(),
                stream: fixture.streaming_api.clone(),
                streaming,
                armed: AtomicBool::new(true),
            }));
        } else {
            save_exact_utf16(&path, after, 2_000_000);
            if expected_notices > 0 {
                let disk = std::fs::read(&path).unwrap();
                let error = fixture
                    .writer
                    .call(
                        source(serde_json::json!({"file_path":path,"content":"overwrite"})),
                        tool_api::test_support::fresh_ctx(),
                        tool_api::test_support::fresh_tx(),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.model_facing_message().string_at(""),
                    Some(tool_file::FILE_UNEXPECTEDLY_MODIFIED_ERROR)
                );
                assert_eq!(
                    std::fs::read(&path).unwrap(),
                    disk,
                    "exact change cannot grant stale Write authority"
                );
            }
        }
        if delivery == ExactDelivery::Cancel {
            armed.store(true, Ordering::SeqCst);
            let prompt = Utf16JsonProjection::from("inspect then cancel before sending");
            if streaming {
                let outcome = OrchestratorHandle::run_turn_streaming_with_cancel(
                    fixture.orch.as_ref(),
                    &prompt,
                    cancel.clone(),
                )
                .await
                .unwrap();
                assert!(matches!(outcome, lingxi_core::host::TurnOutcome::Cancelled));
            } else {
                let outcome = fixture
                    .orch
                    .run_turn_with_cancel(&prompt, cancel.clone())
                    .await
                    .unwrap();
                assert_eq!(outcome, orchestrator::TurnOutcome::Cancelled);
            }
            assert_eq!(
                fixture.requests(streaming).await.len(),
                2,
                "cancelled preparation never dispatches"
            );
        }
        fixture.run(streaming).await;
        fixture.run(streaming).await;
        let requests = fixture.requests(streaming).await;
        let delivered = exact_changed_rows(&requests[requests.len() - 2]);
        assert_eq!(
            delivered.len(),
            expected_notices,
            "streaming={streaming}, delivery={delivery:?}"
        );
        assert_eq!(
            delivered,
            exact_changed_rows(requests.last().unwrap()),
            "one stable notice identity and exact prefix on continuation"
        );
        assert_eq!(
            reads::get_content_units(&fixture.map, &path).as_deref(),
            Some(after)
        );
        if delivery == ExactDelivery::Steering {
            assert!(
                exact_changed_rows(&requests[2]).is_empty(),
                "the file changes during the running turn"
            );
            assert!(requests[3].iter().any(|row| row
                .text_content()
                .contains("human steering after external UTF16 save")));
        }
        if let Some((_, units)) = delivered.first() {
            let exact_snippet = "1\ta\n2\t"
                .encode_utf16()
                .chain(after[2..].iter().copied())
                .collect::<Vec<_>>();
            assert!(
                units
                    .windows(exact_snippet.len())
                    .any(|window| window == exact_snippet),
                "changed snippet must preserve exact units"
            );
        }
        let id = fixture.orch.current_session_id().await.as_uuid();
        let raw = std::fs::read_to_string(&transcript).unwrap();
        let routed = session::jsonl::route_lines(&raw);
        let replayed = orchestrator::resume::replay_captured_session(
            transcript.clone(),
            raw.clone(),
            routed,
            id,
        )
        .unwrap();
        let persisted = replayed
            .messages
            .iter()
            .filter(|row| {
                row.message_type == "attachment"
                    && row
                        .attachment_projection
                        .as_ref()
                        .and_then(|source| source.string_at("/type"))
                        == Some("edited_text_file")
            })
            .collect::<Vec<_>>();
        assert_eq!(
            persisted.len(),
            expected_notices,
            "retained notice is persisted exactly once"
        );
        if let Some(row) = persisted.first() {
            let snippet = row
                .attachment_projection
                .as_ref()
                .unwrap()
                .string_units("/snippet")
                .unwrap();
            assert_eq!(
                snippet,
                "1\ta\n2\t"
                    .encode_utf16()
                    .chain(after[2..].iter().copied())
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                row.extra.string_units("/rendered/0/content"),
                Some(delivered[0].1.clone())
            );
        }
        let runtime = replayed.handle_runtime_snapshot();
        OrchestratorHandle::resume_session(
            fixture.orch.as_ref(),
            lingxi_core::types::SessionId::from_uuid(id),
            replayed.state.history,
            replayed.last_message_uuid.map(|id| id.to_string()),
            None,
            runtime,
        )
        .await
        .unwrap();
        fixture.run(streaming).await;
        assert_eq!(
            exact_changed_rows(fixture.requests(streaming).await.last().unwrap()),
            delivered,
            "hot restore keeps exact notice and dedup"
        );
        let cold = Fixture::new(
            &cwd,
            fs,
            Arc::new(StaticMemoryProvider::empty()),
            Arc::new(NoOpPermissionGate),
            vec![Step::Done],
        )
        .await;
        let routed = session::jsonl::route_lines(&raw);
        cold.orch
            .restore_captured_session_history(
                id,
                &replayed.messages,
                &routed.messages_in_order,
                true,
                permission::PermissionMode::Default,
                ConversationMode::Code,
                &raw,
            )
            .await
            .unwrap();
        cold.run(streaming).await;
        assert_eq!(
            exact_changed_rows(&cold.requests(streaming).await[0]),
            delivered,
            "cold restore matches the exact hot request prefix"
        );
    }
}

#[tokio::test]
async fn batched_exact_utf16_changes_keep_notices_and_recovery() {
    exact_file_change_delivery(false, ExactDelivery::Normal).await;
}
#[tokio::test]
async fn streaming_exact_utf16_changes_keep_notices_and_recovery() {
    exact_file_change_delivery(true, ExactDelivery::Normal).await;
}
#[tokio::test]
async fn batched_cancelled_exact_utf16_changes_survive_continue() {
    exact_file_change_delivery(false, ExactDelivery::Cancel).await;
}
#[tokio::test]
async fn streaming_cancelled_exact_utf16_changes_survive_continue() {
    exact_file_change_delivery(true, ExactDelivery::Cancel).await;
}
#[tokio::test]
async fn batched_mid_turn_exact_utf16_changes_keep_steering_and_notice() {
    exact_file_change_delivery(false, ExactDelivery::Steering).await;
}
#[tokio::test]
async fn streaming_mid_turn_exact_utf16_changes_keep_steering_and_notice() {
    exact_file_change_delivery(true, ExactDelivery::Steering).await;
}
