//! Changed-file attachments must survive as history, without being replayed at
//! each tool continuation. Later edits must still be detected in the same turn.

use async_trait::async_trait;
use lingxi_core::host::{FileSystem, OrchestratorHandle};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ConversationMessage, MessageId, SessionId};
use llm_runtime::{ContentBlock, HistoryEvent, HistoryResponse};
use orchestrator::test_support::{
    content_block_start_text, content_block_start_tool_use, content_block_stop, input_json_delta,
    message_delta_stop, message_start, message_stop, mock_message_response, noop_hook_executor,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use platform_posix::fs::PosixFileSystem;
use session::jsonl::{reader::JsonlReader, writer::JsonlWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    generated_builtin_result, generated_builtin_schema, DescriptionOptions, PromptOptions, Tool,
    ToolInputRepairMetadata, ToolInputRepairSchema, ToolInputSchemaProvenance, ToolStaticContext,
};
use tool_api::{ToolCallResult, ToolError};

const ORIGINAL: &str = "original contents\n";
const FIRST_EDIT: &str = "first edit before the turn\n";
const MID_TURN_EDIT: &str = "second edit during tool execution\n";
const NEXT_TURN_EDIT: &str = "third edit before the next turn\n";
const CHANGED_FILE_NOTE: &str = "changed on disk since you last read it.";

// Advance the mtime deterministically, including on filesystems with coarse
// timestamp resolution. No sleeps or modifications to the Read cache are needed.
fn edit_file(path: &Path, contents: &str, seconds: u64) {
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

struct ContinueTool {
    paths: Vec<PathBuf>,
    change_on_call: usize,
    calls: AtomicUsize,
    schema: Utf16JsonProjection,
}

#[async_trait]
impl Tool for ContinueTool {
    fn name(&self) -> &str {
        "ContinueFixture"
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        &self.schema
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
    async fn check_permissions(
        &self,
        _: &Utf16JsonProjection,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "isolated test fixture".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &Utf16JsonProjection, _: &DescriptionOptions) -> String {
        "Continue the test fixture".into()
    }
    async fn prompt(&self, _: &PromptOptions) -> Utf16JsonProjection {
        "Continue the test fixture".into()
    }
    async fn call(
        &self,
        _: Utf16JsonProjection,
        _: ToolUseContext,
        _: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == self.change_on_call {
            for path in &self.paths {
                edit_file(path, MID_TURN_EDIT, 2_000);
            }
        }
        Ok(ToolCallResult::from_data(generated_builtin_result(
            &serde_json::json!({"ok": true}),
        )))
    }
}

fn changed_rows(messages: &[ConversationMessage]) -> Vec<(usize, MessageId, String)> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            let text = message.text_content();
            text.contains(CHANGED_FILE_NOTE)
                .then(|| (index, message.id(), text))
        })
        .collect()
}

async fn run_turn(orch: &ConversationOrchestrator, streaming: bool, prompt: &str) {
    let outcome = if streaming {
        orch.run_turn_streaming(prompt).await
    } else {
        orch.run_turn(prompt).await
    }
    .expect("scripted conversation succeeds");
    assert!(matches!(outcome, ConversationOutcome::EndTurn { .. }));
}

fn model_responses(tool_calls: usize, end_turns: usize) -> Vec<HistoryResponse> {
    (0..tool_calls + end_turns)
        .map(|step| {
            let mut response = if step < tool_calls {
                mock_message_response(
                    vec![ContentBlock::ToolCall {
                        id: format!("call_continue_{step}"),
                        name: "ContinueFixture".into(),
                        input: lingxi_llm_client::protocol::JsonPayload::parse("{}").unwrap(),
                    }],
                    Some("tool_use"),
                )
            } else {
                mock_message_response(
                    vec![ContentBlock::Text {
                        text: "done".into(),
                        cache_control: None,
                        citations: None,
                    }],
                    Some("end_turn"),
                )
            };
            response.model = "gpt-6.1-sol".into();
            response
        })
        .collect()
}

