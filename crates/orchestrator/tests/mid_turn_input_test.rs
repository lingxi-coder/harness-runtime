//! Mid-turn drain + Now-priority abort-reason seam (spec §27).
//!
//! Exercises the orchestrator-side half of the two coupled features:
//!
//! 1. MID-TURN DRAIN: a wired [`MidTurnInputSource`] is polled at each
//!    top-of-loop step; its returned text is injected as a meta user message so
//!    the NEXT model call sees it. When no source is wired the drain is a strict
//!    no-op (byte-identical to today).
//!
//! 2. NOW-ABORT reason: when the turn's cancel token fires AND the wired
//!    [`CancelReasonFlag`] reads `QueueNowCommand`, the loop ends the turn
//!    WITHOUT injecting the user-interrupt message (the urgent command is the
//!    interruption, run next by the between-turn drain). With the default
//!    reason (`UserInterrupt`, or no flag wired) the interrupt message IS
//!    injected — today's behavior unchanged.

use async_trait::async_trait;
use lingxi_core::types::{ContentBlock, ConversationMessage, ToolUseId};
use orchestrator::prompt::mid_turn_input::{CancelReason, CancelReasonFlag, MidTurnInputSource};
use orchestrator::test_support::{
    MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::test_support_stream::{
    content_block_start_text, content_block_stop, input_json_delta, message_delta_stop,
    message_start, message_stop, text_delta, MockStreamingApiClient,
};
use orchestrator::{scripted, ConversationOrchestrator, OrchestratorConfig, TurnOutcome};
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tool_api::progress::ToolProgressSender;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, InterruptBehavior, PromptOptions, Tool, ToolCallResult, ToolError,
    ToolStaticContext, ValidationError,
};

/// A concurrency-safe Cancel-behavior tool that blocks on its `ctx.cancel` token
/// so the turn stays in-flight until we fire the turn's cancel — exercising the
/// post-tools cancel guard (where the abort-reason disambiguation lives).
struct CancelBlockingTool;

#[async_trait]
impl Tool for CancelBlockingTool {
    fn name(&self) -> &str {
        "CancelTool"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
        &SCHEMA
    }
    fn is_enabled(&self, _ctx: &ToolStaticContext) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024 * 1024
    }
    fn is_concurrency_safe(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn is_read_only(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn interrupt_behavior(&self, _input: &serde_json::Value) -> InterruptBehavior {
        InterruptBehavior::Cancel
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
    async fn description(&self, _input: &serde_json::Value, _opts: &DescriptionOptions) -> String {
        "cancel-tool".into()
    }
    async fn prompt(&self, _opts: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _input: serde_json::Value,
        ctx: tool_api::context::ToolUseContext,
        _tx: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        match ctx.cancel.clone() {
            Some(t) => {
                t.cancelled().await;
                Err(ToolError::Aborted)
            }
            None => Ok(ToolCallResult {
                data: json!({ "content": "ran-to-end" }),
                model_content: None,
                new_messages: vec![],
                context_modifier: None,
                is_error: false,
                mcp_meta: None,
            }),
        }
    }
}

fn build_orch_with_cancel_tool(api: Arc<MockStreamingApiClient>) -> ConversationOrchestrator {
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(CancelBlockingTool) as Arc<dyn Tool>);
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let memory = Arc::new(StaticMemoryProvider::empty());
    ConversationOrchestrator::new_with_streaming(
        OrchestratorConfig::default(),
        batched,
        api,
        Arc::new(registry),
        hooks,
        perms,
        output,
        memory,
        PathBuf::from("/tmp"),
    )
}

/// Whether `session.history` carries any user message text mentioning the
/// interrupt phrase ("[Request interrupted by user...]").
async fn history_has_interrupt(orch: &ConversationOrchestrator) -> bool {
    let session = orch.session();
    let s = session.lock().await;
    s.history.iter().any(|m| match m {
        ConversationMessage::User { content, .. } => content.iter().any(
            |b| matches!(b, ContentBlock::Text { text } if text.contains("interrupted by user")),
        ),
        _ => false,
    })
}

fn build_orch(api: Arc<MockStreamingApiClient>) -> ConversationOrchestrator {
    build_orch_with_config(api, OrchestratorConfig::default())
}

fn build_orch_with_config(
    api: Arc<MockStreamingApiClient>,
    config: OrchestratorConfig,
) -> ConversationOrchestrator {
    let batched = Arc::new(MockApiClient::new(Vec::new()));
    let output = Arc::new(MockOutputStream::new());
    let tools = Arc::new(ToolRegistry::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let memory = Arc::new(StaticMemoryProvider::empty());
    ConversationOrchestrator::new_with_streaming(
        config,
        batched,
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        PathBuf::from("/tmp"),
    )
}

/// A `MidTurnInputSource` that yields its texts one call at a time, then `None`.
struct ScriptedSource(std::sync::Mutex<std::collections::VecDeque<String>>);

impl ScriptedSource {
    fn new(texts: Vec<&str>) -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(
            texts.into_iter().map(str::to_string).collect(),
        )))
    }
}

