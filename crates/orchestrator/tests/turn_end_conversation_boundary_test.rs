//! Admission tests for PR 4, terminal-state family 2: `EndConversation`.
//!
//! These are the §4 rows where the two drivers genuinely disagree, and the
//! disagreement is an ORDERING one that no outcome-shaped assertion can see.
//!
//! Batched (`turn_loop.rs`) runs the wakeup check FIRST — short-circuited on
//! hook-prevent and tool-requested-end — and only then swaps the
//! `end_conversation_slot`, unconditionally. `end_conversation` is the first arm
//! of the disposition match, so it outranks both hook-prevent and
//! tool-requested-end in the outcome.
//!
//! Streaming (`conversation/drivers/mod.rs`) swaps the EndConversation slot
//! FIRST and returns on a hit — which means the wakeup check below it never
//! runs, and the wakeup flag is NOT consumed. §4 states this plainly: "命中
//! EndConversation 后不会执行后面的 wakeup 消费。因此两条路径都可能跳过 wakeup
//! 消费。"
//!
//! So the same input — both slots raised — leaves the two drivers in DIFFERENT
//! next-turn states, on purpose. A shared end-handler that picks one order
//! erases a row of the matrix, and both drivers still end their turn, and every
//! assertion about outcomes still passes. The only thing that changes is whether
//! a scheduled wake-up survives.

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    content_block_stop, input_json_delta, message_delta_stop, message_start, message_stop,
    mock_message_response, noop_hook_executor, MockApiClient, MockOutputStream,
    MockStreamingApiClient, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig};
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use platform_api::OutputEvent;
use protocol::ToolUseId;
use serde_json::json;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{DescriptionOptions, PromptOptions, Tool, ToolStaticContext};
use tool_api::{ToolCallResult, ToolError, ValidationError};

fn run_with_large_stack<F, Fut>(build: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    let handle = std::thread::Builder::new()
        .name("turn-end-conversation-boundary".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build test runtime")
                .block_on(build());
        })
        .expect("spawn large-stack test thread");
    handle.join().expect("large-stack test thread panicked");
}

/// See `turn_end_wakeup_boundary_test`: the wakeup arm is gated on
/// `Fable5Mitigations`, which the default model does not carry.
const GATED_MODEL: &str = "claude-fable-5-1";

struct StubTool {
    name: &'static str,
    ends_turn: bool,
}

#[async_trait::async_trait]
impl Tool for StubTool {
    fn name(&self) -> &str {
        self.name
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({"type": "object"}));
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
        Ok(ToolCallResult {
            data: json!({"ok": true}),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: self
                .ends_turn
                .then(|| json!({"_meta": {"claude/endTurn": true}})),
        })
    }
}

