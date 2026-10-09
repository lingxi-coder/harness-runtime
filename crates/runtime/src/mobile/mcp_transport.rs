//! Mobile MCP transport.
//!
//! Mobile reaches MCP servers over the network only (SSE and streamable HTTP):
//! there is no process to spawn and no in-process server. Every other transport
//! kind is refused at connect time instead of being passed to a transport that
//! would reject it later with a less specific error.

use async_trait::async_trait;
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError,
    McpNotificationStream, McpPromptDto, McpRawConnection, McpResourceContentDto, McpResourceDto,
    McpResourceTemplateDto, McpToolDto, McpToolResultDto, McpTransport, McpTransportKind,
    McpTransportSpec, ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use platform_common::RemoteMcpTransport;
use serde_json::Value;
use std::sync::Arc;

/// The mobile MCP transport: remote (SSE / HTTP) servers only.
pub(crate) struct MobileMcpTransport {
    remote: Arc<RemoteMcpTransport>,
}

impl MobileMcpTransport {
    pub(crate) fn new(remote: Arc<RemoteMcpTransport>) -> Self {
        Self { remote }
    }

    fn check_supported(spec: &McpTransportSpec) -> Result<(), McpError> {
        if matches!(
            spec,
            McpTransportSpec::Sse { .. } | McpTransportSpec::Http { .. }
        ) {
            Ok(())
        } else {
            Err(McpError::UnsupportedTransport(spec.transport_kind()))
        }
    }
}

#[async_trait]
impl McpTransport for MobileMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        Self::check_supported(spec)?;
        self.remote.connect(spec).await
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        Self::check_supported(spec)?;
        self.remote.connect_and_initialize(spec, options).await
    }

    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        self.remote.initialize(conn).await
    }

    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        self.remote.list_tools(conn).await
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        self.remote.list_resources(conn).await
    }

    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        self.remote.list_resource_templates(conn).await
    }

    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        self.remote.list_prompts(conn).await
    }

    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        self.remote.call_tool(conn, tool, input).await
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        self.remote.read_resource(conn, uri).await
    }

    async fn ping(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        self.remote.ping(conn_id).await
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        self.remote.notifications(conn).await
    }

    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        request: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        self.remote.handle_elicitation(conn, request).await
    }

    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        self.remote.disconnect(conn_id).await
    }

    fn disconnect_sync(&self, conn_id: McpConnectionId) {
        self.remote.disconnect_sync(conn_id);
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![McpTransportKind::Sse, McpTransportKind::Http]
    }
}

impl mcp::RawConnectionProvider for MobileMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<jsonrpc::Connection>> {
        self.remote.connection_for(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unsupported_transports_are_refused_before_reaching_the_remote_transport() {
        let transport = MobileMcpTransport::new(Arc::new(RemoteMcpTransport::new()));
        let error = transport
            .connect(&McpTransportSpec::InProcess {
                registry_key: "anything".into(),
            })
            .await
            .expect_err("in-process servers do not exist on mobile");
        assert!(matches!(error, McpError::UnsupportedTransport(_)));
        assert_eq!(
            transport.supported_transports(),
            vec![McpTransportKind::Sse, McpTransportKind::Http]
        );
    }
}