#[async_trait]
impl MidTurnInputSource for ScriptedSource {
    async fn take_mid_turn_input(&self) -> Option<String> {
        self.0.lock().unwrap().pop_front()
    }
}

/// A source that appears empty on the first loop poll, then yields its one
/// queued message. This models a message arriving while the first model/tool
/// step is in flight, just before the loop would attempt its next turn.
struct DelayedOnceSource(std::sync::Mutex<(usize, Option<String>)>);

impl DelayedOnceSource {
    fn after_empty_polls(empty_polls: usize, text: &str) -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new((
            empty_polls,
            Some(text.to_string()),
        ))))
    }
}

#[async_trait]
impl MidTurnInputSource for DelayedOnceSource {
    async fn take_mid_turn_input(&self) -> Option<String> {
        let mut state = self.0.lock().unwrap();
        if state.0 != 0 {
            state.0 -= 1;
            None
        } else {
            state.1.take()
        }
    }
}

/// Collect the text of every user message in the FIRST captured model request.
async fn first_request_user_texts(api: &MockStreamingApiClient) -> Vec<String> {
    let calls = api.captured_calls().await;
    let first = calls.first().expect("at least one model call");
    first
        .messages
        .iter()
        .filter_map(|m| match m {
            ConversationMessage::User { content, .. } => {
                let joined: String = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                Some(joined)
            }
            _ => None,
        })
        .collect()
}

/// MID-TURN DRAIN: the wired source's text is injected and visible to the model
/// on the FIRST model call (the drain runs at the top of the loop BEFORE the
/// call). The drain loop pulls EVERY queued item before the call.
#[tokio::test]
async fn mid_turn_source_injects_queued_text_into_first_model_call() {
    let stream = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let orch = build_orch(api.clone());
    orch.set_mid_turn_input(ScriptedSource::new(vec!["queued one", "queued two"]));

    let cancel = CancellationToken::new();
    let outcome = orch
        .run_turn_streaming_with_cancel("seed", cancel)
        .await
        .unwrap();
    assert_eq!(outcome, TurnOutcome::EndTurn);

    let texts = first_request_user_texts(&api).await;
    let all = texts.join("\n");
    assert!(all.contains("seed"), "seed prompt present: {texts:?}");
    assert!(all.contains("queued one"), "first drained item: {texts:?}");
    assert!(all.contains("queued two"), "second drained item: {texts:?}");
    // 2.1.206 YAt envelope: each drained user message is wrapped with the
    // "The user sent a new message while you were working:" prefix and the
    // mid-turn explainer suffix (em-dash U+2014).
    assert!(
        all.contains("The user sent a new message while you were working:\nqueued one"),
        "envelope prefix wraps the drained text: {texts:?}"
    );
    assert!(
        all.contains("This is how LingXi surfaces messages the user sends mid-turn \u{2014} within the running turn, often alongside the next tool result, rather than as a separate conversation turn. Address the message above as you continue this turn."),
        "envelope explainer suffix present: {texts:?}"
    );
}

