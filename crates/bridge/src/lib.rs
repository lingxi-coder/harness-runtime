//! `lingxi-bridge` — IDE bridge over MCP-WebSocket.
//!
//! This crate runs an MCP-over-WebSocket endpoint that IDE plugins
//! (claude-code, VS Code Claude, etc.) discover via
//! `~/.lingxi/ide/<port>.lock`:
//!
//! 1. [`lockfile::IdeLockfile`] writes the lockfile carrying the auth token
//!    an IDE plugin must echo back in the
//!    `X-LingXi-Ide-Authorization` header.
//! 2. [`LockfileGuard`] removes that file on shutdown AND on panic.
//! 3. [`mcp_endpoint::McpEndpoint`] is the TCP+WebSocket server with an
//!    auth-gating handshake (401 on missing/wrong token).
//! 4. [`IdeBridge`] glues the endpoint + lockfile together as the engine's
//!    single owning handle.
//! 5. [`state::BridgeState`] is the observable connection snapshot held by
//!    the engine for UI / telemetry.

#![forbid(unsafe_code)]

pub mod lockfile;
pub mod mcp_endpoint;
pub mod state;
pub mod transport;
/// M9-targeted remote-drive **wire protocol** types (handshake + framing).
/// Named `wire` to avoid shadowing the `protocol` crate dependency.
pub mod wire;

pub use lockfile::{
    IdeLockfile, LockfileBody, LockfileGuard, BRIDGE_IDE_NAME, IDE_NAME, TRANSPORT,
};
pub use mcp_endpoint::{FramePump, FrameSink, McpEndpoint, AUTH_HEADER_NAME, WS_SUBPROTOCOL};
pub use state::BridgeState;
pub use transport::{BridgeError, IdeBridge};
pub use wire::{
    version_compatible, AuthChallenge, AuthResponse, BridgeRequest, BridgeResponse,
    BridgeWireError, Capabilities, ClientHello, Frame, ServerHello, BRIDGE_PROTOCOL_VERSION,
};
