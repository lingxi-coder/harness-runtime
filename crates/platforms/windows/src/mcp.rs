//! MCP transport — Windows.
//!
//! Mirrors the POSIX remote-transport boundary: stdio process ownership stays
//! here, while HTTP/SSE/IDE JSON-RPC state is held by the shared
//! [`platform_common::RemoteMcpTransport`]. `SseIde` and `WsIde` therefore use
//! the same loopback auth-header and path handling as POSIX without importing
//! any provider credential or cloud-auth machinery.
//!
//! M2-02c also lands `spawn_stdio` (mirrors the POSIX implementation, NDJSON
//! framing + 64 MB stderr ring) plus a re-export of the shared WebSocket
//! connector at `platform_windows::mcp::connect_ws`.

use async_trait::async_trait;
use futures_util::FutureExt;
use jsonrpc::Connection;
use lingxi_core::host::{
    ElicitRequestDto, ElicitResultDto, McpConnectOptions, McpConnectResult, McpError,
    McpNegotiatedProtocol, McpNotificationStream, McpPromptDto, McpProtocolEra, McpRawConnection,
    McpResourceContentDto, McpResourceContentsRich, McpResourceDto, McpResourceTemplateDto,
    McpToolDto, McpToolResultDto, McpTransport, McpTransportKind, McpTransportSpec,
    ServerCapabilitiesDto,
};
use lingxi_core::types::McpConnectionId;
use platform_common::mcp_stdio::{StderrRing, StdioConfig};
use platform_common::RemoteMcpTransport;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::Mutex as AsyncMutex;

/// Per-connection state held by `WindowsMcpTransport`.
///
/// Remote connections are owned by [`RemoteMcpTransport`] so both Windows and
/// POSIX share identical JSON-RPC cleanup. This map only contains stdio
/// children, whose process lifecycle is Windows-specific.
pub(crate) enum WindowsMcpConnection {
    /// `Stdio` connection — owns the child process.
    Stdio {
        /// Owned child process; killed on `disconnect`.
        child: Child,
    },
}

/// Windows MCP transport.
///
/// Supports stdio plus shared remote `Sse`, `Http`, `SseIde`, and `WsIde`
/// transports. Generic `WebSocket`, `InProcess`, and `SdkControl` specs remain
/// unsupported, matching the POSIX transport contract. Remote connection
/// cleanup is delegated to the shared transport so disconnecting an IDE
/// endpoint closes its broker and removes its connection id.
#[derive(Default)]
pub struct WindowsMcpTransport {
    connections: Mutex<HashMap<McpConnectionId, WindowsMcpConnection>>,
    remote: Arc<RemoteMcpTransport>,
}

impl WindowsMcpTransport {
    /// Construct a new `WindowsMcpTransport`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, id: McpConnectionId, conn: WindowsMcpConnection) {
        // Recover from a poisoned std `Mutex` by silently dropping the
        // insert — the engine will surface the failure on the next call
        // when the connection id misses the map.
        if let Ok(mut guard) = self.connections.lock() {
            guard.insert(id, conn);
        }
    }

    fn is_remote_connection(&self, id: McpConnectionId) -> bool {
        self.remote.connection_for(id).is_some()
    }
}

/// Best-effort synchronous cleanup for a connection whose combined handshake
/// future was cancelled. Remote brokers have their own equivalent guard;
/// this one covers the Windows-owned stdio child map.
struct ConnectionCleanupGuard<'a> {
    transport: &'a WindowsMcpTransport,
    id: McpConnectionId,
    armed: bool,
}

impl ConnectionCleanupGuard<'_> {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ConnectionCleanupGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.transport.disconnect_sync(self.id);
        }
    }
}