fn stream_turns(responses: Vec<HistoryResponse>) -> Vec<Vec<HistoryEvent>> {
    responses
        .into_iter()
        .map(|response| {
            let mut events = vec![message_start(&response.id, &response.model)];
            match &response.content[0] {
                ContentBlock::ToolCall { id, name, .. } => {
                    events.extend([
                        content_block_start_tool_use(0, id.clone().into(), name),
                        input_json_delta(0, "{"),
                        input_json_delta(0, "}"),
                    ]);
                }
                ContentBlock::Text { text, .. } => {
                    events.extend([content_block_start_text(0), text_delta(0, text)]);
                }
                _ => unreachable!("fixture emits only tool calls and text"),
            }
            events.extend([
                content_block_stop(0),
                message_delta_stop(response.stop_reason.as_deref().unwrap()),
                message_stop(),
            ]);
            events
        })
        .collect()
}

struct SteeringSource {
    tool: Arc<ContinueTool>,
    delivered: AtomicBool,
}

#[async_trait]
impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for SteeringSource {
    async fn take_mid_turn_input(&self) -> Option<Utf16JsonProjection> {
        (self.tool.calls.load(Ordering::SeqCst) > 0 && !self.delivered.swap(true, Ordering::SeqCst))
            .then(|| "human steering between tool steps".into())
    }
}

