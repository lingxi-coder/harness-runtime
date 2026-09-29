#![allow(
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::unused_async
)]

use cost::pricing::PricingCatalog;
use cost::CostTracker;
use llm_runtime::{ContentBlock, ExecutionUsage as Usage, HistoryResponse};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::OrchestratorHandle;
use protocol::SessionId;
use std::sync::Arc;
use tokio::sync::mpsc;
use tool_api::registry::ToolRegistry;

fn end_turn_response_with_usage(input: u64, output: u64) -> HistoryResponse {
    HistoryResponse {
        id: "msg_mock".to_string(),
        model: "claude-opus-4-6".to_string(),
        // Visible text: an empty-content end_turn trips the #78 thinking-only
        // nudge, which would request another turn the single-response mock can't
        // serve.
        content: vec![ContentBlock::Text {
            text: "Done.".to_string(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".to_string()),
        stop_details: None,
        usage: Usage {
            report: llm_runtime::UsageReport::measured(
                llm_runtime::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                    reasoning_tokens: 0,
                    ..Default::default()
                },
                llm_runtime::services::sdk::protocol::UsageState::Complete,
            ),
            ..Default::default()
        },
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

/// Construct an orchestrator pre-loaded with N priced responses (each
/// `input=1000, output=500`) and a fresh `CostTracker`.
async fn make_orch_with_n_responses(
    n: usize,
) -> (
    Arc<ConversationOrchestrator>,
    mpsc::Receiver<cost::CostState>,
    Arc<CostTracker>,
) {
    let responses: Vec<_> = (0..n)
        .map(|_| end_turn_response_with_usage(1_000, 500))
        .collect();
    let api = Arc::new(MockApiClient::new(responses));

    let (tx, rx) = mpsc::channel(64);
    let session_id = SessionId::new();
    let tracker = Arc::new(CostTracker::new(
        session_id,
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));

    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into();

    let orch = Arc::new(
        ConversationOrchestrator::new(
            cfg,
            api,
            tools,
            hooks,
            perms,
            output,
            memory,
            std::env::temp_dir(),
        )
        .with_session_id(session_id)
        .with_cost_tracker(tracker.clone()),
    );
    (orch, rx, tracker)
}

#[tokio::test]
async fn zero_turn_snapshot_is_zeros() {
    let (orch, _rx, _tracker) = make_orch_with_n_responses(0).await;
    let snap = orch.snapshot_cost().await;
    assert_eq!(snap.total_usd, 0.0);
    assert_eq!(snap.input_tokens, 0);
    assert_eq!(snap.output_tokens, 0);
    assert_eq!(snap.api_calls, 0);
}

#[tokio::test]
async fn one_turn_snapshot_is_non_zero() {
    let (orch, _rx, _tracker) = make_orch_with_n_responses(1).await;
    orch.run_turn("hi").await.unwrap();
    let snap = orch.snapshot_cost().await;

    // 17_500_000 nano-USD = $0.0175
    assert!(
        (snap.total_usd - 0.0175).abs() < 1e-9,
        "expected total_usd ≈ 0.0175, got {}",
        snap.total_usd
    );
    assert_eq!(snap.input_tokens, 1_000);
    assert_eq!(snap.output_tokens, 500);
    assert_eq!(snap.api_calls, 1);
    assert_eq!(snap.total_tokens, 1_500);
}

#[tokio::test]
async fn two_turns_accumulate() {
    let (orch, _rx, _tracker) = make_orch_with_n_responses(2).await;
    orch.run_turn("hi").await.unwrap();
    orch.run_turn("hi again").await.unwrap();
    let snap = orch.snapshot_cost().await;

    assert_eq!(snap.input_tokens, 2_000);
    assert_eq!(snap.output_tokens, 1_000);
    assert_eq!(snap.api_calls, 2);
    assert!((snap.total_usd - 0.0350).abs() < 1e-9);
}

#[tokio::test]
async fn clear_session_resets_cost_snapshot() {
    // parity 2.1.212: claude-code's clearConversation calls resetCostState, so
    // `/clear` must zero the session cost counter instead of carrying the prior
    // conversation's accumulated total into the fresh session.
    let (orch, _rx, _tracker) = make_orch_with_n_responses(1).await;
    orch.run_turn("hi").await.unwrap();
    let before = orch.snapshot_cost().await;
    assert!(before.total_nano_usd > 0, "turn accrued cost");
    assert_eq!(before.api_calls, 1);

    <ConversationOrchestrator as OrchestratorHandle>::clear_session(&*orch)
        .await
        .expect("clear session");

    let after = orch.snapshot_cost().await;
    assert_eq!(after.total_nano_usd, 0, "cost total reset by /clear");
    assert_eq!(after.total_usd, 0.0);
    assert_eq!(after.input_tokens, 0);
    assert_eq!(after.output_tokens, 0);
    assert_eq!(after.api_calls, 0, "api-call counter reset by /clear");
    assert!(after.by_model.is_empty(), "per-model breakdown cleared");
}

#[tokio::test]
async fn emit_end_turn_carries_real_cost() {
    // Build a fresh wiring where we can inspect the captured events.
    let response = end_turn_response_with_usage(1_000, 500);
    let api = Arc::new(MockApiClient::new(vec![response]));

    let (tx, _rx) = mpsc::channel(64);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));

    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output_capture = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into();
    let orch = Arc::new(
        ConversationOrchestrator::new(
            cfg,
            api,
            tools,
            hooks,
            perms,
            output_capture.clone(),
            memory,
            std::env::temp_dir(),
        )
        .with_cost_tracker(tracker),
    );
    orch.run_turn("hi").await.unwrap();

    let events = output_capture.snapshot().await;
    let end_turn_cost = events
        .iter()
        .find_map(|e| match e {
            platform_api::OutputEvent::EndTurn { cost, .. } => Some(cost.clone()),
            _ => None,
        })
        .expect("end_turn event present");

    // The cost embedded in emit_end_turn must reflect the real numbers,
    // not the legacy zeros from cost_snapshot_from_session.
    assert!(
        (end_turn_cost.total_usd - 0.0175).abs() < 1e-9,
        "got: {}",
        end_turn_cost.total_usd
    );
    assert_eq!(end_turn_cost.input_tokens, 1_000);
    assert_eq!(end_turn_cost.output_tokens, 500);
    assert_eq!(end_turn_cost.api_calls, 1);
}

#[tokio::test]
async fn restore_session_cost_seeds_the_snapshot_total() {
    // Resume parity: restoring a prior session's cost makes it visible in the
    // very next snapshot, before any new turn runs (the footer shows the
    // running total instead of $0 after `--resume`).
    let (orch, _rx, _tracker) = make_orch_with_n_responses(0).await;
    orch.restore_session_cost(17_500_000).await; // $0.0175 from the prior run
    let snap = orch.snapshot_cost().await;
    assert!(
        (snap.total_usd - 0.0175).abs() < 1e-9,
        "restored total_usd should be 0.0175, got {}",
        snap.total_usd
    );
}

#[tokio::test]
async fn restore_then_run_turn_accumulates_on_top() {
    // A restored session keeps accumulating: the next turn adds to the restored
    // base, not to zero.
    let (orch, _rx, _tracker) = make_orch_with_n_responses(1).await;
    orch.restore_session_cost(17_500_000).await; // prior $0.0175
    orch.run_turn("hi").await.unwrap(); // + this turn's $0.0175
    let snap = orch.snapshot_cost().await;
    assert!(
        (snap.total_usd - 0.0350).abs() < 1e-9,
        "restored + new turn should be 0.0350, got {}",
        snap.total_usd
    );
}

#[tokio::test]
async fn restore_session_cost_without_a_tracker_is_a_noop() {
    // Library users who never wire a tracker: restore must not panic and the
    // snapshot stays zero.
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        std::env::temp_dir(),
    );
    orch.restore_session_cost(17_500_000).await; // no tracker → silently ignored
    assert_eq!(orch.snapshot_cost().await.total_usd, 0.0);
}

#[tokio::test]
async fn no_tracker_returns_zeroed_snapshot() {
    // Backward compat — library users who don't wire a tracker still get
    // a valid (zero) snapshot with the correct session_id.
    let api = Arc::new(MockApiClient::new(Vec::new()));
    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());
    let orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        api,
        tools,
        hooks,
        perms,
        output,
        memory,
        std::env::temp_dir(),
    );
    let snap = orch.snapshot_cost().await;
    assert_eq!(snap.total_usd, 0.0);
    assert_eq!(snap.input_tokens, 0);
    assert_eq!(snap.api_calls, 0);
    assert_eq!(snap.session_id, orch.current_session_id().await);
}

