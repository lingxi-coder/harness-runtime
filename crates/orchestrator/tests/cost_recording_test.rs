//! recorded the response's token usage and computed a non-zero cost.
#![allow(clippy::field_reassign_with_default)]

use cost::pricing::PricingCatalog;
use cost::CostTracker;
use llm_runtime::{ContentBlock, HistoryResponse, TokenUsage, Usage};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use protocol::SessionId;
use std::sync::Arc;
use tokio::sync::mpsc;
use tool_api::registry::ToolRegistry;

/// Build an `HistoryResponse` that emulates a single `end_turn` API reply with
/// the given token usage.
fn end_turn_response_with_usage(input: u64, output: u64) -> HistoryResponse {
    HistoryResponse {
        id: "msg_mock".to_string(),
        model: "claude-opus-4-6".to_string(),
        // Visible text so the response is a genuine end_turn completion — an
        // empty-content end_turn trips the #78 thinking-only nudge (faithful),
        // which would request another turn the single-response mock can't serve.
        content: vec![ContentBlock::Text {
            text: "Done.".to_string(),
            cache_control: None,
        }],
        stop_reason: Some("end_turn".to_string()),
        stop_details: None,
        usage: Usage {
            billable_tokens: TokenUsage {
                input,
                output,
                ..Default::default()
            },
            ..Default::default()
        },
        cost: None,
        provider_metadata: serde_json::Value::Null,
    }
}

#[tokio::test]
async fn turn_with_known_tokens_records_real_cost() {
    let response = end_turn_response_with_usage(1_000, 500);
    let api = Arc::new(MockApiClient::new(vec![response]));

    let (tx, mut rx) = mpsc::channel(8);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));

    let tools = Arc::new(ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into(); // priced in builtin_reference catalog

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
        .with_cost_tracker(tracker.clone()),
    );

    orch.run_turn("hi").await.expect("run_turn ok");

    // Drain the persist channel (CostTracker pushes a snapshot per record).
    let snap = rx.recv().await.expect("persist_tx received a snapshot");
    // 1000*5000 + 500*25000 = 17_500_000 nano-USD = $0.0175 (matches the
    // M3-05 tracker test).
    assert_eq!(snap.total_nano_usd, 17_500_000);
}

#[tokio::test]
async fn turn_records_the_clients_frozen_quote_for_a_conditional_model() {
    let mut response = end_turn_response_with_usage(1_000, 500);
    response.model = "deepseek-flash".into();
    let mut quote = llm_runtime::CostEstimate::unestimated(llm_runtime::PricingModelRef {
        pricing_provider_id: llm_runtime::ProviderId::OpenAICompatible {
            name: "deepseek".into(),
        },
        billing_model: "deepseek-flash".into(),
        request_model: "deepseek-flash".into(),
        display_model: "DeepSeek V4.1 Flash".into(),
    });
    quote.estimated = true;
    quote.total_cost_usd = Some(0.00075);
    response.cost = Some(quote);
    let (tx, mut rx) = mpsc::channel(8);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::empty()),
        tx,
    ));
    let mut cfg = OrchestratorConfig::default();
    cfg.model = "deepseek-flash".into();
    let orch = ConversationOrchestrator::new(
        cfg,
        Arc::new(MockApiClient::new(vec![response])),
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cost_tracker(tracker.clone());
    orch.run_turn("hi").await.expect("run_turn ok");
    let snap = rx.recv().await.expect("quoted response persisted");
    assert_eq!(snap.total_nano_usd, 750_000);
    assert!(snap.unpriced_models.is_empty());
}

/// With an `AnalyticsBus` wired (as the desktop composition root does), a live
/// turn fires `tengu_api_success` per completed API response — 1:1 with
/// claude-code 2.1.195's `logEvent('tengu_api_success', …)`. (The port-only
/// `tengu_cost_recorded` event was dropped under strict parity.) Without a bus
/// (every other test here), the tracker accrues totals but emits no event.
#[tokio::test]
async fn run_turn_emits_tengu_api_success_when_bus_attached() {
    use telemetry::{AnalyticsBus, InMemorySink};

    let response = end_turn_response_with_usage(1_000, 500);
    let api = Arc::new(MockApiClient::new(vec![response]));

    let (tx, _rx) = mpsc::channel(8); // _rx held → persist channel stays open
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));

    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;

    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into(); // priced in builtin_reference

    let orch = Arc::new(
        ConversationOrchestrator::new(
            cfg,
            api,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_cost_tracker(tracker)
        .with_analytics_bus(bus),
    );

    orch.run_turn("hi").await.expect("run_turn ok");

    let events = sink.events().await;
    let names: Vec<&str> = events.iter().map(|e| e.name.as_str()).collect();
    assert!(
        names.contains(&"tengu_api_success"),
        "a live turn with a wired bus must fire tengu_api_success; got {names:?}"
    );
}