/// A message that arrives while the final model response is streaming must be
/// drained before the natural end-turn disposition is committed. Otherwise a
/// text-only turn has no second top-of-loop boundary and silently strands the
/// pending message.
#[tokio::test]
async fn mid_turn_input_arriving_during_final_response_continues_the_turn() {
    let first = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "first answer"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let second = scripted![
        message_start("m2", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "updated answer"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![first, second]));
    let orch = build_orch(api.clone());
    orch.set_mid_turn_input(DelayedOnceSource::after_empty_polls(1, "late guidance"));

    let outcome = orch
        .run_turn_streaming_with_cancel("seed", CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome, TurnOutcome::EndTurn);
    let calls = api.captured_calls().await;
    assert_eq!(
        calls.len(),
        2,
        "late input must trigger another model iteration"
    );
    let second_user_text = calls[1]
        .messages
        .iter()
        .filter_map(|message| match message {
            ConversationMessage::User { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(""),
            ),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        second_user_text.contains("late guidance"),
        "{second_user_text}"
    );
}

/// NO SOURCE WIRED: the drain is a strict no-op — the outgoing request carries
/// ONLY the seed prompt (no extra injected user messages). This is the
/// regression guard: an un-wired turn is byte-identical to today.
#[tokio::test]
async fn no_source_is_noop_only_seed_prompt_reaches_model() {
    let stream = scripted![
        message_start("m1", "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, "ok"),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ];
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let orch = build_orch(api.clone());
    // No mid-turn source wired.

    let cancel = CancellationToken::new();
    orch.run_turn_streaming_with_cancel("only seed", cancel)
        .await
        .unwrap();

    let texts = first_request_user_texts(&api).await;
    let all = texts.join("\n");
    assert!(all.contains("only seed"));
    // The only non-meta-context user text is the seed; nothing else injected.
    assert!(
        !all.contains("queued"),
        "no source ⇒ nothing injected: {texts:?}"
    );
}

/// A message that arrives during a streaming tool step must be persisted before
/// the `--max-turns` terminal exits the loop. Claude Code 2.1.205 fixed the
/// previous loss of this message at the turn cap; this locks that behavior.
#[tokio::test]
async fn mid_turn_input_is_preserved_when_max_turns_ends_streaming_loop() {
    let tool_use_id = ToolUseId::new();
    let stream = scripted![
        message_start("m1", "claude-opus-4-7"),
        orchestrator::test_support_stream::content_block_start_tool_use(
            0,
            tool_use_id,
            "UnknownTool",
        ),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    let api = Arc::new(MockStreamingApiClient::with_turns(vec![stream]));
    let orch = build_orch_with_config(
        api.clone(),
        OrchestratorConfig {
            max_turns: 1,
            ..OrchestratorConfig::default()
        },
    );
    // The first top-of-loop drain sees nothing. The source then yields once at
    // the second loop boundary, exactly where the max-turns guard used to return
    // before consuming it.
    orch.set_mid_turn_input(DelayedOnceSource::after_empty_polls(
        1,
        "please preserve this message",
    ));

    let outcome = orch
        .run_turn_streaming_with_cancel("seed", CancellationToken::new())
        .await
        .expect("the one-turn cap must end the stream cleanly");
    assert_eq!(outcome, TurnOutcome::MaxTurns);
    assert_eq!(api.captured_calls().await.len(), 1, "no second model call");

    let session = orch.session();
    let s = session.lock().await;
    assert!(
        s.history.iter().any(|message| matches!(
            message,
            ConversationMessage::User { content, .. }
                if content.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text }
                        if text.contains("please preserve this message")
                ))
        )),
        "the queued mid-turn message must remain in history when max_turns ends the loop"
    );
}

/// Script ONE tool_use turn for the blocking Cancel tool (no second turn — the
/// abort must stop the loop). Shared by the two interrupt-disambiguation tests.
fn one_cancel_tool_turn() -> Arc<MockStreamingApiClient> {
    let id = ToolUseId::new();
    let turn1 = scripted![
        message_start("m1", "claude-opus-4-7"),
        orchestrator::test_support_stream::content_block_start_tool_use(0, id, "CancelTool"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ];
    Arc::new(MockStreamingApiClient::with_turns(vec![turn1]))
}

/// Fire `cancel` after a short delay so the blocking CancelTool is in flight when
/// the turn's cancel token trips — driving the POST-TOOLS cancel guard (where the
/// abort-reason disambiguation lives).
fn fire_cancel_soon(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
    })
}

