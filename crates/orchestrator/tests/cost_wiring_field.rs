//! and wired correctly.

use cost::pricing::PricingCatalog;
use cost::CostTracker;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use protocol::SessionId;
use std::sync::Arc;
use tokio::sync::mpsc;
use tool_api::registry::ToolRegistry;

fn make_tracker() -> Arc<CostTracker> {
    let (tx, _rx) = mpsc::channel(8);
    Arc::new(CostTracker::new(
        SessionId::new(),
        Arc::new(PricingCatalog::builtin_reference()),
        tx,
    ))
}

#[tokio::test]
async fn with_cost_tracker_builder_stores_tracker() {
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
    )
    .with_cost_tracker(make_tracker());

    assert!(
        orch.has_cost_tracker(),
        "with_cost_tracker did not store the tracker"
    );
}

#[tokio::test]
async fn fresh_orchestrator_has_no_cost_tracker() {
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

    assert!(
        !orch.has_cost_tracker(),
        "default orchestrator must not carry a tracker"
    );
}
