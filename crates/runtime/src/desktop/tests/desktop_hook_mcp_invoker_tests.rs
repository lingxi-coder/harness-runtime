use super::{
    hook_mcp_full_name, map_hook_mcp_tool_error, map_hook_mcp_tool_result, DesktopHookMcpInvoker,
};
use hooks::HookMcpInvoker;
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError, McpHeaders,
    McpNotificationStream, McpProtocolEra, McpRawConnection, McpResourceContentDto, McpResourceDto,
    McpResourceTemplateDto, McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec,
    ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
struct NoopTransport {
    connect_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl McpTransport for NoopTransport {
    async fn connect(&self, _spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        self.connect_calls.fetch_add(1, Ordering::SeqCst);
        Err(McpError::Connection("unexpected connect".into()))
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        _options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        let conn = self.connect(spec).await?;
        let capabilities = self.initialize(&conn).await?;
        Ok(McpConnectResult {
            connection: conn,
            capabilities,
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
        })
    }

    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }

    async fn list_tools(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpToolDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_resource_templates(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        Ok(Vec::new())
    }

    async fn list_prompts(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<lingxi_core::host::McpPromptDto>, McpError> {
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        _tool: &str,
        _input: serde_json::Value,
    ) -> Result<McpToolResultDto, McpError> {
        unreachable!("adapter no-lazy-dial test never calls transport tools")
    }

    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        unreachable!("unused")
    }

    async fn ping(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        Err(McpError::Connection("unused".into()))
    }

    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Ok(ElicitResultDto {
            data: serde_json::json!({ "action": "cancel" }),
        })
    }

    async fn disconnect(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Http]
    }
}

fn http_cfg(name: &str) -> mcp::McpServerConfig {
    mcp::McpServerConfig {
        name: name.into(),
        spec: McpTransportSpec::Http {
            url: "https://mcp.example.com".into(),
            headers: McpHeaders::default(),
            headers_helper: None,
            oauth: None,
        },
        scope: mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        disabled: false,
        timeout_ms: None,
        discovery_cache: None,
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        config_error: None,
        metadata: Default::default(),
    }
}

#[test]
fn hook_full_name_normalizes_server_and_respects_existing_prefix() {
    assert_eq!(
        hook_mcp_full_name("claude.ai Linear", "search"),
        format!(
            "mcp__{}__search",
            mcp::normalization::normalize_name_for_mcp("claude.ai Linear")
        )
    );
    assert_eq!(
        hook_mcp_full_name("ignored", "mcp__docs__read"),
        "mcp__docs__read"
    );
}

#[test]
fn hook_result_mapping_extracts_text_and_preserves_is_error() {
    let success = map_hook_mcp_tool_result(McpToolResultDto {
        content: serde_json::json!([
            {"type":"text","text":"first"},
            "second",
            {"ignored": true},
            [{"type":"text","text":"third"}]
        ]),
        is_error: false,
        meta: None,
        structured_content: None,
    });
    assert_eq!(
        success,
        hooks::HookMcpInvocationResult::Success {
            text_content: vec!["first".into(), "second".into(), "third".into()],
        }
    );

    let is_error = map_hook_mcp_tool_result(McpToolResultDto {
        content: serde_json::json!("boom"),
        is_error: true,
        meta: None,
        structured_content: None,
    });
    assert_eq!(
        is_error,
        hooks::HookMcpInvocationResult::Error {
            text_content: vec!["boom".into()],
            message: "boom".into(),
        }
    );
}

#[test]
fn hook_error_mapping_treats_timeouts_separately() {
    assert_eq!(
        map_hook_mcp_tool_error(mcp::McpClientError::Timeout {
            server: "srv".into(),
            tool: "read".into(),
            secs: 3,
        }),
        hooks::HookMcpInvocationResult::Timeout {
            text_content: Vec::new(),
        }
    );
    assert_eq!(
        map_hook_mcp_tool_error(mcp::McpClientError::IdleTimeout {
            server: "srv".into(),
            tool: "read".into(),
            secs: 3,
        }),
        hooks::HookMcpInvocationResult::Timeout {
            text_content: Vec::new(),
        }
    );
    assert_eq!(
        map_hook_mcp_tool_error(mcp::McpClientError::Rpc("bad".into())),
        hooks::HookMcpInvocationResult::Error {
            text_content: Vec::new(),
            message: "JSON-RPC error: bad".into(),
        }
    );
}

#[tokio::test]
async fn invoker_is_late_bound_and_shared_across_clones() {
    let invoker = DesktopHookMcpInvoker::default();
    let first = invoker
        .invoke(hooks::HookMcpInvocation {
            server: "srv".into(),
            tool: "tool".into(),
            input: HashMap::new(),
            timeout: Duration::from_secs(1),
        })
        .await;
    assert_eq!(
        first,
        hooks::HookMcpInvocationResult::NotConnected {
            message: "MCP registry is not ready".into(),
        }
    );

    let transport = Arc::new(NoopTransport::default());
    let registry = Arc::new(mcp::McpRegistry::new(transport));
    let clone = invoker.clone();
    clone.bind(registry.clone());
    assert_eq!(
        Arc::strong_count(&registry),
        1,
        "the invoker must not retain the session registry"
    );

    let after_bind = invoker
        .invoke(hooks::HookMcpInvocation {
            server: "srv".into(),
            tool: "tool".into(),
            input: HashMap::new(),
            timeout: Duration::from_secs(1),
        })
        .await;
    assert_eq!(
        after_bind,
        hooks::HookMcpInvocationResult::NotConnected {
            message: "MCP server \"srv\" is not connected".into(),
        }
    );
}

#[tokio::test]
async fn invoker_uses_get_client_and_does_not_lazy_connect_cached_servers() {
    let transport = Arc::new(NoopTransport::default());
    let registry = Arc::new(mcp::McpRegistry::new(
        transport.clone() as Arc<dyn McpTransport>
    ));
    registry.connections.write().await.insert(
        "srv".into(),
        mcp::connection::McpConnectionState::Cached {
            config: http_cfg("srv"),
            connection_id: McpConnectionId::new(),
            capabilities: ServerCapabilitiesDto {
                tools: true,
                resources: false,
                prompts: false,
                logging: false,
                directory_read: false,
                experimental: HashMap::new(),
                extensions: HashMap::new(),
            },
            negotiated: lingxi_core::host::McpNegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
            tools: Vec::new(),
            resources: Vec::new(),
            resource_templates: Vec::new(),
            prompts: Vec::new(),
            cache_saved_at_ms: 1,
            age_ms: 1,
        },
    );

    let invoker = DesktopHookMcpInvoker::default();
    invoker.bind(registry.clone());
    let result = invoker
        .invoke(hooks::HookMcpInvocation {
            server: "srv".into(),
            tool: "read".into(),
            input: HashMap::new(),
            timeout: Duration::from_secs(1),
        })
        .await;

    assert_eq!(
        result,
        hooks::HookMcpInvocationResult::NotConnected {
            message: "MCP server \"srv\" is not connected".into(),
        }
    );
    assert_eq!(
        transport.connect_calls.load(Ordering::SeqCst),
        0,
        "hook invoker must not trigger lazy dial; it only uses get_client"
    );
}

#[test]
fn invoker_preserves_caller_supplied_prefixed_full_name() {
    assert_eq!(
        hook_mcp_full_name("claude.ai Linear", "mcp__custom_server__search"),
        "mcp__custom_server__search"
    );
}