/// NOW-ABORT reason: when the turn's cancel fires mid-tools AND the reason flag
/// reads `QueueNowCommand`, the turn ends Cancelled but does NOT inject the
/// user-interrupt message — the urgent queued command will run next via the
/// between-turn drain.
#[tokio::test]
async fn now_command_abort_does_not_inject_interrupt_message() {
    let api = one_cancel_tool_turn();
    let orch = Arc::new(build_orch_with_cancel_tool(api.clone()));
    let reason = CancelReasonFlag::new();
    reason.set(CancelReason::QueueNowCommand);
    orch.set_cancel_reason(reason);

    let cancel = CancellationToken::new();
    let firer = fire_cancel_soon(cancel.clone());
    let outcome = orch
        .run_turn_streaming_with_cancel("call the cancel tool", cancel)
        .await
        .unwrap();
    firer.await.unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);

    assert!(
        !history_has_interrupt(&orch).await,
        "a Now-command abort must NOT inject the user-interrupt message"
    );
}

/// DEFAULT reason (no flag wired): a mid-tools cancel injects the user-interrupt
/// message exactly as before — the regression guard proving the Now-abort branch
/// does not change today's Ctrl+C behavior.
#[tokio::test]
async fn user_interrupt_default_injects_interrupt_message() {
    let api = one_cancel_tool_turn();
    let orch = Arc::new(build_orch_with_cancel_tool(api.clone()));
    // No cancel-reason flag wired ⇒ defaults to UserInterrupt.

    let cancel = CancellationToken::new();
    let firer = fire_cancel_soon(cancel.clone());
    let outcome = orch
        .run_turn_streaming_with_cancel("call the cancel tool", cancel)
        .await
        .unwrap();
    firer.await.unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);

    assert!(
        history_has_interrupt(&orch).await,
        "a default (user) interrupt MUST inject the interrupt message (today's behavior)"
    );
}

// ── §4 late-drain rows ───────────────────────────────────────────────────────
//
// `mid_turn_input_arriving_during_final_response_continues_the_turn` above pins
// the NATURAL end: streaming's `Complete` arm gives pending input one last drain
// AFTER `emit_end_turn` has already fired, and continues the SAME turn on a hit.
//
// §4 keeps one neighbour of that row deliberately different: a tool-requested
// end produces `ForcedComplete`, which breaks immediately and does NOT drain.
// The two arms sit three lines apart:
//
//     Complete(id)       => { if orch.drain_mid_turn_input().await { continue; } … }
//     ForcedComplete(id) => { final_message_id = id; break; }
//
// A shared end-handler that gives every "the turn is finishing" disposition the
// same last drain erases that, and the only visible symptom is a turn the tool
// asked to end carrying on for another model round.
//
// §4 also records that batched has NO late drain at all — its loop ends the turn
// the moment the step reports `Ended`.

/// Counts how many times the loop asked for pending input, and never yields any.
///
/// Counting the ASKS is the point: "did this disposition take the late drain?"
/// is a question about whether the drain ran, and inferring it from the number
/// of model rounds fails when nothing was left to drain anyway.
struct CountingSource(std::sync::atomic::AtomicUsize);

impl CountingSource {
    fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::atomic::AtomicUsize::new(0)))
    }
    fn polls(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait]
