//! §3.6, the file-history epilogue: who runs it, and who deliberately does not.
//!
//! The epilogue lives in the streaming `run()` alone — `make_snapshot` at turn
//! start, `append_file_history_snapshot` AFTER the loop. `try_run_turn` and
//! `try_run_turn_cancelable` have no equivalent, and §3.6 says so outright:
//! "PR 5 不得给 batched print/REPL 新增 file-history，也不得让 streaming 的
//! `ReturnDirect` 突然获得收尾." §8 lists "给 batched 误加 file-history" as its
//! own risk row.
//!
//! Three properties, and the second and third are the ones PR 5 can break by
//! being tidy:
//!
//!   * streaming's normal completion persists a snapshot;
//!   * batched persists none — it has no epilogue at all;
//!   * streaming's `Return(...)` arm returns from INSIDE the loop, so it skips
//!     the epilogue that sits after it. EndConversation takes that arm.
//!
//! Observed at the real output — the transcript file — rather than through a
//! mock writer, because "was a snapshot persisted" is a question about the
//! JSONL, and the line carries its own marker (`"type":"file-history-snapshot"`).

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, text_delta, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use platform_api::FileSystem;
use platform_posix::fs::PosixFileSystem;
use session::jsonl::JsonlWriter;
use std::future::Future;
use std::sync::Arc;
use tempfile::tempdir;
use tool_api::registry::ToolRegistry;

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("turn-epilogue-boundary".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(build());
        })
        .expect("spawn");
    handle.join().expect("test thread panicked");
}

/// Everything a turn needs to be able to persist a file-history snapshot: a
/// transcript to write to, and a `FileHistory` to snapshot.
struct Fixture {
    dir: tempfile::TempDir,
    session_path: std::path::PathBuf,
    writer: Arc<JsonlWriter>,
    file_history: Arc<session::FileHistory>,
}

fn fixture() -> Fixture {
    let dir = tempdir().expect("tempdir");
    let session_path = dir.path().join("session.jsonl");
    let fs: Arc<dyn FileSystem> = Arc::new(PosixFileSystem::new(dir.path().to_path_buf()));
    let writer = Arc::new(JsonlWriter::new(session_path.clone(), fs));
    let file_history = Arc::new(session::FileHistory::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "epilogue-test".to_string(),
    ));
    Fixture {
        dir,
        session_path,
        writer,
        file_history,
    }
}

/// How many `file-history-snapshot` lines the transcript carries.
/// A registry with one inert tool, so a streamed tool round can dispatch.
fn epilogue_tool_registry() -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    r.register_builtin(Arc::new(NoopTool));
    Arc::new(r)
}

/// A tool that cancels the turn's token, so the NEXT loop-top guard sees it.
///
/// This is how §3.2's loop-top row is reached: the round completes, the
/// disposition says continue, and the guard at the top of the next iteration
/// finds the token already set.
struct CancellingTool(tokio_util::sync::CancellationToken);

#[async_trait::async_trait]
impl tool_api::tool_trait::Tool for CancellingTool {
    fn name(&self) -> &str {
        "CancelsTurn"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), tool_api::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(
        &self,
        _input: &serde_json::Value,
        _opts: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &tool_api::tool_trait::PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        self.0.cancel();
        Ok(tool_api::ToolCallResult {
            data: serde_json::json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

struct NoopTool;

#[async_trait::async_trait]
impl tool_api::tool_trait::Tool for NoopTool {
    fn name(&self) -> &str {
        "Noop"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &tool_api::tool_trait::ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    async fn validate_input(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> Result<(), tool_api::ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(
        &self,
        _input: &serde_json::Value,
        _opts: &tool_api::tool_trait::DescriptionOptions,
    ) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &tool_api::tool_trait::PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: tool_api::progress::ToolProgressSender,
    ) -> Result<tool_api::ToolCallResult, tool_api::ToolError> {
        Ok(tool_api::ToolCallResult {
            data: serde_json::json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: None,
        })
    }
}

fn snapshot_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("type")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                })
                .as_deref()
                == Some("file-history-snapshot")
        })
        .count()
}

/// A streamed round that CALLS A TOOL.
///
/// The EndConversation arm lives in the tool-round disposition, so a text-only
/// `end_turn` never reaches it — the turn ends through the natural `Complete`
/// arm instead, which does run the epilogue. The first draft of the Return test
/// scripted text and failed on exactly that.
fn streamed_tool_round(id: &str, tool: &str) -> Vec<llm_runtime::HistoryEvent> {
    vec![
        message_start(id, "claude-opus-4-7"),
        orchestrator::test_support_stream::content_block_start_tool_use(
            0,
            protocol::ToolUseId::new(),
            tool,
        ),
        orchestrator::test_support::input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ]
}

