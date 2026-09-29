//! Bridge from a platform MCP transport to the live `jsonrpc::Connection`.
//!
//! The 4 builtin MCP tools dispatch through
//! [`crate::registry::McpRegistry::get_client`] which returns an
//! `Arc<McpClient>`, and [`crate::client::McpClient`] wraps an
//! `Arc<jsonrpc::Connection>`. The transport privately owns that
//! `Arc<Connection>` (e.g. inside `PosixMcpTransport`'s connection map), and
//! [`crate::registry::McpRegistry`] only holds an `Arc<dyn lingxi_core::host::McpTransport>`
//! — it cannot reach jsonrpc through the frozen `traits` boundary.
//!
//! This trait lives in the **`mcp` crate** (NOT `traits/`) precisely so it can
//! name `jsonrpc::Connection` without forcing `traits → jsonrpc`. The dep DAG
//! is `posix → mcp → jsonrpc`, so the platform transport can implement it and
//! the registry can consume it. When supplied to
//! [`crate::registry::McpRegistry::with_raw_conn`], the registry builds a live
//! [`crate::client::McpClient`] per connected server so the builtin tools reach
//! the server at runtime instead of erroring "not registered".
//!
//! TS ref: `services/mcp/client.ts:1029-1115` (`connectToServer` building the
//! SDK `Client`) / `:1688-1709` (`ensureConnectedClient` returning a live
//! client).

use std::sync::Arc;

/// Hands out the live `Arc<jsonrpc::Connection>` for a connection id.
///
/// Implemented by the platform transport that owns the connection map (the
/// `mcp` crate cannot construct connections itself; it only consumes them).
/// Returns `None` when no connection is registered for `id` (e.g. a stub
/// transport, or a connection that was already torn down).
pub trait RawConnectionProvider: Send + Sync {
    /// Clone out the `Arc<jsonrpc::Connection>` for `id`, if one is live.
    fn connection_for(
        &self,
        id: lingxi_core::types::McpConnectionId,
    ) -> Option<Arc<jsonrpc::Connection>>;
}