impl MidTurnInputSource for CountingSource {
    async fn take_mid_turn_input(&self) -> Option<String> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        None
    }
}

/// A tool that asks the turn to end, via the `_meta` key the port classifies.
struct EndTurnTool;

#[async_trait]
impl Tool for EndTurnTool {
    fn name(&self) -> &str {
        "EndsTurn"
    }
    fn input_schema(&self) -> &serde_json::Value {
        static SCHEMA: once_cell::sync::Lazy<serde_json::Value> =
            once_cell::sync::Lazy::new(|| json!({ "type": "object", "properties": {} }));
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
            data: json!({ "content": "done" }),
            model_content: None,
            new_messages: vec![],
            context_modifier: None,
            is_error: false,
            mcp_meta: Some(json!({"_meta": {"claude/endTurn": true}})),
        })
    }
}

fn streaming_round_calling(tool: &str, id: ToolUseId) -> Vec<llm_runtime::HistoryEvent> {
    vec![
        message_start("m1", "claude-opus-4-7"),
        orchestrator::test_support_stream::content_block_start_tool_use(0, id, tool),
        input_json_delta(0, "{}"),
        content_block_stop(0),
        message_delta_stop("tool_use"),
        message_stop(),
    ]
}

fn text_round(id: &str, text: &str) -> Vec<llm_runtime::HistoryEvent> {
    vec![
        message_start(id, "claude-opus-4-7"),
        content_block_start_text(0),
        text_delta(0, text),
        content_block_stop(0),
        message_delta_stop("end_turn"),
        message_stop(),
    ]
}

/// STREAMING: a tool-requested end does NOT take the late drain.
///
/// Measured by counting how many times the loop ASKS for pending input. A
/// natural end asks once more than a tool-requested one, because `Complete`
/// drains after `emit_end_turn` and `ForcedComplete` breaks straight out.
///
/// The first version of this test asserted a model-round count instead, and a
/// probe that added the drain to `ForcedComplete` left it green: the input had
/// already been taken by the top-of-loop drain, so the extra drain found
/// nothing and changed no rounds. Counting rounds answers "did the turn carry
/// on", which is a different question from "did the drain run".
#[tokio::test]
async fn a_tool_requested_end_does_not_take_the_late_drain() {
    let forced = CountingSource::new();
    {
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![
            streaming_round_calling("EndsTurn", ToolUseId::new()),
            text_round("m2", "should never be requested"),
        ]));
        let mut registry = ToolRegistry::new();
        registry.register_builtin(Arc::new(EndTurnTool));
        let orch = ConversationOrchestrator::new_with_streaming(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(Vec::new())),
            api.clone(),
            Arc::new(registry),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        orch.set_mid_turn_input(forced.clone());
        orch.run_turn_streaming("seed").await.expect("streaming");
        assert_eq!(
            api.captured_calls().await.len(),
            1,
            "a tool-requested end stops at that round"
        );
    }

    let natural = CountingSource::new();
    {
        let api = Arc::new(MockStreamingApiClient::with_turns(vec![text_round(
            "m1", "answer",
        )]));
        let orch = build_orch(api.clone());
        orch.set_mid_turn_input(natural.clone());
        orch.run_turn_streaming("seed").await.expect("streaming");
        assert_eq!(api.captured_calls().await.len(), 1, "one round either way");
    }

    assert!(
        natural.polls() > forced.polls(),
        "the natural end must ask for pending input MORE times than the tool-requested one \
         ({} vs {}): `Complete` drains after emit_end_turn, `ForcedComplete` breaks straight \
         out three lines away. A shared end-handler that drains on every finishing \
         disposition makes these equal, and a turn the tool asked to stop runs on.",
        natural.polls(),
        forced.polls()
    );
}