fn streamed_end_turn(id: &str) -> Vec<llm_runtime::HistoryEvent> {
    scripted![
        message_start(id, "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "answer"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

/// STREAMING persists the snapshot after a normal completion.
///
/// This is the control the other two tests are measured against: without it,
/// "batched wrote none" and "the Return arm wrote none" could both be true
/// because nothing in the fixture can write one at all.
#[test]
fn streaming_persists_a_file_history_snapshot_after_a_normal_turn() {
    run_with_large_stack(|| async {
        let f = fixture();
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![streamed_end_turn(
                "m1",
            )])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            f.dir.path().to_path_buf(),
        )
        .with_jsonl_writer(f.writer.clone())
        .with_file_history(f.file_history.clone());

        orch.run_turn_streaming("ping").await.expect("streaming");

        assert_eq!(
            snapshot_lines(&f.session_path),
            1,
            "the streaming epilogue must persist exactly one file-history snapshot per turn; \
             /rewind and --resume rebuild their index from these lines"
        );
    });
}

/// BATCHED persists none — it has no epilogue.
///
/// Same wiring, same opportunity, different driver. §3.6 records the absence as
/// a fact about today's code, and §8 lists "giving batched file-history" as a
/// risk of PR 5 rather than a fix: `run_turn` backs print and the stdio REPL,
/// and handing them a restore point they never had changes what those
/// transcripts contain.
#[test]
fn the_batched_entries_persist_no_file_history_snapshot() {
    run_with_large_stack(|| async {
        for (label, cancelable) in [("run_turn", false), ("run_turn_with_cancel", true)] {
            let f = fixture();
            let orch = ConversationOrchestrator::new(
                OrchestratorConfig::default(),
                Arc::new(MockApiClient::new(vec![mock_message_response(
                    vec![LlmContentBlock::Text {
                        text: "answer".into(),
                        cache_control: None,
                    }],
                    Some("end_turn"),
                )])),
                Arc::new(ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                f.dir.path().to_path_buf(),
            )
            .with_jsonl_writer(f.writer.clone())
            .with_file_history(f.file_history.clone());

            if cancelable {
                orch.run_turn_with_cancel("ping", tokio_util::sync::CancellationToken::new())
                    .await
                    .expect(label);
            } else {
                orch.run_turn("ping").await.expect(label);
            }

            assert_eq!(
                snapshot_lines(&f.session_path),
                0,
                "{label} must persist NO file-history snapshot. The epilogue lives in the \
                 streaming run() alone; a shared one that every entry reaches would add \
                 restore points to print and the stdio REPL, which have never had them."
            );
        }
    });
}

/// STREAMING's `Return(...)` arm skips the epilogue.
///
/// `EndConversation` returns from INSIDE the loop, and the epilogue sits after
/// it — so a turn that ends that way persists no snapshot even though the same
/// driver persists one on a normal end. §4 spells this out as "ReturnDirect,
/// 跳过 epilogue", and §3.6 forbids letting it "suddenly acquire" one.
///
/// This is the row a unified driver is most likely to erase: moving the
/// epilogue into a shared exit path gives it to every terminal, including the
/// ones that return early on purpose.
#[test]
fn the_streaming_return_arm_skips_the_epilogue() {
    run_with_large_stack(|| async {
        let f = fixture();
        let end_slot: Arc<std::sync::atomic::AtomicBool> =
            Arc::new(std::sync::atomic::AtomicBool::new(true));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![
                streamed_tool_round("m1", "Noop"),
                streamed_end_turn("m2"),
            ])),
            epilogue_tool_registry(),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            f.dir.path().to_path_buf(),
        )
        .with_jsonl_writer(f.writer.clone())
        .with_file_history(f.file_history.clone())
        .with_end_conversation_slot(end_slot.clone());

        orch.run_turn_streaming("ping").await.expect("streaming");

        assert_eq!(
            snapshot_lines(&f.session_path),
            0,
            "a turn that ends through the Return arm must persist NO snapshot: it returns from \
             inside the loop and the epilogue is after it. Hoisting the epilogue into a shared \
             exit hands it to terminals that return early on purpose."
        );
    });
}

/// A mid-turn input source that cancels the turn when the loop drains it.
///
/// Streaming's guard order is drain -> max_turns -> budget -> increment ->
/// cancel, so cancelling from inside the drain lands the token exactly where the
/// loop-top guard reads it — between rounds, not during one. That is the
/// difference between §3.2 row 5 and row 6.
struct CancelsOnDrain(tokio_util::sync::CancellationToken);

#[async_trait::async_trait]
impl orchestrator::prompt::mid_turn_input::MidTurnInputSource for CancelsOnDrain {
    async fn take_mid_turn_input(&self) -> Option<String> {
        self.0.cancel();
        None
    }
}