#[async_trait]
impl McpTransport for WindowsMcpTransport {
    async fn connect(&self, spec: &McpTransportSpec) -> Result<McpRawConnection, McpError> {
        let id = McpConnectionId::new();
        match spec {
            McpTransportSpec::Stdio { command, args, env } => {
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                cmd.stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped());
                let child = cmd
                    .spawn()
                    .map_err(|e| McpError::Connection(e.to_string()))?;
                self.insert(id, WindowsMcpConnection::Stdio { child });
            }
            McpTransportSpec::Sse { .. }
            | McpTransportSpec::Http { .. }
            | McpTransportSpec::SseIde { .. }
            | McpTransportSpec::WsIde { .. } => return self.remote.connect(spec).await,
            other => return Err(McpError::UnsupportedTransport(map_kind(other))),
        }
        Ok(McpRawConnection { connection_id: id })
    }

    async fn connect_and_initialize(
        &self,
        spec: &McpTransportSpec,
        options: McpConnectOptions,
    ) -> Result<McpConnectResult, McpError> {
        // Keep remote transports on the shared modern-negotiation path. This
        // is what POSIX does and is important for IDE servers that advertise
        // the modern result envelope or skills extension.
        if matches!(
            spec,
            McpTransportSpec::Sse { .. }
                | McpTransportSpec::Http { .. }
                | McpTransportSpec::SseIde { .. }
                | McpTransportSpec::WsIde { .. }
        ) {
            return self.remote.connect_and_initialize(spec, options).await;
        }

        // Stdio remains a Windows-owned transport. Preserve the trait's
        // deadline and cancellation semantics even though its current
        // initialize implementation is a platform stub.
        let deadline = tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(options.deadline_ms))
            .ok_or_else(|| McpError::Connection("MCP connection deadline overflow".into()))?;
        let connection = tokio::time::timeout_at(deadline, self.connect(spec))
            .await
            .map_err(|_| McpError::Connection("MCP connection deadline exceeded".into()))??;
        let cleanup = ConnectionCleanupGuard {
            transport: self,
            id: connection.connection_id,
            armed: true,
        };
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_default();
        let result = std::panic::AssertUnwindSafe(tokio::time::timeout(
            remaining,
            self.initialize(&connection),
        ))
        .catch_unwind()
        .await;
        let capabilities = match result {
            Ok(Ok(Ok(capabilities))) => capabilities,
            Ok(Ok(Err(error))) => {
                if self.disconnect(connection.connection_id).await.is_ok() {
                    cleanup.disarm();
                }
                return Err(error);
            }
            Ok(Err(_)) => {
                if self.disconnect(connection.connection_id).await.is_ok() {
                    cleanup.disarm();
                }
                return Err(McpError::Connection(
                    "MCP connection deadline exceeded".into(),
                ));
            }
            Err(payload) => {
                let _ = self.disconnect(connection.connection_id).await;
                std::panic::resume_unwind(payload);
            }
        };
        cleanup.disarm();
        Ok(McpConnectResult {
            connection,
            capabilities,
            negotiated: McpNegotiatedProtocol {
                era: McpProtocolEra::Legacy,
                version: "2025-11-25".into(),
            },
        })
    }

    async fn initialize(&self, conn: &McpRawConnection) -> Result<ServerCapabilitiesDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.initialize(conn).await;
        }
        // M2.03 stub — full `JSON-RPC` initialize lands in M2 phase 3.
        Ok(ServerCapabilitiesDto {
            tools: true,
            resources: false,
            prompts: false,
            logging: false,
            directory_read: false,
            extensions: HashMap::new(),
            experimental: HashMap::new(),
        })
    }

    async fn list_tools(&self, conn: &McpRawConnection) -> Result<Vec<McpToolDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_tools(conn).await;
        }
        Err(McpError::Internal(
            "windows mcp list_tools delegated to lingxi-mcp::McpClient (M2-02b)".into(),
        ))
    }

    async fn list_resources(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_resources(conn).await;
        }
        Ok(Vec::new())
    }

    async fn list_resource_templates(
        &self,
        conn: &McpRawConnection,
    ) -> Result<Vec<McpResourceTemplateDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_resource_templates(conn).await;
        }
        Ok(Vec::new())
    }

    async fn list_prompts(&self, conn: &McpRawConnection) -> Result<Vec<McpPromptDto>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.list_prompts(conn).await;
        }
        Ok(Vec::new())
    }

    async fn call_tool(
        &self,
        conn: &McpRawConnection,
        tool: &str,
        input: Value,
    ) -> Result<McpToolResultDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.call_tool(conn, tool, input).await;
        }
        Err(McpError::Internal(
            "windows mcp call_tool delegated to lingxi-mcp::McpClient (M2-02b)".into(),
        ))
    }

    async fn read_resource(
        &self,
        conn: &McpRawConnection,
        uri: &str,
    ) -> Result<McpResourceContentDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.read_resource(conn, uri).await;
        }
        Err(McpError::Internal(
            "windows mcp read_resource delegated to lingxi-mcp::McpClient (M2-02b)".into(),
        ))
    }

    async fn read_resource_rich(
        &self,
        conn: &McpRawConnection,
        uri: &str,
        output_dir: &std::path::Path,
    ) -> Result<Vec<McpResourceContentsRich>, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.read_resource_rich(conn, uri, output_dir).await;
        }
        let single = self.read_resource(conn, uri).await?;
        Ok(vec![McpResourceContentsRich {
            uri: single.uri,
            mime_type: single.mime_type,
            meta: single.meta,
            text: Some(single.content),
            blob_saved_to: None,
        }])
    }

    async fn ping(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        if self.is_remote_connection(conn_id) {
            return self.remote.ping(conn_id).await;
        }
        Ok(())
    }

    async fn notifications(
        &self,
        conn: &McpRawConnection,
    ) -> Result<McpNotificationStream, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.notifications(conn).await;
        }
        use futures::stream::empty;
        Ok(Box::pin(empty()))
    }

    async fn handle_elicitation(
        &self,
        conn: &McpRawConnection,
        req: ElicitRequestDto,
    ) -> Result<ElicitResultDto, McpError> {
        if self.is_remote_connection(conn.connection_id) {
            return self.remote.handle_elicitation(conn, req).await;
        }
        Err(McpError::Internal(
            "windows mcp elicitation delegated to lingxi-mcp::McpClient (M2-02b)".into(),
        ))
    }

    async fn disconnect(&self, conn_id: McpConnectionId) -> Result<(), McpError> {
        if self.is_remote_connection(conn_id) {
            return self.remote.disconnect(conn_id).await;
        }
        let entry = self
            .connections
            .lock()
            .ok()
            .and_then(|mut g| g.remove(&conn_id));
        if let Some(WindowsMcpConnection::Stdio { mut child }) = entry {
            let _ = child.kill().await;
        }
        Ok(())
    }

    fn disconnect_sync(&self, conn_id: McpConnectionId) {
        if self.is_remote_connection(conn_id) {
            self.remote.disconnect_sync(conn_id);
            return;
        }
        let entry = self
            .connections
            .lock()
            .ok()
            .and_then(|mut guard| guard.remove(&conn_id));
        if let Some(WindowsMcpConnection::Stdio { mut child }) = entry {
            let _ = child.start_kill();
        }
    }

    fn supported_transports(&self) -> Vec<McpTransportKind> {
        vec![
            McpTransportKind::Stdio,
            McpTransportKind::Sse,
            McpTransportKind::Http,
            McpTransportKind::SseIde,
            McpTransportKind::WsIde,
        ]
    }
}

