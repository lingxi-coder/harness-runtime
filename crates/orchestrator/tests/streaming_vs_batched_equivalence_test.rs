//! streaming (M5-04 Task 17).
//!
//! Asserts:
//!   - both paths produce `ConversationOutcome::EndTurn { turn_count: 1, .. }`
//!   - both paths append identical assistant message bodies to the session.
use lingxi_core::types::{ContentBlock, ConversationMessage};
use llm_runtime::{ContentBlock as LlmContentBlock, ExecutionUsage as Usage, HistoryResponse};
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockApiClient, MockOutputStream, MockStreamingApiClient, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, ConversationOutcome, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, Tool, ToolStaticContext};
use tool_api::{registry::ToolRegistry, ToolCallResult, ToolError, ValidationError};

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("orchestrator-equivalence-test".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime");
            runtime.block_on(build());
        })
        .expect("spawn large-stack test thread");
    handle.join().expect("large-stack test thread panicked");
}

/// The model every fixture in this file speaks as. It must stay on the
/// `todo_tools_gate` allowlist, or the task reminder stops firing.
const FIXTURE_MODEL: &str = "claude-opus-4-7";

fn batched_response(text: &str) -> HistoryResponse {
    HistoryResponse {
        id: "msg_eq".into(),
        model: "claude-opus-4-7".into(),
        content: vec![LlmContentBlock::Text {
            text: text.into(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".into()),
        stop_details: None,
        usage: Usage::default(),
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

#[test]
fn batched_and_streaming_produce_same_assistant_text() {
    run_with_large_stack(|| async {
        // ── Batched path
        let batched_mock = Arc::new(MockApiClient::new(vec![batched_response("hello world")]));
        let streaming_stub = Arc::new(MockStreamingApiClient::empty());
        let output_b = Arc::new(MockOutputStream::new());
        let orch_b = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched_mock,
            streaming_stub,
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output_b.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let outcome_b = orch_b.run_turn("ping").await.expect("batched");

        // ── Streaming path
        let stream_script = scripted![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "hello world"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let streaming_mock = Arc::new(MockStreamingApiClient::with_turns(vec![stream_script]));
        let batched_stub = Arc::new(MockApiClient::new(Vec::new()));
        let output_s = Arc::new(MockOutputStream::new());
        let orch_s = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched_stub,
            streaming_mock,
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output_s.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let outcome_s = orch_s.run_turn_streaming("ping").await.expect("stream");

        // Same outcome shape.
        match (outcome_b, outcome_s) {
            (
                ConversationOutcome::EndTurn {
                    turn_count: tc_b, ..
                },
                ConversationOutcome::EndTurn {
                    turn_count: tc_s, ..
                },
            ) => {
                assert_eq!(tc_b, 1);
                assert_eq!(tc_s, 1);
            }
            _ => panic!("unexpected outcome variants"),
        }

        // Same assistant text in the session history.
        let session_b = orch_b.session();
        let session_s = orch_s.session();
        let s_b = session_b.lock().await;
        let s_s = session_s.lock().await;
        let extract_text = |hist: &[ConversationMessage]| -> Option<String> {
            for m in hist {
                if let ConversationMessage::Assistant { content, .. } = m {
                    for blk in content {
                        if let ContentBlock::Text { text } = blk {
                            return Some(text.clone());
                        }
                    }
                }
            }
            None
        };
        assert_eq!(extract_text(&s_b.history), Some("hello world".into()));
        assert_eq!(extract_text(&s_s.history), Some("hello world".into()));
    });
}

// ── #3 (main-loop parity): per-turn reminder symmetry across paths ───────────
//
// Both the batched (`run_turn` / `try_run_turn_cancelable`) and streaming
// (`run_turn_streaming`) drivers must inject the SAME per-turn reminders in the
// canonical order. Historically the batched path injected `<new-diagnostics>`
// but NOT `<task-notification>`, and the streaming path the reverse — both are
// consume-once drains, so the bytes sent to the model diverged purely by entry
// path. These two tests pin the symmetry: each reminder, when its source is
// wired, now appears on BOTH paths.

struct OnceTaskNotifications(
    std::sync::Mutex<Vec<lingxi_core::host::task_registry::TaskNotification>>,
);
#[async_trait::async_trait]
impl orchestrator::prompt::task_notification::TaskNotificationProvider for OnceTaskNotifications {
    async fn take_pending_task_notifications(
        &self,
    ) -> Vec<lingxi_core::host::task_registry::TaskNotification> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct InlineRuntime;

#[async_trait::async_trait]
impl lingxi_core::host::RuntimeSpawner for InlineRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<lingxi_core::host::BackgroundTaskHandle, lingxi_core::host::RuntimeError> {
        tokio::spawn(task);
        Ok(lingxi_core::host::BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: 0,
        })
    }

    async fn sleep(&self, _duration: std::time::Duration) {}

    async fn cancel(
        &self,
        _handle: &lingxi_core::host::BackgroundTaskHandle,
    ) -> Result<(), lingxi_core::host::RuntimeError> {
        Ok(())
    }
}

struct EmptySideQuery;

#[async_trait::async_trait]
impl sidequery::SideQueryClient for EmptySideQuery {
    async fn query(
        &self,
        _request: sidequery::SideQueryRequest,
    ) -> Result<sidequery::SideQueryResponse, sidequery::SideQueryError> {
        Ok(sidequery::SideQueryResponse {
            text: Some("{\"filenames\":[]}".into()),
            structured: None,
            tool_calls: Vec::new(),
            usage: cost::Usage::default(),
            stop_reason: Some("end_turn".into()),
            retry_count: 0,
        })
    }
}

fn memory_prefetch(memdir: &std::path::Path) -> Arc<memory::prefetch::MemoryPrefetch> {
    Arc::new(memory::prefetch::MemoryPrefetch::new(
        Arc::new(memory::selector::MemorySelector::new(Arc::new(
            EmptySideQuery,
        ))),
        Arc::new(InlineRuntime),
        memory::memdir::MemdirRoots {
            user_memdir: memdir.to_path_buf(),
            session_memdir: memdir.join("session-memory"),
            team_memdir: None,
        },
    ))
}

/// A do-nothing tool registered under a real name.
///
/// Several reminders gate on a tool being DISPATCHABLE this turn
/// (`skill_listing` on `Skill`, `task_reminder` on `TaskUpdate`), so the fixture
/// has to put something in the registry under that name. Nothing here is ever
/// called: the gate is `find_dispatchable_tool(name).is_some()`.
struct NamedStubTool(&'static str);

#[async_trait::async_trait]
impl Tool for NamedStubTool {
    fn name(&self) -> &str {
        self.0
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| serde_json::json!({"type": "object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
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
    ) -> Result<(), ValidationError> {
        Ok(())
    }
    async fn check_permissions(
        &self,
        _input: &serde_json::Value,
        _ctx: &tool_api::context::ToolUseContext,
    ) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "test".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        String::new()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        _ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        Err(ToolError::Internal("stub tool is never dispatched".into()))
    }
}

struct StaticSkills;
#[async_trait::async_trait]
impl orchestrator::prompt::skill_listing::SkillListingProvider for StaticSkills {
    async fn skill_entries(&self) -> Vec<orchestrator::prompt::skill_listing::SkillListingEntry> {
        vec![orchestrator::prompt::skill_listing::SkillListingEntry {
            name: "debug".into(),
            description: "Debug a failing test".into(),
            when_to_use: None,
            is_bundled: false,
        }]
    }
}

/// Registry holding the tools the reminder gates look for.
///
/// Setting the registry's main-loop model here would be a no-op: orchestrator
/// construction overwrites it from `config.model` (`conversation/wiring.rs`),
/// and every turn republishes it from the live session
/// (`conversation/tooling.rs`). The `OO()` gate that decides whether the task
/// reminder renders is therefore pinned via `OrchestratorConfig::model` below.
fn gated_tool_registry() -> Arc<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(NamedStubTool("Skill")));
    registry.register_builtin(Arc::new(NamedStubTool("TaskUpdate")));
    Arc::new(registry)
}

struct MockDiag(Option<String>);
#[async_trait::async_trait]
impl lingxi_core::host::NewDiagnosticsSource for MockDiag {
    async fn take_new_diagnostics_block(&self) -> Option<String> {
        self.0.clone()
    }
}

fn one_task_notification() -> lingxi_core::host::task_registry::TaskNotification {
    lingxi_core::host::task_registry::TaskNotification {
        task_id: "b12345678".into(),
        task_type: "local_bash".into(),
        status: "completed".into(),
        description: "run tests".into(),
        tool_use_id: None,
        output_path: Some("/tmp/tasks/b12345678.output".into()),
        exit_code: Some(0),
        error: None,
        result: None,
        usage: None,
        killed_by: None,
        worktree_path: None,
        worktree_branch: None,
        workflow_failures: Vec::new(),
        workflow_agent_count: None,
        workflow_total_tokens: None,
        workflow_total_tool_calls: None,
        workflow_duration_ms: None,
        ..Default::default()
    }
}

fn one_dream_notification() -> lingxi_core::host::task_registry::TaskNotification {
    lingxi_core::host::task_registry::TaskNotification {
        task_id: "d12345678".into(),
        task_type: "dream".into(),
        status: "completed".into(),
        description: "memory consolidation".into(),
        result: Some("merged durable notes".into()),
        ..Default::default()
    }
}

/// Put the session into the state the remaining reminder gates require.
///
/// `plan_mode` gates on the session flag; the task/todo reminder gates on BOTH
/// cadence counters having reached their thresholds and on a non-empty history.
/// Applied identically to both orchestrators so any divergence in the output is
/// the drivers', not the fixture's.
async fn arm_gated_reminders(orch: &ConversationOrchestrator) {
    let session = orch.session();
    let mut s = session.lock().await;
    s.plan_mode = true;
    s.turns_since_last_todo_write = tool_task::reminder::TURNS_SINCE_WRITE;
    s.turns_since_last_reminder = tool_task::reminder::TURNS_BETWEEN_REMINDERS;
    s.history.push(ConversationMessage::user_meta(
        lingxi_core::types::MessageId::new(),
        "seed".into(),
    ));
}

/// Render the outgoing messages, with THIS orchestrator's own session id masked.
///
/// The two orchestrators are separate sessions, so anything that embeds a
/// session id (the plan-mode reminder names the session's plan file) differs by
/// construction. Masking each instance's OWN id — rather than scrubbing every
/// UUID-shaped string — keeps the comparison blind to that one identifier and
/// nothing else.
async fn model_input_texts(
    orch: &ConversationOrchestrator,
    messages: &[ConversationMessage],
) -> Vec<String> {
    let session_id = orch.session().lock().await.session_id.as_uuid().to_string();
    messages
        .iter()
        .map(|m| m.text_content().replace(&session_id, "<session-id>"))
        .collect()
}

/// The model-facing order the shared collector must produce, as
/// (label, distinctive substring) in the order they are expected to appear.
///
/// Sourced from the collector's own sequence in
/// `conversation/drivers/prepare.rs`, which this file locks; the substrings come
/// from each renderer, not from a previous run's output. The durable
/// task-notification leads because it is appended to the snapshot BEFORE the
/// transient reminders, then the transient ones follow in collector order:
/// output_style → plan_mode → skill_listing → task/todo → memory_update →
/// total_tokens.
///
/// `total_tokens` is last because it is DEFAULT ON in this port
/// (`prompt::total_tokens::PORT_DEFAULT_MODE`, oracle parity since 2026-08-20),
/// so it fires on a stock turn. `CLAUDE_CODE_TOTAL_TOKENS_REMINDER=off` in the
/// environment turns it back off and this test will report it missing — that is
/// an environment override, not a regression.
///
/// NOT covered here, and deliberately so rather than silently: `silent_turn`
/// needs a model-capability table entry AND a multi-turn tool-result stretch,
/// neither of which this single-turn fixture can produce. `brief_mode`,
/// `conditional_rules`, `nested_memory`, `new_diagnostics`, `agent_listing`,
/// `changed_files`, `tool_search_usage`, `async_hook_response`,
/// `relevant_memory` and `skill_discovery` stay dark here; their PRESENCE on
/// both paths is pinned by `tests/reminder_twin_wiring_test.rs`, their position
/// is not pinned by anything yet.
const EXPECTED_REMINDER_ORDER: &[(&str, &str)] = &[
    ("task_notification (durable)", "<task-notification>"),
    ("output_style", "Explanatory output style is active"),
    ("plan_mode", "Plan mode is active."),
    (
        "skill_listing",
        "The following skills are available for use with the Skill tool:",
    ),
    (
        "task_reminder",
        "The task tools haven't been used recently.",
    ),
    ("memory_update", "Background memory consolidation updated"),
    ("total_tokens", "<total_tokens>"),
];

/// Pin the RELATIVE ORDER of every reminder this fixture switches on.
///
/// This is the assertion with teeth. The byte-equality check between the two
/// paths cannot see a reorder inside the collector — one function feeds both
/// entries, so a swap moves both sides identically and the comparison still
/// passes. Only this function fails on that.
fn assert_shared_reminder_order(messages: &[String]) {
    let mut last: Option<(&str, usize)> = None;
    for (label, needle) in EXPECTED_REMINDER_ORDER {
        let hits = messages.iter().filter(|text| text.contains(needle)).count();
        assert_eq!(
            hits, 1,
            "{label}: needle {needle:?} must match exactly one message, or the \
             position below is meaningless (0 hits can also mean the producer \
             is switched off in this environment): {messages:#?}"
        );
        let at = messages
            .iter()
            .position(|text| text.contains(needle))
            .expect("just counted one hit");
        if let Some((prev_label, prev_at)) = last {
            assert!(
                prev_at < at,
                "{prev_label} must precede {label} (collector order in \
                 conversation/drivers/prepare.rs), got {prev_at} then {at}: {messages:#?}"
            );
        }
        last = Some((label, at));
    }
}

#[test]
fn batched_and_streaming_share_complete_reminder_order() {
    run_with_large_stack(|| async {
        let batched_memdir = tempfile::TempDir::new().expect("batched memory directory");
        let streaming_memdir = tempfile::TempDir::new().expect("streaming memory directory");
        let config = OrchestratorConfig {
            output_style: Some("Explanatory".into()),
            // `OrchestratorConfig::DEFAULT_MODEL` ("claude-opus-4-8") is an
            // Anthropic id that is NOT on the `todo_tools_gate` allowlist, so the
            // task reminder is correctly suppressed under it. The orchestrator
            // republishes the registry's main-loop model from the SESSION model
            // every turn, so pinning it here — not on the registry — is what
            // survives into the gate.
            model: FIXTURE_MODEL.into(),
            ..OrchestratorConfig::default()
        };

        let batched_mock = Arc::new(MockApiClient::new(vec![batched_response("ok")]));
        let orch_b = ConversationOrchestrator::new_with_streaming(
            config.clone(),
            batched_mock.clone(),
            Arc::new(MockStreamingApiClient::empty()),
            gated_tool_registry(),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_memory_prefetch(memory_prefetch(batched_memdir.path()))
        .with_skill_listing(Arc::new(StaticSkills))
        .with_task_notifications(Arc::new(OnceTaskNotifications(std::sync::Mutex::new(
            vec![one_dream_notification()],
        ))));
        arm_gated_reminders(&orch_b).await;
        orch_b.run_turn("ping").await.expect("batched");
        let batched_calls = batched_mock.captured_msgs().await;
        let batched_messages = model_input_texts(&orch_b, &batched_calls[0]).await;

        let stream_script = scripted![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let streaming_mock = Arc::new(MockStreamingApiClient::with_turns(vec![stream_script]));
        let orch_s = ConversationOrchestrator::new_with_streaming(
            config,
            Arc::new(MockApiClient::new(Vec::new())),
            streaming_mock.clone(),
            gated_tool_registry(),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_memory_prefetch(memory_prefetch(streaming_memdir.path()))
        .with_skill_listing(Arc::new(StaticSkills))
        .with_task_notifications(Arc::new(OnceTaskNotifications(std::sync::Mutex::new(
            vec![one_dream_notification()],
        ))));
        arm_gated_reminders(&orch_s).await;
        orch_s.run_turn_streaming("ping").await.expect("streaming");
        let streaming_calls = streaming_mock.captured_calls().await;
        let streaming_messages = model_input_texts(&orch_s, &streaming_calls[0].messages).await;

        // Order first: it is the only check that can see a reorder inside the
        // collector, and it gives the clearer message when one happens.
        assert_shared_reminder_order(&batched_messages);
        assert_shared_reminder_order(&streaming_messages);
        // Then cross-path equality. This does NOT pin collector order — one
        // function feeds both entries, so a swap moves both sides identically.
        // What it does pin is that both real entries route through that
        // collector and neither adds, drops, or repositions anything AROUND it:
        // the durable task-notification's placement ahead of the transient
        // reminders, the leading context, and the user message itself.
        assert_eq!(
            batched_messages, streaming_messages,
            "the batched and streaming entries must send the same model input; a \
             difference here is something one driver does outside the shared \
             collector, not a collector-order change"
        );
    });
}

/// BATCHED path must inject the `<task-notification>` reminder (parity with
/// streaming). Pre-fix the batched snapshot omitted it entirely.
#[test]
fn batched_turn_injects_task_notification_reminder() {
    run_with_large_stack(|| async {
        let batched_mock = Arc::new(MockApiClient::new(vec![batched_response("ok")]));
        let streaming_stub = Arc::new(MockStreamingApiClient::empty());
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched_mock.clone(),
            streaming_stub,
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_task_notifications(Arc::new(OnceTaskNotifications(std::sync::Mutex::new(
            vec![one_task_notification()],
        ))));
        orch.run_turn("ping").await.expect("batched");
        let calls = batched_mock.captured_msgs().await;
        let first = format!("{:?}", calls[0]);
        assert!(
        first.contains("<task-notification>"),
        "batched turn must inject the task-notification reminder (parity with streaming); got: {first}"
    );
    });
}

/// STREAMING path must inject the `<new-diagnostics>` reminder (parity with
/// batched). Pre-fix the streaming snapshot omitted it entirely.
#[test]
fn streaming_turn_injects_new_diagnostics_reminder() {
    run_with_large_stack(|| async {
        let stream_script = scripted![
            message_start("m1", "claude-opus-4-7"),
            content_block_start_text(0),
            text_delta(0, "ok"),
            content_block_stop(0),
            message_delta_stop("end_turn"),
            message_stop(),
        ];
        let streaming_mock = Arc::new(MockStreamingApiClient::with_turns(vec![stream_script]));
        let batched_stub = Arc::new(MockApiClient::new(Vec::new()));
        let block = "<new-diagnostics>The following new diagnostic issues were detected:\n\nx.rs:\n  \u{2718} [Line 1:1] boom</new-diagnostics>";
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched_stub,
            streaming_mock.clone(),
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_new_diagnostics_source(Arc::new(MockDiag(Some(block.to_string()))));
        orch.run_turn_streaming("ping").await.expect("stream");
        let calls = streaming_mock.captured_calls().await;
        let first = format!("{:?}", calls[0].messages);
        assert!(
        first.contains("<new-diagnostics>"),
        "streaming turn must inject the new-diagnostics reminder (parity with batched); got: {first}"
    );
    });
}

/// #1 (main-loop parity): a connect-phase prompt-too-long on the STREAMING path
/// (the adapter returns `Err(ContextOverflow)` from `stream()`) must trigger the
/// reactive PTL recovery — the SAME `call_api_with_ptl_recovery` helper the
/// batched path uses — recovering via a non-streaming call, NOT bubbling a hard
/// `OrchestratorError::Streaming` error (the prior documented divergence).
#[test]
fn streaming_connect_413_recovers_via_reactive_ptl() {
    run_with_large_stack(|| async {
        use llm_runtime::LlmError;
        // The stream OPEN returns a connect-phase 413/ContextOverflow once.
        let streaming_mock = Arc::new(MockStreamingApiClient::with_open_error(
            LlmError::ContextOverflow { token_gap: 100 },
            Vec::new(),
        ));
        // Reactive recovery issues a batched messages_create that SUCCEEDS.
        let batched_mock = Arc::new(MockApiClient::new(vec![batched_response("recovered")]));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            batched_mock.clone(),
            streaming_mock.clone(),
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        let outcome = orch
            .run_turn_streaming("ping")
            .await
            .expect("streaming 413 must RECOVER, not surface a hard error");
        match outcome {
            ConversationOutcome::EndTurn { turn_count, .. } => assert_eq!(turn_count, 1),
            other => panic!("expected EndTurn after recovery, got {other:?}"),
        }
        // One stream-open attempt; the recovery then used the batched API once.
        assert_eq!(
            streaming_mock.captured_calls().await.len(),
            1,
            "exactly one stream-open attempt"
        );
        assert_eq!(
            batched_mock.captured_msgs().await.len(),
            1,
            "reactive recovery issued exactly one batched call"
        );
        // The recovered assistant text reached the session history.
        let session = orch.session();
        let s = session.lock().await;
        let has_recovered = s.history.iter().any(|m| {
        matches!(m, ConversationMessage::Assistant { content, .. }
            if content.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "recovered")))
    });
        assert!(
            has_recovered,
            "recovered assistant text must be appended to session history"
        );
    });
}