/// §3.2 row 5 — the LOOP-TOP guard.
///
/// The cancel lands during the top-of-loop drain, so the guard a few lines later
/// catches it before any model call. It emits `aborted_streaming` from the
/// driver's own literal emit — the site the row-6 test does NOT reach — and runs
/// the epilogue.
///
/// Distinct from the ENTRY pre-cancel: that check lives in
/// `run_turn_streaming_inputs_locked`, before the driver is entered at all, and
/// emits nothing. Here the driver has been entered and its loop is running.
#[test]
fn the_loop_top_guard_emits_aborted_streaming_and_runs_the_epilogue() {
    run_with_large_stack(|| async {
        let f = fixture();
        let token = tokio_util::sync::CancellationToken::new();
        let out = Arc::new(MockOutputStream::new());
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![streamed_end_turn(
                "m1",
            )])),
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            out.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            f.dir.path().to_path_buf(),
        )
        .with_jsonl_writer(f.writer.clone())
        .with_file_history(f.file_history.clone());
        orch.set_mid_turn_input(Arc::new(CancelsOnDrain(token.clone())));

        orch.run_turn_streaming_with_cancel("ping", token.clone())
            .await
            .expect("streaming");

        let ends: Vec<String> = out
            .snapshot()
            .await
            .iter()
            .filter_map(|e| match e {
                platform_api::OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect();

        assert!(
            token.is_cancelled(),
            "the drain must have cancelled the token"
        );
        assert_eq!(
            ends,
            vec!["aborted_streaming".to_string()],
            "the loop-top guard emits aborted_streaming. Unlike the entry pre-cancel, the \
             driver HAS been entered; unlike the post-drive abort, no round was driven."
        );
        assert_eq!(
            snapshot_lines(&f.session_path),
            1,
            "and it breaks rather than returning, so the epilogue after the loop runs. §3.2 \
             gives this row 执行 in the epilogue column."
        );
    });
}

/// §3.2 row 6 — the POST-DRIVE stream/tool abort — and the mirror of
/// `a_pre_cancelled_streaming_turn_emits_no_end_event`.
///
/// The tool cancels the token during its own execution, so the cancel is caught
/// after the round is driven rather than at the next loop top. `aborted_during_
/// stream` is true there, so the reason is `aborted_streaming` rather than
/// `aborted_tools`, and the epilogue RUNS.
///
/// Against the entry pre-cancel, which emits nothing and runs nothing, these are
/// opposite side effects behind the same `Cancelled` outcome. With both pinned,
/// a shared exit cannot flatten them without turning one red.
///
/// NOT this test: §3.2 row 5, the loop-top guard. That one needs a cancel
/// landing BETWEEN rounds rather than during one, and it emits from a different
/// site (`drivers/mod.rs`'s literal `emit_end_turn("aborted_streaming")`, which
/// this test does not reach — mutating it leaves this test green, which is how
/// the mislabel was found). It remains uncovered.
#[test]
fn the_post_drive_abort_emits_aborted_streaming_and_runs_the_epilogue() {
    run_with_large_stack(|| async {
        let f = fixture();
        let token = tokio_util::sync::CancellationToken::new();
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(CancellingTool(token.clone())));
        let out = Arc::new(MockOutputStream::new());

        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![
                streamed_tool_round("m1", "CancelsTurn"),
                streamed_end_turn("m2"),
            ])),
            Arc::new(registry),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            out.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            f.dir.path().to_path_buf(),
        )
        .with_jsonl_writer(f.writer.clone())
        .with_file_history(f.file_history.clone());

        orch.run_turn_streaming_with_cancel("ping", token.clone())
            .await
            .expect("streaming");

        let ends: Vec<String> = out
            .snapshot()
            .await
            .iter()
            .filter_map(|e| match e {
                platform_api::OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
                _ => None,
            })
            .collect();

        assert!(
            token.is_cancelled(),
            "the tool must have cancelled the token, or this exercises the natural end instead"
        );
        assert_eq!(
            ends,
            vec!["aborted_streaming".to_string()],
            "the post-drive abort emits aborted_STREAMING when the cancel landed during the \
             stream. Pinned exactly rather than as any aborted_*: aborted_TOOLS is the same \
             row's other branch (cancel during tool execution) and a guard accepting either \
             could not tell them apart. The entry pre-cancel emits neither."
        );
        assert_eq!(
            snapshot_lines(&f.session_path),
            1,
            "and it must RUN the epilogue, unlike the pre-cancel and unlike the Return arm. \
             §3.2 gives this row 执行 in the epilogue column; the cancel-ish terminals have \
             different answers and a shared exit owes each of them."
        );
    });
}
