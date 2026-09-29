//! Stub [`LspTransport`] — full stdio-based wiring lands in Plan 17.

use async_trait::async_trait;
use lingxi_core::host::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
};
use lingxi_core::types::McpConnectionId;
use serde_json::Value;

/// Stub LSP transport.
#[derive(Default)]
pub struct PosixLsp;

impl PosixLsp {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl LspTransport for PosixLsp {
    async fn start_server(&self, _config: &LspServerConfig) -> Result<LspRawConnection, LspError> {
        Err(LspError::Unavailable)
    }

    async fn initialize(
        &self,
        _conn: &LspRawConnection,
        _root_uri: &str,
    ) -> Result<LspServerCapabilities, LspError> {
        Err(LspError::Unavailable)
    }

    async fn request(
        &self,
        _conn: &LspRawConnection,
        _method: &str,
        _params: Value,
    ) -> Result<Value, LspError> {
        Err(LspError::Unavailable)
    }

    async fn notify(
        &self,
        _conn: &LspRawConnection,
        _method: &str,
        _params: Value,
    ) -> Result<(), LspError> {
        Err(LspError::Unavailable)
    }

    async fn shutdown(&self, _conn_id: McpConnectionId) -> Result<(), LspError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        false
    }
}
