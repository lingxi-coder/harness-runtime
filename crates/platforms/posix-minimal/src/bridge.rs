//! Stub [`BridgeTransport`] — IDE WebSocket transport ships in Plan 17.

use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_util::stream::empty;
use lingxi_core::host::{BridgeConfig, BridgeConnection, BridgeError, BridgeTransport};
use std::pin::Pin;

/// Stub bridge transport — every method returns `BridgeError::Unsupported`
/// except `disconnect`, which is idempotent.
#[derive(Default)]
pub struct PosixBridge;

impl PosixBridge {
    /// Construct.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BridgeTransport for PosixBridge {
    async fn connect(&self, _config: &BridgeConfig) -> Result<BridgeConnection, BridgeError> {
        Err(BridgeError::Unsupported)
    }

    async fn send(
        &self,
        _conn: &BridgeConnection,
        _message: serde_json::Value,
    ) -> Result<(), BridgeError> {
        Err(BridgeError::Closed)
    }

    async fn receive(
        &self,
        _conn: &BridgeConnection,
    ) -> Result<Pin<Box<dyn Stream<Item = serde_json::Value> + Send>>, BridgeError> {
        Ok(Box::pin(empty()))
    }

    async fn disconnect(&self, _conn: BridgeConnection) -> Result<(), BridgeError> {
        Ok(())
    }
}
