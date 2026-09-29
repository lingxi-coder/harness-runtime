//! IDE bridge transport abstraction.
//!
//! The engine talks to an IDE-side bridge (VS Code / `JetBrains` plugin, or
//! any other host) through a `WebSocket`-shaped duplex transport. The concrete
//! transport (Tokio `WebSocket`, in-memory loopback, etc.) lives in a platform
//! crate. The engine receives an `Arc<dyn BridgeTransport>` and never touches a
//! concrete socket directly.
//!
//! See spec §29 (IDE Bridge) and D17 (Runtime boundary).

use async_trait::async_trait;
use futures_core::stream::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use thiserror::Error;

/// Duplex bridge transport between the engine and an IDE plugin.
///
/// Implementations are typically thin WebSocket clients but the trait makes no
/// such assumption — any framed JSON transport works. The engine treats each
/// session as `connect` → many `send`/`receive` → `disconnect`.
#[async_trait]
pub trait BridgeTransport: Send + Sync {
    /// Open a connection using `config`. Returns an opaque handle identifying
    /// the live connection.
    async fn connect(&self, config: &BridgeConfig) -> Result<BridgeConnection, BridgeError>;

    /// Send a single JSON message on `conn`. The payload is opaque to the
    /// transport; framing (e.g. WebSocket text frame) is the implementation's
    /// concern.
    async fn send(
        &self,
        conn: &BridgeConnection,
        message: serde_json::Value,
    ) -> Result<(), BridgeError>;

    /// Subscribe to incoming JSON messages on `conn`. The returned stream
    /// terminates when the peer closes the connection.
    async fn receive(
        &self,
        conn: &BridgeConnection,
    ) -> Result<Pin<Box<dyn Stream<Item = serde_json::Value> + Send>>, BridgeError>;

    /// Close `conn`. Idempotent: closing an already-closed connection is not
    /// an error.
    async fn disconnect(&self, conn: BridgeConnection) -> Result<(), BridgeError>;
}

/// Configuration for opening a bridge connection.
///
/// `jwt_token` is treated as a secret at use sites — the field is excluded
/// from serialization to avoid leaking it through diagnostics. Wrap with
/// [`crate::types::Secret`] before passing it through user-visible code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    /// WebSocket-style endpoint URL, e.g. `ws://127.0.0.1:8765/bridge`.
    pub bridge_url: String,
    /// Optional bearer JWT. Skipped during serialization to keep the token
    /// out of logs and persisted state.
    #[serde(skip_serializing, default)]
    pub jwt_token: Option<String>,
    /// Heartbeat / keepalive cadence in milliseconds.
    pub poll_interval_ms: u32,
    /// Stable identifier of the paired IDE device (matches a `TrustedDevice`).
    pub trusted_device_id: String,
}

/// Opaque handle returned by [`BridgeTransport::connect`].
///
/// Carries whatever identifier the transport needs to route subsequent
/// `send` / `receive` / `disconnect` calls back to the same socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConnection {
    /// Transport-defined connection identifier (e.g. a UUID).
    pub connection_id: String,
}

/// Errors raised by [`BridgeTransport`] implementations.
#[derive(Debug, Clone, Error)]
pub enum BridgeError {
    /// Failed to establish the underlying socket (DNS, TCP, TLS handshake).
    #[error("connection failed: {0}")]
    Connection(String),
    /// Auth handshake failed — typically a bad or expired JWT.
    #[error("auth failed: {0}")]
    Auth(String),
    /// The peer closed the connection or the socket is no longer usable.
    #[error("transport closed")]
    Closed,
    /// The transport rejected the request because rate limits were exceeded.
    #[error("rate limited: {0}")]
    RateLimited(String),
    /// The current platform does not support this transport.
    #[error("unsupported on this platform")]
    Unsupported,
}
