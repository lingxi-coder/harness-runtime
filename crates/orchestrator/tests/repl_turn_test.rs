//! `current_should_exit`.
use lingxi_core::host::OrchestratorHandle;
use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig, TurnOutcome};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

fn build_orch_with_response(
    response: llm_runtime::HistoryResponse,
) -> (
    ConversationOrchestrator,
    Arc<MockApiClient>,
    Arc<MockOutputStream>,
) {
    let api = Arc::new(MockApiClient::new(vec![response]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    (orch, api, output)
}

#[tokio::test]
async fn run_turn_with_cancel_returns_end_turn_on_normal_completion() {
    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "Hi!".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let (orch, api, _output) = build_orch_with_response(resp);
    let token = CancellationToken::new();
    let outcome = orch
        .run_turn_with_cancel("hello", token)
        .await
        .expect("turn ok");
    assert_eq!(outcome, TurnOutcome::EndTurn);
    // Mock should have received exactly one API call.
    assert_eq!(api.captured_msgs().await.len(), 1);
}

#[tokio::test]
async fn run_turn_with_cancel_returns_cancelled_when_token_fires_before_turn() {
    let resp = mock_message_response(
        vec![LlmContentBlock::Text {
            text: "Should not see this".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    );
    let (orch, api, _output) = build_orch_with_response(resp);

    // Cancel the token immediately before calling run_turn_with_cancel.
    let token = CancellationToken::new();
    token.cancel();

    let outcome = orch
        .run_turn_with_cancel("hello", token)
        .await
        .expect("turn ok");
    assert_eq!(outcome, TurnOutcome::Cancelled);
    // No API calls should have been made because we cancelled before the loop body.
    assert_eq!(api.captured_msgs().await.len(), 0);
}

/// #2 (main-loop parity): the cancelable REPL driver must be recovery-aware,
/// like the non-cancelable [`ConversationOrchestrator::run_turn`] batched path.
/// A `max_tokens` stop_reason must trigger the A1 multi-turn recovery nudge
/// (a byte-exact "resume directly" user message appended before the next call),
/// NOT the legacy no-recovery shim which continued WITHOUT injecting the nudge.
#[tokio::test]
async fn run_turn_with_cancel_injects_max_tokens_recovery_nudge() {
    let api = Arc::new(MockApiClient::new(vec![
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "partial".into(),
                cache_control: None,
            }],
            Some("max_tokens"),
        ),
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "done".into(),
                cache_control: None,
            }],
            Some("end_turn"),
        ),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api.clone(),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        orchestrator::test_support::noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );
    let token = CancellationToken::new();
    let outcome = orch
        .run_turn_with_cancel("hello", token)
        .await
        .expect("turn ok");
    assert_eq!(outcome, TurnOutcome::EndTurn);

    let calls = api.captured_msgs().await;
    assert_eq!(
        calls.len(),
        2,
        "max_tokens should trigger a recovery continuation (2 API calls)"
    );
    // The 2nd call must carry the byte-exact A1 recovery nudge appended to
    // history after the `max_tokens` response. The legacy no-recovery shim
    // continued WITHOUT injecting the nudge, so this asserts the recovery path.
    let second = format!("{:?}", calls[1]);
    assert!(
        second.contains("Output token limit hit. Resume directly"),
        "expected the A1 max_output_tokens recovery nudge in the 2nd call's messages, got: {second}"
    );
}

#[tokio::test]
async fn current_should_exit_returns_false_by_default() {
    let resp = mock_message_response(vec![], Some("end_turn"));
    let (orch, _, _) = build_orch_with_response(resp);
    assert!(!orch.current_should_exit());
}

#[tokio::test]
async fn current_should_exit_returns_true_after_request_exit() {
    let resp = mock_message_response(vec![], Some("end_turn"));
    let (orch, _, _) = build_orch_with_response(resp);
    // Call through OrchestratorHandle trait.
    let handle: Arc<dyn OrchestratorHandle> =
        Arc::new(orchestrator::test_support::MockOrchestratorHandle::default());
    handle.request_exit().await;
    assert!(handle.current_should_exit().await);

    // Also test directly on ConversationOrchestrator.
    OrchestratorHandle::request_exit(&orch).await;
    assert!(orch.current_should_exit());
    // And via the trait method:
    assert!(OrchestratorHandle::current_should_exit(&orch).await);
}

#[tokio::test]
async fn turn_outcome_variants_are_eq_comparable() {
    assert_eq!(TurnOutcome::EndTurn, TurnOutcome::EndTurn);
    assert_eq!(TurnOutcome::MaxTurns, TurnOutcome::MaxTurns);
    assert_eq!(TurnOutcome::Cancelled, TurnOutcome::Cancelled);
    assert_ne!(TurnOutcome::EndTurn, TurnOutcome::Cancelled);
    assert_ne!(TurnOutcome::MaxTurns, TurnOutcome::EndTurn);
}