struct SavedCostState(cost::CostState);

#[async_trait::async_trait]
impl cost::CostHydrator for SavedCostState {
    async fn hydrate(
        &self,
        session_id: SessionId,
    ) -> Result<cost::CostHydration, cost::CostPersistError> {
        assert_eq!(session_id, self.0.session_id);
        Ok(cost::CostHydration {
            state: self.0.clone(),
            journal_revision: self.0.cost_revision,
            attempt_outputs: Vec::new(),
        })
    }
}

#[tokio::test]
async fn status_restores_hydrated_totals_before_a_turn_and_keeps_sessions_isolated() {
    let (original, _original_rx, original_tracker) = make_orch_with_n_responses(2).await;
    original.run_turn("first turn").await.unwrap();
    original.run_turn("second turn").await.unwrap();
    let saved = SavedCostState(original_tracker.snapshot().await);
    let saved_id = saved.0.session_id;
    let history = original.conversation_transcript().await;
    drop(original);
    drop(original_tracker);

    // Fresh objects model a restart: hydrate the persisted ledger, then resume
    // the transcript without making any new provider request.
    let (restarted, _restarted_rx, tracker) = make_orch_with_n_responses(0).await;
    tracker
        .switch_session_hydrated(saved_id, &saved)
        .await
        .unwrap();
    restarted
        .resume_session(saved_id, history.clone(), None, None, Default::default())
        .await
        .unwrap();
    let status = restarted.get_status_snapshot().await;
    assert_eq!(status.session_id, saved_id.to_string());
    assert_eq!(status.input_tokens, 2_000);
    assert_eq!(status.output_tokens, 1_000);
    assert!((status.total_cost_usd - 0.0350).abs() < 1e-9);

    let other_id = SessionId::new();
    restarted
        .resume_session(other_id, Vec::new(), None, None, Default::default())
        .await
        .unwrap();
    let other = restarted.get_status_snapshot().await;
    assert_eq!(other.session_id, other_id.to_string());
    assert_eq!(
        other.input_tokens, 0,
        "another session must not inherit paid input"
    );
    assert_eq!(
        other.output_tokens, 0,
        "another session must not inherit output"
    );
    assert_eq!(other.total_cost_usd, 0.0);

    restarted
        .resume_session(saved_id, history, None, None, Default::default())
        .await
        .unwrap();
    let restored = restarted.get_status_snapshot().await;
    assert_eq!(restored.session_id, status.session_id);
    assert_eq!(restored.input_tokens, status.input_tokens);
    assert_eq!(restored.output_tokens, status.output_tokens);
    assert_eq!(restored.total_cost_usd, status.total_cost_usd);
}
