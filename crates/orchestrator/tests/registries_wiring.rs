//! builders attach the three optional registries onto
//! [`orchestrator::ConversationOrchestrator`].

use agent::AgentDefinition;
use async_trait::async_trait;
use hooks::HookRegistry;
use mcp::McpRegistry;
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
    McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use protocol::McpConnectionId as ConnId;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

struct StubTransport;

#[async_trait]
impl McpTransport for StubTransport {
    async fn connect(&self, _s: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        unreachable!()
    }
    async fn initialize(&self, _c: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        unreachable!()
    }
    async fn list_tools(&self, _c: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        unreachable!()
    }
    async fn list_resources(&self, _c: &McpRawConnection) -> Result<Vec<McpResourceDto>, McpError> {
        unreachable!()
    }
    async fn list_prompts(&self, _c: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        unreachable!()
    }
    async fn call_tool(
        &self,
        _c: &McpRawConnection,
        _t: &str,
        _i: Value,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!()
    }
    async fn read_resource(
        &self,
        _c: &McpRawConnection,
        _u: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!()
    }
    async fn ping(&self, _id: ConnId) -> Result<(), McpError> {
        unreachable!()
    }
    async fn notifications(
        &self,
        _c: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        unreachable!()
    }
    async fn handle_elicitation(
        &self,
        _c: &McpRawConnection,
        _r: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        unreachable!()
    }
    async fn disconnect(&self, _id: ConnId) -> Result<(), McpError> {
        unreachable!()
    }
    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Stdio]
    }
}

#[tokio::test]
async fn with_mcp_hook_agent_builders_store_fields() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
    let hooks = noop_hook_executor();
    let perms = Arc::new(NoOpPermissionGate);
    let output = Arc::new(MockOutputStream::new());
    let memory = Arc::new(StaticMemoryProvider::empty());

    let mcp = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
    let hook_reg = Arc::new(RwLock::new(HookRegistry::new()));
    let agents = Arc::new(RwLock::new(Vec::<AgentDefinition>::new()));

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
    .with_mcp_registry(mcp.clone())
    .with_hook_registry(hook_reg.clone())
    .with_agent_catalog(agents.clone());

    assert!(orch.has_mcp_registry());
    assert!(orch.has_hook_registry());
    assert!(orch.has_agent_catalog());
}

#[tokio::test]
async fn default_orchestrator_has_no_registries() {
    let api = Arc::new(MockApiClient::new(vec![]));
    let tools = Arc::new(tool_api::registry::ToolRegistry::new());
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

    assert!(!orch.has_mcp_registry());
    assert!(!orch.has_hook_registry());
    assert!(!orch.has_agent_catalog());
}
