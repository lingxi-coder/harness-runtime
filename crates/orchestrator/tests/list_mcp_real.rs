use async_trait::async_trait;
use mcp::{ConfigScope, McpRegistry, McpServerConfig};
use orchestrator::test_support::{
    noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate, StaticMemoryProvider,
};
use orchestrator::{ConversationOrchestrator, OrchestratorConfig};
use platform_api::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpStatus, McpToolDto,
    McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec, OrchestratorHandle,
    ServerCapabilitiesDto,
};
use protocol::McpConnectionId as ConnId;
use serde_json::Value;
use std::sync::Arc;

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

fn stdio_cfg(name: &str) -> McpServerConfig {
    McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Stdio {
            command: "echo".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
        },
        scope: ConfigScope::Settings(protocol::SettingsScope::Project),
        disabled: false,
        timeout_ms: None,
        always_load: false,
        discovery_cache: None,
        tools: Vec::new(),
        tool_permissions: std::collections::BTreeMap::new(),
        config_error: None,
        metadata: Default::default(),
    }
}

fn build_orch() -> ConversationOrchestrator {
    ConversationOrchestrator::new(
        OrchestratorConfig::default(),
        Arc::new(MockApiClient::new(vec![])),
        Arc::new(tool_api::registry::ToolRegistry::new()),
        noop_hook_executor(),
        Arc::new(NoOpPermissionGate),
        Arc::new(MockOutputStream::new()),
        Arc::new(StaticMemoryProvider::empty()),
        std::env::temp_dir(),
    )
}

#[tokio::test]
async fn list_mcp_servers_returns_empty_when_no_registry() {
    let orch = Arc::new(build_orch());
    let v = orch.list_mcp_servers().await;
    assert!(v.is_empty());
}

#[tokio::test]
async fn list_mcp_servers_returns_two_when_two_registered() {
    let reg = Arc::new(McpRegistry::new(Arc::new(StubTransport)));
    {
        let mut c = reg.connections.write().await;
        c.insert(
            "memory".into(),
            mcp::McpConnectionState::Disconnected {
                config: stdio_cfg("memory"),
                last_error: None,
            },
        );
        c.insert(
            "filesystem".into(),
            mcp::McpConnectionState::Disconnected {
                config: stdio_cfg("filesystem"),
                last_error: None,
            },
        );
    }
    let orch = Arc::new(build_orch().with_mcp_registry(reg));
    let v = orch.list_mcp_servers().await;
    assert_eq!(v.len(), 2);
    // Sorted by name.
    assert_eq!(v[0].name, "filesystem");
    assert_eq!(v[0].status, McpStatus::Disconnected);
    assert_eq!(v[0].transport, "stdio");
    assert_eq!(v[1].name, "memory");
}