/// A non-`end_turn` (looping) reply that records the same $0.0175 (17.5M nano)
/// cost per turn.
fn looping_response_with_usage(input: u64, output: u64) -> HistoryResponse {
    let mut r = end_turn_response_with_usage(input, output);
    r.stop_reason = Some("max_tokens".to_string()); // not end_turn → loop continues
    r
}

fn budget_orch(
    api: Arc<MockApiClient>,
    tracker: Arc<CostTracker>,
    max_budget_nano_usd: Option<u64>,
) -> ConversationOrchestrator {
    let mut cfg = OrchestratorConfig::default();
    cfg.model = "claude-opus-4-6".into(); // priced in builtin_reference
    cfg.max_budget_nano_usd = max_budget_nano_usd;
    ConversationOrchestrator::new(
        cfg,
        api,
        Arc::new(ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
    .with_cost_tracker(tracker)
}

#[tokio::test]
async fn run_turn_stops_at_max_budget() {
    // Turn 1 records $0.0175 (17.5M nano); with a 1M-nano ($0.001) cap, the next
    // iteration's over-budget check stops with MaxBudgetReached — 1:1 with
    // claude-code `getTotalCost() >= maxBudgetUsd`.
    let api = Arc::new(MockApiClient::new(vec![
        looping_response_with_usage(1_000, 500),
        looping_response_with_usage(1_000, 500),
        looping_response_with_usage(1_000, 500),
    ]));
    let (tx, _rx) = mpsc::channel(8); // _rx held for the whole test → channel stays open
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let orch = budget_orch(api, tracker, Some(1_000_000));
    let err = orch.run_turn("hi").await.expect_err("must stop on budget");
    assert!(
        matches!(
            err,
            orchestrator::OrchestratorError::MaxBudgetReached { .. }
        ),
        "expected MaxBudgetReached, got {err:?}"
    );
}

#[tokio::test]
async fn no_budget_cap_never_stops_on_budget() {
    // max_budget_nano_usd = None (default) → the cap is inert; the loop runs to
    // queue exhaustion, never MaxBudgetReached. Post-#10, queue exhaustion is a
    // model/runtime error, so the loop now ends GRACEFULLY (Ok(EndTurn) with
    // reason model_error) rather than bubbling a hard error — either way the
    // result must NOT be MaxBudgetReached.
    let api = Arc::new(MockApiClient::new(vec![
        looping_response_with_usage(1_000, 500),
        looping_response_with_usage(1_000, 500),
    ]));
    let (tx, _rx) = mpsc::channel(8);
    let tracker = Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ));
    let orch = budget_orch(api, tracker, None);
    let outcome = orch.run_turn("hi").await;
    assert!(
        !matches!(
            outcome,
            Err(orchestrator::OrchestratorError::MaxBudgetReached { .. })
        ),
        "no cap must not stop on budget, got {outcome:?}"
    );
}

/// querytracking parity (`query.ts:965`): the `tengu_query_error` event carries
/// `queryChainId` (a uuid) + `queryDepth`. `queryDepth` is always 0 in this port
/// because subagents never run through `ConversationOrchestrator`.
#[tokio::test]
async fn query_error_carries_querytracking_fields() {
    use telemetry::{AnalyticsBus, AnalyticsValue, InMemorySink};
    // Empty mock → first call exhausts (Transport) → #10 graceful model_error →
    // `tengu_query_error` fires.
    let api = Arc::new(MockApiClient::new(vec![]));
    let bus = Arc::new(AnalyticsBus::new());
    let sink = Arc::new(InMemorySink::new());
    bus.attach_sink(sink.clone()).await;
    let orch = Arc::new(
        ConversationOrchestrator::new(
            OrchestratorConfig::default(),
            api,
            Arc::new(ToolRegistry::new()),
            noop_hook_executor(),
            Arc::new(NoOpPermissionGate),
            Arc::new(MockOutputStream::new()),
            Arc::new(StaticMemoryProvider::empty()),
            std::env::temp_dir(),
        )
        .with_analytics_bus(bus),
    );
    let _ = orch.run_turn("hi").await;

    let events = sink.events().await;
    let qe = events
        .iter()
        .find(|e| e.name == "tengu_query_error")
        .expect("tengu_query_error must fire on the model_error path");
    match qe.metadata.get("queryChainId") {
        Some(AnalyticsValue::String(s)) => {
            assert!(!s.is_empty(), "queryChainId must be a non-empty uuid")
        }
        other => panic!("queryChainId must be a String, got {other:?}"),
    }
    assert!(
        matches!(qe.metadata.get("queryDepth"), Some(AnalyticsValue::Int(0))),
        "queryDepth must be 0; got {:?}",
        qe.metadata.get("queryDepth")
    );
}
