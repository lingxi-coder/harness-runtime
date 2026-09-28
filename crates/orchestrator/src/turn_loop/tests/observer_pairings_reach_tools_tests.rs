use crate::conversation::ConversationOrchestrator;
use crate::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
    StaticMemoryProvider,
};
use crate::turn_loop::dispatch_tool_uses_tracked;
use crate::OrchestratorConfig;
use async_trait::async_trait;
use platform_api::observer_pairing::ObserverPairings;
use protocol::{ContentBlock, ToolUseId};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tool_api::context::ToolUseContext;
use tool_api::registry::ToolRegistry;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};

/// Reports back whether the dispatcher handed it the session's pairing
/// table, and whether it is the SAME allocation the host installed.
struct Probe(Arc<Mutex<Option<Option<Arc<ObserverPairings>>>>>);

#[async_trait]
impl Tool for Probe {
    fn name(&self) -> &str {
        "Probe"
    }
    fn input_schema(&self) -> &Value {
        static SCHEMA: once_cell::sync::Lazy<Value> =
            once_cell::sync::Lazy::new(|| json!({"type":"object"}));
        &SCHEMA
    }
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }
    fn max_result_size_chars(&self) -> usize {
        1024
    }
    async fn check_permissions(
        &self,
        _: &Value,
        _: &ToolUseContext,
    ) -> permission::PermissionResult {
        permission::PermissionResult::Allow {
            reason: permission::PermissionDecisionReason::Other {
                reason: "probe".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: permission::result::PermissionMetadata::default(),
        }
    }
    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        String::new()
    }
    async fn prompt(&self, _: &PromptOptions) -> String {
        String::new()
    }
    async fn call(
        &self,
        _: Value,
        ctx: ToolUseContext,
        _: tool_api::progress::ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        *self.0.lock().unwrap() = Some(ctx.observer_pairings.clone());
        Ok(ToolCallResult::from_data(json!({"ok": true})))
    }
}

async fn dispatch_with(table: Option<Arc<ObserverPairings>>) -> Option<Arc<ObserverPairings>> {
    let seen = Arc::new(Mutex::new(None));
    let mut registry = ToolRegistry::new();
    registry.register_builtin(Arc::new(Probe(seen.clone())) as Arc<dyn Tool>);
    let mut orch = ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(registry),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        PathBuf::from("/tmp"),
    );
    if let Some(table) = table {
        orch = orch.with_observer_pairings(table);
    }
    let uses = vec![(ToolUseId::new(), "Probe".to_string(), json!({}), None)];
    let (blocks, ..) = dispatch_tool_uses_tracked(&orch, &uses, None)
        .await
        .expect("dispatch must succeed");
    assert!(matches!(blocks[0], ContentBlock::ToolResult { .. }));
    let got = seen.lock().unwrap().clone();
    got.expect("the probe must have run")
}

/// The host installs one table; a tool must receive THAT allocation.
///
/// Both dispatch entry points funnel through one `ToolUseContext`
/// construction, so this covers the streaming driver (mobile, desktop) as
/// well as the plain turn loop. Without it, deleting the
/// `observer_pairings:` line from that literal compiles, ships, and leaves
/// `ObserverReport` answering "not armed" forever.
#[tokio::test]
async fn a_tool_receives_the_very_table_the_host_installed() {
    let installed = Arc::new(ObserverPairings::new());
    let received = dispatch_with(Some(installed.clone()))
        .await
        .expect("the table must reach the tool");
    assert!(
        Arc::ptr_eq(&installed, &received),
        "the tool must get the host's table, not a second one"
    );
}

/// A/B twin: a host that installs nothing hands the tool nothing, which is
/// what makes `ObserverReport` refuse rather than invent a pairing. Without
/// this the assertion above would pass against a dispatcher that always
/// manufactured a table.
#[tokio::test]
async fn a_host_without_observers_hands_the_tool_nothing() {
    assert!(dispatch_with(None).await.is_none());
}