/// Bridge remote connections into `McpRegistry`'s live `McpClient` path.
///
/// The remote connection map is owned by the shared transport, so Windows and
/// POSIX expose the same `Arc<jsonrpc::Connection>` without duplicating wire
/// or credential handling. Windows-owned stdio remains a process-only stub
/// until its platform-specific child connection lifecycle is upgraded.
impl mcp::RawConnectionProvider for WindowsMcpTransport {
    fn connection_for(&self, id: McpConnectionId) -> Option<Arc<Connection>> {
        self.remote.connection_for(id)
    }
}

fn map_kind(spec: &McpTransportSpec) -> McpTransportKind {
    match spec {
        McpTransportSpec::Stdio { .. } => McpTransportKind::Stdio,
        McpTransportSpec::Sse { .. } => McpTransportKind::Sse,
        McpTransportSpec::Http { .. } => McpTransportKind::Http,
        McpTransportSpec::WebSocket { .. } => McpTransportKind::WebSocket,
        McpTransportSpec::InProcess { .. } => McpTransportKind::InProcess,
        McpTransportSpec::SseIde { .. } => McpTransportKind::SseIde,
        McpTransportSpec::WsIde { .. } => McpTransportKind::WsIde,
        McpTransportSpec::SdkControl { .. } => McpTransportKind::SdkControl,
    }
}

