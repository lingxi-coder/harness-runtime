//! `BridgeTransport` — POSIX (M2 stub).
//!
//! Ships the type surface so the engine's IDE bridge layer can hold an
//! `Arc<dyn BridgeTransport>` without conditional compilation. Real
//! `WebSocket` wiring (handshake, JWT, heartbeat) lands in M2 phase 3.

use async_trait::async_trait;
use futures::stream::{empty, Stream};
use lingxi_core::host::{BridgeConfig, BridgeConnection, BridgeError, BridgeTransport};
use std::pin::Pin;

/// POSIX `BridgeTransport` — M2 stub returning `Unsupported` on connect.
#[derive(Default)]
pub struct PosixBridgeTransport;

impl PosixBridgeTransport {
    /// Construct a new `PosixBridgeTransport`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BridgeTransport for PosixBridgeTransport {
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