/// BATCHED has no late drain at all.
///
/// The same pending input that continues a streaming turn strands on the batched
/// path: its loop ends as soon as the step reports `Ended`. §4 records this as a
/// real difference to preserve, not an oversight to harmonise — PR 4 must not
/// hand batched a drain while unifying the end handling.
///
/// Counts polls for the same reason as the test above, plus one of its own: an
/// assertion that batched "does not drain again" is vacuous if batched never
/// drains at all, so the lower bound is asserted too.
#[tokio::test]
async fn the_batched_path_has_no_late_drain() {
    let api = Arc::new(MockApiClient::new(vec![
        orchestrator::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "first answer".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
        orchestrator::test_support::mock_message_response(
            vec![llm_runtime::ContentBlock::Text {
                text: "should never be requested".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let source = CountingSource::new();
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    orch.set_mid_turn_input(source.clone());

    orch.run_turn("seed").await.expect("batched");

    assert_eq!(
        api.captured_msgs().await.len(),
        1,
        "the batched loop ends the turn as soon as the step reports Ended"
    );
    assert!(
        source.polls() >= 1,
        "batched must drain at the TOP of its loop — without that this test proves nothing, \
         because 'it does not drain a second time' is free if it never drains at all"
    );
    assert_eq!(
        source.polls(),
        1,
        "exactly the one top-of-loop drain, and no post-emit drain. Batched has no late-drain \
         arm; giving it one while unifying the end handling would be a behaviour change \
         dressed as unification, and input arriving during the final response would start \
         continuing turns that used to end."
    );
}

// ── §3.5 LoopGuardOrder ──────────────────────────────────────────────────────
//
// The three entries run their top-of-loop guards in three different orders, and
// PR 5 keeps all three behind a `LoopGuardOrder` selector rather than picking
// one:
//
//   Batched            drain → max_turns → budget → increment
//   BatchedCancelable  cancel → max_turns → budget → increment   (NO drain)
//   Streaming          drain → max_turns → budget → increment → cancel
//
// Verified against the source before writing this: `try_run_turn` drains,
// `try_run_turn_cancelable` does not and checks the token first, and the
// streaming loop increments before its cancel guard.
//
// The missing drain is the one difference nothing tested. It is also the easiest
// to erase, because "every loop should drain pending input" reads like a bug fix
// rather than a behaviour change — and a cancelable turn that starts draining
// would pull a queued message into a turn the user is cancelling.

/// `run_turn` drains at the top of its loop; `run_turn_with_cancel` does not.
///
/// One test over both entries because the assertion IS the difference. Separate
/// per-entry tests would both keep passing under a unified guard order that gave
/// the cancelable path a drain — only the comparison notices they became the
/// same.
#[tokio::test]
async fn only_the_non_cancelable_batched_entry_drains_at_the_top_of_its_loop() {
    fn build(source: Arc<CountingSource>) -> ConversationOrchestrator {
        let orch = ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            Arc::new(MockApiClient::new(vec![
                orchestrator::test_support::mock_message_response(
                    vec![llm_runtime::ContentBlock::Text {
                        text: "done".into(),
                        cache_control: None,
                    }],
                    Some("end_turn"),
                ),
            ])),
            Arc::new(ToolRegistry::new()),
            orchestrator::test_support::noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            PathBuf::from("/tmp"),
        );
        orch.set_mid_turn_input(source);
        orch
    }

    let plain = CountingSource::new();
    build(plain.clone())
        .run_turn("seed")
        .await
        .expect("batched");

    let cancelable = CountingSource::new();
    build(cancelable.clone())
        .run_turn_with_cancel("seed", CancellationToken::new())
        .await
        .expect("cancelable");

    assert!(
        plain.polls() >= 1,
        "run_turn must drain pending input at the top of its loop, before max_turns and \
         budget, so a queued message is not dropped by a limit"
    );
    assert_eq!(
        cancelable.polls(),
        0,
        "run_turn_with_cancel must NOT drain: its guard order is cancel → max_turns → budget, \
         with no drain at all. Giving it one reads like a bug fix and is a behaviour change — \
         a turn the user is cancelling would pull in a queued message on its way out."
    );
}
