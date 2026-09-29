//! §3.2, the late-cancel rows: a token that fires AFTER the model step already
//! succeeded.
//!
//! The two entries answer differently, and §3.2 records both:
//!
//!   Batched: 成功返回后晚到取消 — 不追加一次 token 检查，保留已完成的
//!            EndTurn，不覆盖成 Cancelled
//!   Streaming: 成功返回后晚到取消 — 主 streaming 包装可将成功 EndTurn /
//!              StopHookPrevented 映射为 Cancelled
//!
//! §8's risk table states the direction of the danger outright: "Streaming 包装
//! 把晚到取消映射为 Cancelled，batched 没有 … 禁止在 batched 最终 return 探测
//! token." PR 5 unifies the entry wrappers, and a single shared "was the token
//! cancelled?" check at the end is the obvious way to write one. It would hand
//! batched a mapping it has never had, and the symptom is a turn that finished
//! its work being reported as cancelled — the work is done, persisted, and the
//! caller is told it did not happen.
//!
//! "Late" is made deterministic rather than raced: the provider cancels the
//! token as a side effect and THEN returns a successful response, so by the time
//! the step resolves the token is already cancelled. No sleeps, no ordering
//! assumptions.

use async_trait::async_trait;
use llm_runtime::{ContentBlock as LlmContentBlock, HistoryResponse, LlmError};
use orchestrator::test_support::{
    mock_message_response, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, OrchestratorApiClient, OrchestratorConfig, TurnOutcome,
};
use protocol::ConversationMessage;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::CancellationToken as Token;
use tool_api::registry::ToolRegistry;

/// A provider that cancels the turn's token and then answers successfully.
///
/// This is what makes the test deterministic: the cancellation is observable
/// before the model step resolves, so the step completing and the token being
/// cancelled are not racing.
struct CancelsThenSucceeds {
    token: Token,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl OrchestratorApiClient for CancelsThenSucceeds {
    async fn messages_create(
        &self,
        _model: &str,
        _profile: Option<&str>,
        _system: Option<&str>,
        _msgs: Vec<ConversationMessage>,
        _tools: Vec<serde_json::Value>,
    ) -> Result<HistoryResponse, LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        // Fire the cancel BEFORE answering, so the step resolves into an
        // already-cancelled turn.
        self.token.cancel();
        Ok(mock_message_response(
            vec![LlmContentBlock::Text {
                text: "finished the work".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ))
    }
}

/// BATCHED: a cancel that arrives after the step succeeded does NOT rewrite the
/// outcome.
///
/// `try_run_turn_cancelable` races the step against the token, and when the step
/// wins it proceeds to the disposition without consulting the token again. The
/// turn produced an answer and persisted it; reporting `Cancelled` would tell
/// the caller that work never happened.
#[test]
fn a_cancel_arriving_after_the_batched_step_succeeded_keeps_the_end_turn() {
    let handle = std::thread::Builder::new()
        .name("late-cancel-mapping".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    let token = CancellationToken::new();
                    let api = Arc::new(CancelsThenSucceeds {
                        token: token.clone(),
                        calls: std::sync::atomic::AtomicUsize::new(0),
                    });
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

                    let outcome = orch
                        .run_turn_with_cancel("ping", token.clone())
                        .await
                        .expect("turn");

                    assert!(
                        token.is_cancelled(),
                        "the fixture must actually have cancelled the token, or this test \
                         proves nothing"
                    );
                    assert_eq!(
                        api.calls.load(std::sync::atomic::Ordering::SeqCst),
                        1,
                        "exactly one model step, and it succeeded"
                    );
                    assert_eq!(
                        outcome,
                        TurnOutcome::EndTurn,
                        "a cancel that lands after the step succeeded must NOT rewrite the \
                         outcome. §3.2: batched adds no final token probe, and §8 forbids \
                         giving it one. A shared entry wrapper that ends with 'was the token \
                         cancelled?' reports a finished, persisted turn as Cancelled."
                    );

                    // And the answer really did land, which is what makes the
                    // wrong mapping a lie rather than a cosmetic difference.
                    let session = orch.session();
                    let history = session.lock().await.history.clone();
                    assert!(
                        history
                            .iter()
                            .any(|m| m.text_content().contains("finished the work")),
                        "the assistant's answer must be in history — the turn did its work"
                    );
                });
        })
        .expect("spawn");
    handle.join().expect("test thread panicked");
}