/// Error type returned by `spawn_stdio` on Windows.
///
/// Mirrors `platform_posix::mcp::McpTransportError` — distinct type
/// per crate so callers can match against either via the shared `From` impls
/// in `lingxi-mcp` (added in M2-02d when the per-platform transport plug-in
/// trait lands).
#[derive(Debug, thiserror::Error)]
pub enum McpTransportError {
    /// Failed to spawn the child process.
    #[error("io: {0}")]
    Io(String),
    /// Failed to acquire one of the stdin/stdout/stderr pipes from the child.
    #[error("missing stdio pipe: {0}")]
    MissingPipe(&'static str),
}

/// Handles returned by [`spawn_stdio_with_handles`] — the same `Connection`
/// that [`spawn_stdio`] returns, plus a shared handle on the stderr ring
/// buffer so callers can snapshot any buffered stderr if the child misbehaves.
#[non_exhaustive]
pub struct StdioHandles {
    /// The fully-wired JSON-RPC `Connection` over the child's stdio.
    pub connection: Connection,
    /// Shared `StderrRing` populated by a background drain task.
    pub stderr: Arc<AsyncMutex<StderrRing>>,
}

/// Spawn an MCP child over stdio on Windows and return a fully-wired
/// `jsonrpc::Connection`.
///
/// Mirrors `platform_posix::mcp::spawn_stdio` exactly in framing
/// (NDJSON / `LineCodec` via `Connection::new_line_delimited`), stderr
/// handling (64 MB drop-oldest ring), and child reaping (`kill_on_drop` so
/// dropping the connection tears the child down).
///
/// Process-group handling is intentionally NOT applied here — Windows has no
/// `setsid` equivalent in `tokio::process`, and MCP children do not require
/// it because `kill_on_drop(true)` handles cleanup adequately. (A future
/// follow-up may wrap the child in a Job Object so descendant processes
/// also die; out of scope for M2-02c.)
///
/// # Errors
///
/// - [`McpTransportError::Io`] if the child fails to spawn (e.g. command not
///   found, permission denied, cwd does not exist).
/// - [`McpTransportError::MissingPipe`] if `Stdio::piped()` failed to attach
///   one of the three pipes — should not happen in practice but is reported
///   rather than panicked on.
pub async fn spawn_stdio(cfg: StdioConfig) -> Result<Connection, McpTransportError> {
    let StdioHandles { connection, .. } = spawn_stdio_with_handles(cfg).await?;
    Ok(connection)
}

/// Same as [`spawn_stdio`] but also surfaces the shared stderr ring buffer
/// so callers can inspect captured stderr after the child exits or hangs.
///
/// The function is `async` to leave room for a future initialize handshake
/// without breaking callers — today every `.await` happens inside the
/// spawned background tasks, so clippy's `unused_async` is allowed here.
#[allow(clippy::unused_async)]
pub async fn spawn_stdio_with_handles(cfg: StdioConfig) -> Result<StdioHandles, McpTransportError> {
    let mut cmd = tokio::process::Command::new(&cfg.cmd);
    cmd.args(&cfg.args);
    // The child inherits the parent's environment, then overrides with
    // `cfg.env`. Callers are responsible for filtering secrets out of
    // `cfg.env` before constructing the config.
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `kill_on_drop` ensures the child dies if the wait task is dropped
    // (e.g. on `Connection` drop, since the wait task owns the `Child`).
    cmd.kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| McpTransportError::Io(e.to_string()))?;

    let stdin = child
        .stdin
        .take()
        .ok_or(McpTransportError::MissingPipe("stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(McpTransportError::MissingPipe("stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or(McpTransportError::MissingPipe("stderr"))?;

    // Drain stderr into a shared `StderrRing` on a background task.
    let stderr_ring = Arc::new(AsyncMutex::new(StderrRing::new(StderrRing::DEFAULT_CAP)));
    {
        let ring = stderr_ring.clone();
        tokio::spawn(async move {
            let mut reader = stderr;
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut guard = ring.lock().await;
                        guard.push(&buf[..n]);
                    }
                }
            }
        });
    }

    // Wire the JSON-RPC connection over NDJSON stdio.
    let connection = Connection::new_line_delimited(stdout, stdin);

    // Reap the child on exit. The waiter task owns the `Child`, so
    // `kill_on_drop(true)` makes the child die if this task is dropped
    // (e.g. on runtime shutdown). When the child exits normally, its
    // stdout closes and the broker shuts down without further action.
    tokio::spawn(async move {
        match child.wait().await {
            Ok(status) => tracing::debug!(?status, "mcp stdio child exited"),
            Err(e) => tracing::warn!(error = %e, "mcp stdio child wait failed"),
        }
    });

    Ok(StdioHandles {
        connection,
        stderr: stderr_ring,
    })
}

// Re-export the shared WebSocket connector at this crate too, so callers
// can write `platform_windows::mcp::connect_ws` directly without
// reaching into `lingxi_platform_common`.
pub use platform_common::mcp_ws::{connect_ws, WsConnectError, AUTH_HEADER_NAME, WS_SUBPROTOCOL};

#[cfg(test)]
mod re_export_tests {
    /// Verify the windows crate exposes the public `connect_ws` symbol at
    /// `platform_windows::mcp::connect_ws` (callers should not have
    /// to import from `lingxi_platform_common` directly).
    #[allow(unused_imports)]
    use crate::mcp::connect_ws;
}
