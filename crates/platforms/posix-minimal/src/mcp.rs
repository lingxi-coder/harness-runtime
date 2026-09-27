//! Stub [`McpTransport`] — full stdio / SSE / WebSocket wiring lands in Plan 17.

use async_trait::async_trait;
use futures_util::stream::empty;
use platform_api::mcp::{
    ElicitRequestDto, ElicitResultDto, McpError, McpNotificationStream, McpPromptDto,
    McpRawConnection, McpResourceContentDto, McpResourceDto, McpToolDto, McpToolResultDto,
    McpTransport, McpTransportKind, McpTransportSpec, ServerCapabilitiesDto,
};
use protocol::McpConnectionId;
use serde_json::Value;
use std::collections::HashMap;

/// Stub MCP transport.
#[derive(Default)]
pub struct PosixMcp;

impl PosixMcp {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl McpTransport for PosixMcp {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let kind = match spec {
            McpTransportSpec::Stdio { .. } => McpTransportKind::Stdio,
            McpTransportSpec::Sse { .. } => McpTransportKind::Sse,
            McpTransportSpec::Http { .. } => McpTransportKind::Http,
            McpTransportSpec::WebSocket { .. } => McpTransportKind::WebSocket,
            McpTransportSpec::InProcess { .. } => McpTransportKind::InProcess,
            McpTransportSpec::SseIde { .. } => McpTransportKind::SseIde,
            McpTransportSpec::WsIde { .. } => McpTransportKind::WsIde,
            McpTransportSpec::SdkControl { .. } => McpTransportKind::SdkControl,
        };
        Err(McpError::UnsupportedTransport(kind))
    }

    async fn initialize(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<ServerCapabilitiesDto, McpError> {
        Ok(ServerCapabilitiesDto {
            tools: false,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            experimental: HashMap::new(),
            extensions: HashMap::new(),
        })
    }

    async fn list_tools(&self, _conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        Ok(vec![])
    }

    async fn list_resources(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        Ok(vec![])
    }

    async fn list_prompts(&self, _conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        Ok(vec![])
    }

    async fn call_tool(
        &self,
        _conn: &McpRawConnection,
        tool: &str,
        _input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        Err(McpError::ToolNotFound(tool.to_string()))
    }

    async fn read_resource(
        &self,
        _conn: &McpRawConnection,
        _uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        Err(McpError::Internal("posix-minimal: stub".into()))
    }

    async fn ping(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    async fn notifications(
        &self,
        _conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        Ok(Box::pin(empty()))
    }

    async fn handle_elicitation(
        &self,
        _conn: &McpRawConnection,
        _req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        Err(McpError::Internal("posix-minimal: stub".into()))
    }

    async fn disconnect(&self, _conn_id: McpConnectionId) -> Result<(), McpError> {
        Ok(())
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        // posix-minimal does not actually carry any transport.
        Vec::new()
    }
}

/// The stub transport owns no live `jsonrpc::Connection`s (every `connect`
/// returns `UnsupportedTransport`), so it always hands back `None`. Providing
/// the impl lets `engine-desktop` construct the registry via
/// `McpRegistry::with_raw_conn` and exercise the production client-bridge path;
/// because no real connection ever exists, `get_client` correctly stays empty.
impl mcp::RawConnectionProvider for PosixMcp {
    fn connection_for(&self, _id: McpConnectionId) -> Option<std::sync::Arc<jsonrpc::Connection>> {
        None
    }
}
