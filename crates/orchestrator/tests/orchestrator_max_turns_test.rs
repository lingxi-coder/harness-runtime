//! Conversation execution enforces its configured turn limit.

use llm_runtime::ContentBlock as LlmContentBlock;
use orchestrator::test_support::{
    mock_message_response, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use orchestrator::{
    ConversationOrchestrator, ConversationOutcome, OrchestratorConfig, OrchestratorError,
};
use std::sync::Arc;
use tool_api::registry::ToolRegistry;

#[tokio::test]
async fn never_ending_loop_aborts_with_max_turns_reached() {
    let make_resp = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "still thinking".into(),
                cache_control: None,
            }],
            Some("max_tokens"), // any value other than "end_turn"
        )
    };
    let api = Arc::new(MockApiClient::new(vec![
        make_resp(),
        make_resp(),
        make_resp(),
        make_resp(),
        make_resp(),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let config = OrchestratorConfig {
        max_turns: 3,
        ..OrchestratorConfig::default()
    };

    let orch = ConversationOrchestrator::new(
        config,
        api.clone(),
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let err = orch.run_turn("forever").await.expect_err("must fail");
    assert!(matches!(
        err,
        OrchestratorError::MaxTurnsReached { max_turns: 3 }
    ));
    assert_eq!(err.to_string(), "Reached maximum number of turns (3)");

    // API was called exactly max_turns (= 3) times.
    assert_eq!(api.captured_msgs().await.len(), 3);
}

#[tokio::test]
async fn max_turns_zero_means_unbounded() {
    // Parity: `max_turns = 0` (the new default) means UNBOUNDED — the loop is
    // NOT hard-capped, mirroring claude-code's optional `maxTurns`
    // (`if (maxTurns && nextTurnCount > maxTurns)`). This is the contrast to
    // `never_ending_loop_aborts_with_max_turns_reached` (which caps at 3): the
    // same looping responses now run to queue-exhaustion, never MaxTurnsReached.
    let make_resp = || {
        mock_message_response(
            vec![LlmContentBlock::Text {
                text: "still thinking".into(),
                cache_control: None,
            }],
            Some("max_tokens"),
        )
    };
    let api = Arc::new(MockApiClient::new(vec![
        make_resp(),
        make_resp(),
        make_resp(),
    ]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let config = OrchestratorConfig {
        max_turns: 0, // == OrchestratorConfig::default().max_turns — unbounded
        ..OrchestratorConfig::default()
    };
    let orch = ConversationOrchestrator::new(
        config,
        api.clone(),
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("forever").await;
    assert!(
        !matches!(outcome, Err(OrchestratorError::MaxTurnsReached { .. })),
        "max_turns = 0 must be UNBOUNDED (never MaxTurnsReached), got: {outcome:?}"
    );
    // No turn cap stopped the loop early — it consumed every queued response
    // (the queue is fully drained). Post-#10, the queue-exhaustion error (a
    // model/runtime error) ends the turn GRACEFULLY as `model_error` rather than
    // bubbling, but the queue is still fully drained first. (With the old hard
    // cap, `0 >= 0` would have hit MaxTurnsReached on turn 0.)
    assert_eq!(
        api.remaining().await,
        0,
        "unbounded loop drains the whole response queue"
    );
}

#[tokio::test]
async fn default_config_single_end_turn_completes_cleanly() {
    // Visible text: an empty-content end_turn trips the #78 thinking-only nudge
    // (faithful), which requests another turn the single-response mock can't serve.
    let api = Arc::new(MockApiClient::new(vec![mock_message_response(
        vec![LlmContentBlock::Text {
            text: "Done.".into(),
            cache_control: None,
        }],
        Some("end_turn"),
    )]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch.run_turn("ping").await.expect("happy");
    assert!(matches!(
        outcome,
        ConversationOutcome::EndTurn { turn_count: 1, .. }
    ));
}

#[tokio::test]
async fn api_error_ends_turn_gracefully_as_model_error() {
    // #10 (batched twin, faithful port of `query.ts:955-997` catch): a generic
    // model/runtime API error (here the empty-mock exhaustion shape, a
    // `Transport` error — NOT a RateLimited/Overloaded carve-out) is no longer
    // bubbled as a hard `OrchestratorError`. The turn ends GRACEFULLY with
    // `reason:"model_error"` and the raw error text surfaced as an api-error
    // assistant message.
    let api = Arc::new(MockApiClient::new(vec![]));
    let output = Arc::new(MockOutputStream::new());
    let hooks = orchestrator::test_support::noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let tools = Arc::new(ToolRegistry::new());

    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output.clone(),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    );

    let outcome = orch
        .run_turn("anything")
        .await
        .expect("a generic API error must end the turn gracefully, not bubble");
    assert!(
        matches!(outcome, ConversationOutcome::EndTurn { .. }),
        "{outcome:?}"
    );
    let events = output.snapshot().await;
    assert!(
        events.iter().any(|e| matches!(
            e,
            lingxi_core::host::OutputEvent::EndTurn { stop_reason, .. } if stop_reason == "model_error"
        )),
        "turn must end with stop_reason model_error; events={events:#?}"
    );
    // The raw error text (the mock exhaustion message) is surfaced verbatim.
    assert!(
        output
            .text_events()
            .await
            .iter()
            .any(|t| t.contains("mock script exhausted")),
        "the raw error text must be surfaced as the model_error message"
    );
}