// ── §3.2 row 3: the entry pre-cancel emits NO end event ──────────────────────
//
// `run_turn_streaming_inputs_locked`'s pre-cancel check aborts the startup
// prewarm and closes the Responses websocket, then returns without entering the
// driver: "不进入 driver，不发 `aborted_*` 结束事件".
//
// Every existing cancel test asserts only the OUTCOME (`Cancelled`), and the
// outcome is the one thing every cancel stage agrees on. What separates the
// stages is their side effects — §3.2 gives each row its own column for them —
// and a unified driver that ends every terminal through one exit would emit an
// `aborted_*` here, where today nothing is emitted at all. Outcome-only
// assertions cannot see that.
//
// The normal completion in the same test is the control: it proves the fixture
// CAN observe an end event, so "no event" is a real absence rather than a
// blind instrument.

use orchestrator::scripted;
use orchestrator::test_support::{
    content_block_start_text, content_block_stop, message_delta_stop, message_start, message_stop,
    text_delta, MockStreamingApiClient,
};
use platform_api::OutputEvent;

fn end_events(events: &[OutputEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            OutputEvent::EndTurn { stop_reason, .. } => Some(stop_reason.clone()),
            _ => None,
        })
        .collect()
}

/// A pre-cancelled streaming turn emits no end event; a normal one emits
/// `end_turn`.
#[test]
fn a_pre_cancelled_streaming_turn_emits_no_end_event() {
    let handle = std::thread::Builder::new()
        .name("pre-cancel-no-end-event".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    fn round() -> Vec<llm_runtime::HistoryEvent> {
                        scripted![
                            message_start("m1", "claude-opus-4-7"),
                            content_block_start_text(0),
                            text_delta(0, "answer"),
                            content_block_stop(0),
                            message_delta_stop("end_turn"),
                            message_stop(),
                        ]
                    }
                    fn build(out: Arc<MockOutputStream>) -> ConversationOrchestrator {
                        ConversationOrchestrator::new_with_streaming(
                            OrchestratorConfig::default(),
                            Arc::new(orchestrator::test_support::MockApiClient::new(Vec::new())),
                            Arc::new(MockStreamingApiClient::with_turns(vec![round()])),
                            Arc::new(ToolRegistry::new()),
                            orchestrator::test_support::noop_hook_executor(),
                            Arc::new(NoOpPermissionGate),
                            out,
                            Arc::new(StaticMemoryProvider::empty()),
                            PathBuf::from("/tmp"),
                        )
                    }

                    // Control: a turn that completes normally.
                    let normal_out = Arc::new(MockOutputStream::new());
                    let normal = build(normal_out.clone());
                    normal
                        .run_turn_streaming_with_cancel("hi", CancellationToken::new())
                        .await
                        .expect("normal");
                    let normal_events = end_events(&normal_out.snapshot().await);

                    // Pre-cancelled: the entry check fires before the driver.
                    let cancelled_out = Arc::new(MockOutputStream::new());
                    let cancelled = build(cancelled_out.clone());
                    let token = CancellationToken::new();
                    token.cancel();
                    let outcome = cancelled
                        .run_turn_streaming_with_cancel("hi", token)
                        .await
                        .expect("cancelled");
                    let cancelled_events = end_events(&cancelled_out.snapshot().await);

                    assert_eq!(outcome, TurnOutcome::Cancelled);
                    assert_eq!(
                        normal_events,
                        vec!["end_turn".to_string()],
                        "the control must observe an end event, or the absence asserted below \
                         proves nothing about the code"
                    );
                    assert!(
                        cancelled_events.is_empty(),
                        "a pre-cancelled turn must emit NO end event — §3.2: 不进入 driver，\
                         不发 aborted_* 结束事件. Got {cancelled_events:?}. A shared exit that \
                         ends every terminal with an event would emit one here, and every \
                         existing cancel test — all of which assert only the outcome — would \
                         stay green."
                    );
                });
        })
        .expect("spawn");
    handle.join().expect("test thread panicked");
}

// ── §3.5 cancel-before-max_turns: NOT deterministically testable ────────────
//
// Attempted and removed rather than shipped. The two guards can only both be
// true from the second loop iteration onward — at the first top `turn_count` is
// 0, so `0 >= max_turns` is false for any real limit. Reaching a second top with
// the token already set needs the cancel to land BETWEEN rounds, and on this
// path there is no such window: the `select!` races the whole step, so a cancel
// arriving during one is taken there, and a cancel arriving before the loop is
// the pre-cancel case where `max_turns` cannot also trip.
//
// The fixture that looked like it worked — a tool that cancels the token, with
// `max_turns: 1` — ends in iteration ONE (measured: one API call), so the
// max-turns guard never runs and the assertion about cancel beating it was
// vacuous. Both cancel branches were probed and neither mattered, which is what
// exposed it.
//
// Left here as a note rather than a test, because a test whose premise is false
// is worse than no test: it reports coverage of an ordering nothing checks.