async fn check_changed_file_continuations(
    streaming: bool,
    initially_changed: bool,
    steering: bool,
) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let path = cwd.join("watched.txt");
    let mid_turn_path = cwd.join("changed-during-turn.txt");
    let next_turn_path = cwd.join("changed-before-next-turn.txt");
    edit_file(
        &path,
        if initially_changed {
            FIRST_EDIT
        } else {
            ORIGINAL
        },
        1_000,
    );
    edit_file(&mid_turn_path, ORIGINAL, 1_000);
    edit_file(&next_turn_path, ORIGINAL, 1_000);
    let state = tool_api::read_file_state::new_read_file_state_map();
    // Each file starts as a full model-visible snapshot. Only the first file
    // may be stale at ingress; the others change at later collection points.
    for watched in [&path, &mid_turn_path, &next_turn_path] {
        tool_api::read_file_state::set_with_requested_path(
            &state,
            watched.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: ORIGINAL.into(),
                mtime_ms: if initially_changed && watched == &path {
                    0
                } else {
                    1_000_000
                },
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
            true,
            Some(watched.clone()),
        );
    }
    let tool = Arc::new(ContinueTool {
        paths: vec![mid_turn_path.clone()],
        change_on_call: if steering { 0 } else { 1 },
        calls: AtomicUsize::new(0),
        schema: generated_builtin_schema(&serde_json::json!({"type": "object"})),
    });
    let mut tools = ToolRegistry::new();
    tools.register_builtin(tool.clone());
    let mut read_context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    read_context.read_file_state = state.clone();
    // Use the production Read implementation: its state refresh is what makes
    // a subsequent unchanged file quiet, independently of ingress caching.
    tools.register_builtin(Arc::new(tool_file::FileReadTool::new(read_context)));

    let responses = model_responses(4, 3);
    let batched_api = Arc::new(MockApiClient::new(responses.clone()));
    let streaming_api = Arc::new(MockStreamingApiClient::with_turns(stream_turns(responses)));
    let session_id = SessionId::new();
    let transcript = session::jsonl::session_path(
        &cwd,
        &cwd.to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    let filesystem: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "gpt-6.1-sol".into(),
                max_turns: 8,
                ..OrchestratorConfig::default()
            },
            batched_api.clone(),
            streaming_api.clone(),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_read_state_map(state.clone())
        .with_mid_turn_input(Arc::new(SteeringSource {
            tool: tool.clone(),
            delivered: AtomicBool::new(!steering),
        }))
        .with_config_home(cwd.join("config"))
        .with_session_id(session_id)
        .with_jsonl_writer(Arc::new(JsonlWriter::new(
            transcript.clone(),
            filesystem.clone(),
        ))),
    );
    orch.initialize_conversation_mode().await.unwrap();
    run_turn(&orch, streaming, "inspect and continue").await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 4);
    run_turn(&orch, streaming, "continue without editing").await;
    edit_file(&next_turn_path, NEXT_TURN_EDIT, 3_000);
    run_turn(&orch, streaming, "inspect the new edit").await;

    let requests: Vec<_> = if streaming {
        assert!(
            batched_api.captured_msgs().await.is_empty(),
            "no batched fallback"
        );
        streaming_api
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        assert!(streaming_api.captured_calls().await.is_empty());
        batched_api.captured_msgs().await
    };
    assert_eq!(requests.len(), 7);
    let initial = usize::from(initially_changed);
    let reminders: Vec<_> = requests
        .iter()
        .map(|messages| changed_rows(messages))
        .collect();
    assert_eq!(
        reminders.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![
            initial,
            initial + usize::from(steering),
            initial + 1,
            initial + 1,
            initial + 1,
            initial + 1,
            initial + 2
        ],
        "only actual edits add reminders, including after an initially empty ingress result"
    );
    assert_eq!(
        tool_api::read_file_state::get(&state, &mid_turn_path)
            .unwrap()
            .content,
        MID_TURN_EDIT
    );
    // An existing reminder retains its identity, contents and position on all
    // later requests. Appending a replay at the tail would break this prefix.
    for (step, pair) in reminders.windows(2).enumerate() {
        assert_eq!(
            &pair[1][..pair[0].len()],
            pair[0].as_slice(),
            "existing reminders must retain their order and positions on request {}",
            step + 1
        );
    }
    if initially_changed {
        assert!(reminders[0][0].2.contains(FIRST_EDIT.trim()));
    }
    assert!(reminders[2][initial].2.contains(MID_TURN_EDIT.trim()));
    assert!(reminders[6][initial + 1].2.contains(NEXT_TURN_EDIT.trim()));
    let final_ids: Vec<_> = reminders[6].iter().map(|row| row.1).collect();
    assert_eq!(
        final_ids
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        initial + 2
    );
    assert_eq!(
        changed_rows(&orch.snapshot_history().await)
            .iter()
            .map(|row| row.1)
            .collect::<Vec<_>>(),
        final_ids,
        "durable history has one copy per actual edit"
    );
    let rows = JsonlReader::new(transcript, filesystem)
        .read_all()
        .await
        .unwrap();
    let persisted: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row
                    .attachment_projection
                    .as_ref()
                    .and_then(|payload| payload.string_at("/type"))
                    == Some("edited_text_file")
        })
        .collect();
    assert_eq!(persisted.len(), initial + 2, "JSONL appends each edit once");
    assert_eq!(
        persisted
            .iter()
            .map(|row| row.uuid.clone())
            .collect::<Vec<_>>(),
        final_ids
            .iter()
            .map(|id| id.as_uuid().to_string())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn batched_changed_file_is_not_replayed_and_later_edits_are_reported() {
    check_changed_file_continuations(false, true, false).await;
}

#[tokio::test]
async fn streaming_changed_file_is_not_replayed_and_later_edits_are_reported() {
    check_changed_file_continuations(true, true, false).await;
}

#[tokio::test]
async fn batched_initially_unchanged_file_still_reports_a_mid_turn_edit() {
    check_changed_file_continuations(false, false, false).await;
}

#[tokio::test]
async fn streaming_initially_unchanged_file_still_reports_a_mid_turn_edit() {
    check_changed_file_continuations(true, false, false).await;
}

#[tokio::test]
async fn batched_steering_checks_new_edits_after_an_initial_reminder() {
    check_changed_file_continuations(false, true, true).await;
}

#[tokio::test]
async fn streaming_steering_checks_new_edits_after_an_initial_reminder() {
    check_changed_file_continuations(true, true, true).await;
}

#[tokio::test]
async fn batched_steering_checks_new_edits_after_empty_ingress() {
    check_changed_file_continuations(false, false, true).await;
}

#[tokio::test]
async fn streaming_steering_checks_new_edits_after_empty_ingress() {
    check_changed_file_continuations(true, false, true).await;
}

struct CancelAfterRead {
    inner: tool_file::FileReadTool,
    cancel: tokio_util::sync::CancellationToken,
    reads: AtomicUsize,
    cancel_after_reads: usize,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl Tool for CancelAfterRead {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn input_schema(&self) -> &Utf16JsonProjection {
        self.inner.input_schema()
    }
    fn input_repair_metadata(&self) -> ToolInputRepairMetadata {
        self.inner.input_repair_metadata()
    }
    fn is_enabled(&self, ctx: &ToolStaticContext) -> bool {
        self.inner.is_enabled(ctx)
    }
    fn max_result_size_chars(&self) -> usize {
        self.inner.max_result_size_chars()
    }
    fn is_concurrency_safe(&self, input: &Utf16JsonProjection) -> bool {
        self.inner.is_concurrency_safe(input)
    }
    fn is_read_only(&self, input: &Utf16JsonProjection) -> bool {
        self.inner.is_read_only(input)
    }
    async fn check_permissions(
        &self,
        input: &Utf16JsonProjection,
        ctx: &ToolUseContext,
    ) -> permission::PermissionResult {
        self.inner.check_permissions(input, ctx).await
    }
    async fn description(
        &self,
        input: &Utf16JsonProjection,
        options: &DescriptionOptions,
    ) -> String {
        self.inner.description(input, options).await
    }
    async fn prompt(&self, options: &PromptOptions) -> Utf16JsonProjection {
        self.inner.prompt(options).await
    }
    async fn call(
        &self,
        input: Utf16JsonProjection,
        ctx: ToolUseContext,
        progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let result = self.inner.call(input, ctx, progress).await?;
        if self.reads.fetch_add(1, Ordering::SeqCst) + 1 == self.cancel_after_reads {
            // The completed Reads have published their state, but no notice
            // has been admitted yet. Yield once with cancellation already set
            // so both driver paths exercise this exact publication window.
            self.cancel.cancel();
            self.entered.notify_one();
            self.release.notified().await;
        }
        Ok(result)
    }
}

async fn check_cancelled_changed_files_survive(
    streaming: bool,
    initial: bool,
    completed_reads: usize,
) {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let paths = [cwd.join("first.txt"), cwd.join("second.txt")];
    let state = tool_api::read_file_state::new_read_file_state_map();
    for path in &paths {
        edit_file(
            path,
            if initial { MID_TURN_EDIT } else { ORIGINAL },
            if initial { 2_000 } else { 1_000 },
        );
        // A post-Edit/Write full snapshot is eligible for change detection.
        tool_api::read_file_state::set_with_requested_path(
            &state,
            path.clone(),
            tool_api::read_file_state::ReadFileEntry {
                content: ORIGINAL.into(),
                mtime_ms: 1_000_000,
                offset: None,
                limit: None,
                from_read: false,
                seeded_from_context: false,
                is_partial_view: false,
            },
            true,
            Some(path.clone()),
        );
    }
    let cancel = tokio_util::sync::CancellationToken::new();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut read_context = tool_api::test_support::ctx_for_file_tools(
        tool_api::test_support::make_dummy_fs(),
        Arc::new(telemetry::AnalyticsBus::new()),
        vec![cwd.clone()],
    );
    read_context.read_file_state = state.clone();
    let read = Arc::new(CancelAfterRead {
        inner: tool_file::FileReadTool::new(read_context),
        cancel: cancel.clone(),
        reads: AtomicUsize::new(0),
        cancel_after_reads: completed_reads,
        entered: entered.clone(),
        release: release.clone(),
    });
    let mut tools = ToolRegistry::new();
    tools.register_builtin(read.clone());
    tools.register_builtin(Arc::new(ContinueTool {
        paths: paths.to_vec(),
        change_on_call: 0,
        calls: AtomicUsize::new(0),
        schema: generated_builtin_schema(&serde_json::json!({"type": "object"})),
    }));
    let responses = model_responses(usize::from(!initial), 2);
    let batched_api = Arc::new(MockApiClient::new(responses.clone()));
    let streaming_api = Arc::new(MockStreamingApiClient::with_turns(stream_turns(responses)));
    let session_id = SessionId::new();
    let transcript = session::jsonl::session_path(
        &cwd,
        &cwd.to_string_lossy(),
        &session_id.as_uuid().to_string(),
    );
    let filesystem: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(cwd.clone()));
    let orch = ConversationOrchestrator::into_shared(
        ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig {
                model: "gpt-6.1-sol".into(),
                max_turns: 5,
                ..Default::default()
            },
            batched_api.clone(),
            streaming_api.clone(),
            Arc::new(tools),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            cwd.clone(),
        )
        .with_read_state_map(state.clone())
        .with_config_home(cwd.join("config"))
        .with_session_id(session_id)
        .with_jsonl_writer(Arc::new(JsonlWriter::new(
            transcript.clone(),
            filesystem.clone(),
        ))),
    );
    orch.initialize_conversation_mode().await.unwrap();
    let turn = tokio::spawn({
        let orch = orch.clone();
        async move {
            let prompt = Utf16JsonProjection::from("inspect and continue");
            if streaming {
                let outcome = OrchestratorHandle::run_turn_streaming_with_cancel(
                    orch.as_ref(),
                    &prompt,
                    cancel,
                )
                .await
                .unwrap();
                assert!(matches!(outcome, lingxi_core::host::TurnOutcome::Cancelled));
            } else {
                let outcome = orch.run_turn_with_cancel(&prompt, cancel).await.unwrap();
                assert_eq!(outcome, orchestrator::TurnOutcome::Cancelled);
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("preparation must reach the real Read publication boundary");
    assert_eq!(read.reads.load(Ordering::SeqCst), completed_reads);
    assert_eq!(
        paths
            .iter()
            .filter(|path| {
                tool_api::read_file_state::get(&state, path)
                    .unwrap()
                    .content
                    == MID_TURN_EDIT
            })
            .count(),
        completed_reads
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), turn)
        .await
        .expect("cancellation must finish after the publication barrier is released")
        .unwrap();
    if !initial {
        assert_eq!(
            changed_rows(&orch.snapshot_history().await).len(),
            completed_reads,
            "cancelling must retain every successfully refreshed file notice"
        );
    }
    run_turn(&orch, streaming, "continue with the changed files").await;
    run_turn(&orch, streaming, "continue without further edits").await;
    assert_eq!(
        read.reads.load(Ordering::SeqCst),
        2,
        "every changed file is read only once"
    );
    let requests: Vec<_> = if streaming {
        assert!(batched_api.captured_msgs().await.is_empty());
        streaming_api
            .captured_calls()
            .await
            .into_iter()
            .map(|call| call.messages)
            .collect()
    } else {
        assert!(streaming_api.captured_calls().await.is_empty());
        batched_api.captured_msgs().await
    };
    assert_eq!(
        requests
            .iter()
            .map(|rows| changed_rows(rows).len())
            .collect::<Vec<_>>(),
        if initial { vec![2, 2] } else { vec![0, 2, 2] },
        "cancelled preparation must not dispatch; resume sees each notice once"
    );
    let first_resumed_request = usize::from(!initial);
    assert_eq!(
        changed_rows(&requests[first_resumed_request]),
        changed_rows(&requests[first_resumed_request + 1])
    );
    let ids: std::collections::HashSet<_> = changed_rows(&requests[first_resumed_request])
        .into_iter()
        .map(|row| row.1.as_uuid().to_string())
        .collect();
    assert_eq!(ids.len(), 2);
    let rows = JsonlReader::new(transcript, filesystem)
        .read_all()
        .await
        .unwrap();
    let persisted: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.message_type == "attachment"
                && row
                    .attachment_projection
                    .as_ref()
                    .and_then(|payload| payload.string_at("/type"))
                    == Some("edited_text_file")
        })
        .collect();
    assert_eq!(
        persisted.len(),
        2,
        "each retained notice is written to JSONL once"
    );
    assert_eq!(
        persisted
            .iter()
            .map(|row| row.uuid.clone())
            .collect::<std::collections::HashSet<_>>(),
        ids
    );
}

#[tokio::test]
async fn batched_cancel_after_read_retains_notices_for_continue() {
    check_cancelled_changed_files_survive(false, false, 2).await;
}

#[tokio::test]
async fn streaming_cancel_after_read_retains_notices_for_continue() {
    check_cancelled_changed_files_survive(true, false, 2).await;
}

#[tokio::test]
async fn batched_initial_cancel_after_read_retains_notices_for_continue() {
    check_cancelled_changed_files_survive(false, true, 2).await;
}

#[tokio::test]
async fn streaming_initial_cancel_after_read_retains_notices_for_continue() {
    check_cancelled_changed_files_survive(true, true, 2).await;
}

#[tokio::test]
async fn batched_initial_cancel_between_files_retains_both_notices_for_continue() {
    check_cancelled_changed_files_survive(false, true, 1).await;
}

#[tokio::test]
async fn streaming_initial_cancel_between_files_retains_both_notices_for_continue() {
    check_cancelled_changed_files_survive(true, true, 1).await;
}