fn registry(tools: &[(&'static str, bool)]) -> Arc<ToolRegistry> {
    let mut r = ToolRegistry::new();
    for (name, ends_turn) in tools {
        r.register_builtin(Arc::new(StubTool {
            name,
            ends_turn: *ends_turn,
        }));
    }
    Arc::new(r)
}

fn config() -> OrchestratorConfig {
    OrchestratorConfig {
        model: GATED_MODEL.into(),
        ..OrchestratorConfig::default()
    }
}

fn ended_message_emitted(output: &MockOutputStream) -> bool {
    futures::executor::block_on(output.snapshot()).iter().any(
        |event| matches!(event, OutputEvent::Text { text } if text == orchestrator::prompt::end_conversation::END_CONVERSATION_ENDED_MESSAGE),
    )
}

/// BATCHED: the EndConversation slot is consumed even when a tool ended the turn.
///
/// The swap is unconditional and sits AFTER the short-circuited wakeup check, so
/// unlike the wakeup flag it never survives a turn. And `end_conversation` is
/// the first arm of the disposition match, so it outranks the tool's request:
/// the user sees the end-of-conversation message rather than a plain end.
#[test]
fn batched_end_conversation_outranks_a_tool_requested_end_and_always_consumes() {
    run_with_large_stack(|| async {
        let end_slot: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
        let wakeup_slot = Arc::new(AtomicBool::new(true));
        let output = Arc::new(MockOutputStream::new());
        let api = Arc::new(MockApiClient::new(vec![mock_message_response(
            vec![LlmContentBlock::ToolCall {
                id: ToolUseId::new().to_string(),
                name: "EndsTurn".into(),
                input: json!({}),
            }],
            Some("tool_use"),
        )]));

        let orch = ConversationOrchestrator::new(
            config(),
            api.clone(),
            registry(&[("EndsTurn", true)]),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_end_conversation_slot(end_slot.clone())
        .with_loop_wakeup_armed_slot(wakeup_slot.clone());

        orch.run_turn("ping").await.expect("turn");

        assert!(
            ended_message_emitted(&output),
            "end_conversation is the FIRST arm of the disposition match, so it outranks the \
             tool-requested end; the user must see the end-of-conversation message"
        );
        assert!(
            !end_slot.load(Ordering::SeqCst),
            "the EndConversation swap is unconditional on the batched path — unlike the wakeup \
             flag, it is never left raised for a later turn"
        );
        assert!(
            wakeup_slot.load(Ordering::SeqCst),
            "the tool-requested end still short-circuits the wakeup check, so THAT flag \
             survives. The two slots behave oppositely in the same turn, which is the point."
        );
    });
}

/// STREAMING: hitting EndConversation leaves the wakeup flag armed.
///
/// The streaming arm swaps the EndConversation slot and returns immediately, so
/// the wakeup check below it never runs. Nothing about the turn's outcome
/// records that — both drivers end the turn and emit the same message. The only
/// difference is what the NEXT turn inherits.
#[test]
fn streaming_end_conversation_returns_before_the_wakeup_check() {
    run_with_large_stack(|| async {
        let end_slot: Arc<AtomicBool> = Arc::new(AtomicBool::new(true));
        let wakeup_slot = Arc::new(AtomicBool::new(true));
        let output = Arc::new(MockOutputStream::new());
        let tool_use_id = ToolUseId::new();

        // A round whose ONLY tool is ScheduleWakeup — so if the wakeup check
        // were reached, it would consume the flag and end the turn itself.
        let stream = scripted![
            message_start("m1", GATED_MODEL),
            orchestrator::test_support_stream::content_block_start_tool_use(
                0,
                tool_use_id,
                "ScheduleWakeup",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];

        let orch = ConversationOrchestrator::new_with_streaming(
            config(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![stream])),
            registry(&[("ScheduleWakeup", false)]),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            output.clone(),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_end_conversation_slot(end_slot.clone())
        .with_loop_wakeup_armed_slot(wakeup_slot.clone());

        orch.run_turn_streaming("ping").await.expect("streaming");

        assert!(
            ended_message_emitted(&output),
            "the streaming arm emits the same end-of-conversation message"
        );
        assert!(
            !end_slot.load(Ordering::SeqCst),
            "the EndConversation slot is consumed by the arm that fires"
        );
        assert!(
            wakeup_slot.load(Ordering::SeqCst),
            "the wakeup flag must SURVIVE: streaming returns from the EndConversation arm \
             before reaching the wakeup check, and calling that check is what consumes the \
             flag. §4: 命中 EndConversation 后不会执行后面的 wakeup 消费. A shared handler \
             that runs the wakeup consume first — the batched order — eats it here."
        );
    });
}

/// The two drivers differ on the SAME input, and that is the row.
///
/// Batched reaches the wakeup check before EndConversation, so a lone
/// `ScheduleWakeup` round with the end-slot raised consumes BOTH. Streaming
/// returns from EndConversation first, so it consumes only the end slot.
///
/// Written as one test over both paths because the assertion IS the difference:
/// separate per-driver tests would each pass under a shared handler that picked
/// either order, and only a comparison notices that the two became the same.
#[test]
fn the_two_drivers_leave_different_wakeup_state_after_end_conversation() {
    run_with_large_stack(|| async {
        // ── batched: wakeup check runs first, so both slots end up consumed
        let batched_end = Arc::new(AtomicBool::new(true));
        let batched_wakeup = Arc::new(AtomicBool::new(true));
        let orch_b = ConversationOrchestrator::new(
            config(),
            Arc::new(MockApiClient::new(vec![mock_message_response(
                vec![LlmContentBlock::ToolCall {
                    id: ToolUseId::new().to_string(),
                    name: "ScheduleWakeup".into(),
                    input: json!({}),
                }],
                Some("tool_use"),
            )])),
            registry(&[("ScheduleWakeup", false)]),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_end_conversation_slot(batched_end.clone())
        .with_loop_wakeup_armed_slot(batched_wakeup.clone());
        orch_b.run_turn("ping").await.expect("batched");

        // ── streaming: EndConversation returns first, so the wakeup survives
        let streaming_end = Arc::new(AtomicBool::new(true));
        let streaming_wakeup = Arc::new(AtomicBool::new(true));
        let stream = scripted![
            message_start("m1", GATED_MODEL),
            orchestrator::test_support_stream::content_block_start_tool_use(
                0,
                ToolUseId::new(),
                "ScheduleWakeup",
            ),
            input_json_delta(0, "{}"),
            content_block_stop(0),
            message_delta_stop("tool_use"),
            message_stop(),
        ];
        let orch_s = ConversationOrchestrator::new_with_streaming(
            config(),
            Arc::new(MockApiClient::new(Vec::new())),
            Arc::new(MockStreamingApiClient::with_turns(vec![stream])),
            registry(&[("ScheduleWakeup", false)]),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        )
        .with_end_conversation_slot(streaming_end.clone())
        .with_loop_wakeup_armed_slot(streaming_wakeup.clone());
        orch_s.run_turn_streaming("ping").await.expect("streaming");

        assert!(
            !batched_end.load(Ordering::SeqCst) && !streaming_end.load(Ordering::SeqCst),
            "both drivers consume the EndConversation slot"
        );
        assert!(
            !batched_wakeup.load(Ordering::SeqCst),
            "batched reaches the wakeup check BEFORE EndConversation, so it consumes the flag"
        );
        assert!(
            streaming_wakeup.load(Ordering::SeqCst),
            "streaming returns from EndConversation first, so it does not"
        );
        assert_ne!(
            batched_wakeup.load(Ordering::SeqCst),
            streaming_wakeup.load(Ordering::SeqCst),
            "the two drivers must DIFFER here. §4 records this asymmetry deliberately; a \
             shared end-handler that settles on one order makes both sides agree, every \
             outcome assertion still passes, and one of the two silently changes what the \
             next turn inherits."
        );
    });
}
