//! LSP transport — shared desktop wiring of `lingxi-lsp::LspClient` over a
//! `tokio::process::Command` child speaking JSON-RPC on stdin/stdout.
//!
//! ## Spawn handshake
//!
//! 1. `tokio::process::Command::spawn()` with `stdin/stdout/stderr` all
//!    `Stdio::piped()` and `kill_on_drop(true)` so the child does not
//!    outlive the transport.
//! 2. **Immediately** call `child.stdin.take()` and `child.stdout.take()`.
//!    If either returns `None`, the spawn race lost (ENOENT, etc.) and the
//!    connection reader task in `lingxi-jsonrpc` would otherwise panic on
//!    a missing stream — we bail with [`LspError::Transport`] first.
//!    (claude-code makes the same guard explicit, see `LSPClient.ts:106`.)
//! 3. Build a `jsonrpc::Connection::new_lsp` over the stdout
//!    reader + stdin writer, then wrap it in [`lsp::LspClient`].
//!
//! The transport's request/notify methods retain the shared flat UTF-16 JSON
//! projection and only turn it into raw JSON text at the stdio wire boundary.

use ::lsp::LspClient;
use async_trait::async_trait;
use jsonrpc::Connection;
use lingxi_core::host::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig,
    LspTransport as LspTransportTrait,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::McpConnectionId;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::warn;

/// Per-connection bundle: the child process handle + the typed client.
struct ConnectionEntry {
    /// Kept alive so `kill_on_drop(true)` cleans up the child when we drop it.
    #[allow(dead_code)]
    child: Child,
    client: Arc<LspClient>,
    config: LspServerConfig,
}

/// Desktop LSP transport. `HIDE_WINDOW` is enabled by the Windows adapter so
/// its child process does not open a console window on Windows.
#[derive(Default)]
pub struct DesktopLspTransport<const HIDE_WINDOW: bool> {
    connections: Mutex<HashMap<McpConnectionId, ConnectionEntry>>,
}

impl<const HIDE_WINDOW: bool> DesktopLspTransport<HIDE_WINDOW> {
    /// Construct a new desktop LSP transport.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    async fn lookup_client(&self, id: McpConnectionId) -> Option<Arc<LspClient>> {
        self.connections
            .lock()
            .await
            .get(&id)
            .map(|e| Arc::clone(&e.client))
    }

    async fn lookup_client_and_config(
        &self,
        id: McpConnectionId,
    ) -> Option<(Arc<LspClient>, LspServerConfig)> {
        self.connections
            .lock()
            .await
            .get(&id)
            .map(|entry| (Arc::clone(&entry.client), entry.config.clone()))
    }
}

fn server_command<const HIDE_WINDOW: bool>(config: &LspServerConfig) -> Command {
    let mut cmd = Command::new(&config.command);
    cmd.args(&config.args);
    for (k, v) in &config.env {
        cmd.env(k, v);
    }
    if let Some(workspace_folder) = config.workspace_folder.as_deref() {
        cmd.current_dir(std::path::Path::new(workspace_folder));
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(target_os = "windows")]
    if let Some(flags) = windows_creation_flags::<HIDE_WINDOW>() {
        cmd.creation_flags(flags);
    }
    cmd
}

#[cfg(target_os = "windows")]
fn windows_creation_flags<const HIDE_WINDOW: bool>() -> Option<u32> {
    // The POSIX adapter deliberately leaves this flag unset on Windows.
    HIDE_WINDOW.then_some(0x0800_0000) // CREATE_NO_WINDOW
}

#[async_trait]
impl<const HIDE_WINDOW: bool> LspTransportTrait for DesktopLspTransport<HIDE_WINDOW> {
    async fn start_server(&self, config: &LspServerConfig) -> Result<LspRawConnection, LspError> {
        let mut cmd = server_command::<HIDE_WINDOW>(config);
        let mut child = cmd
            .spawn()
            .map_err(|e| LspError::Transport(format!("spawn {}: {}", config.command, e)))?;

        let stdin = child.stdin.take().ok_or_else(|| {
            LspError::Transport(format!(
                "LSP server '{}' stdin not available (spawn race)",
                config.name
            ))
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            LspError::Transport(format!(
                "LSP server '{}' stdout not available (spawn race)",
                config.name
            ))
        })?;

        // Drain stderr into a debug log so the user has diagnostics if the
        // server misbehaves. Don't propagate stderr errors — they're
        // expected to occur when the server exits.
        if let Some(stderr) = child.stderr.take() {
            let name = config.name.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "lingxi_lsp::stderr", server = %name, "{}", line);
                }
            });
        }

        let connection = Connection::new_lsp(stdout, stdin);
        let client = Arc::new(LspClient::with_shared(
            config.name.clone(),
            Arc::new(connection),
        ));

        let id = McpConnectionId::new();
        self.connections.lock().await.insert(
            id,
            ConnectionEntry {
                child,
                client,
                config: config.clone(),
            },
        );
        Ok(LspRawConnection { connection_id: id })
    }

    async fn initialize(
        &self,
        conn: &LspRawConnection,
        root_uri: &str,
    ) -> Result<LspServerCapabilities, LspError> {
        let (client, config) = self
            .lookup_client_and_config(conn.connection_id)
            .await
            .ok_or_else(|| LspError::Transport("connection not found".into()))?;
        let settings = config.settings.clone();
        client.register_workspace_configuration(settings).await;
        let caps = client.initialize(root_uri, &config).await?;
        Ok(LspServerCapabilities {
            text_document_sync: ::lsp::client::text_document_sync_label(
                caps.text_document_sync.as_ref(),
            ),
            completion: caps.completion_provider.is_some(),
            hover: caps.hover_provider.is_some(),
            definition: caps.definition_provider.is_some(),
            references: caps.references_provider.is_some(),
            diagnostics: config.diagnostics.unwrap_or(true),
            symbols: caps.document_symbol_provider.is_some(),
            formatting: caps.document_formatting_provider.is_some(),
            rename: caps.rename_provider.is_some(),
            code_action: caps.code_action_provider.is_some(),
        })
    }

    async fn request(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Utf16JsonProjection,
    ) -> Result<Utf16JsonProjection, LspError> {
        let client = self
            .lookup_client(conn.connection_id)
            .await
            .ok_or_else(|| LspError::Transport("connection not found".into()))?;
        client.request(method, params).await
    }

    async fn notify(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Utf16JsonProjection,
    ) -> Result<(), LspError> {
        let client = self
            .lookup_client(conn.connection_id)
            .await
            .ok_or_else(|| LspError::Transport("connection not found".into()))?;
        client.notify(method, params).await
    }

    async fn connection(&self, conn_id: McpConnectionId) -> Result<Arc<Connection>, LspError> {
        let client = self
            .lookup_client(conn_id)
            .await
            .ok_or_else(|| LspError::Transport("connection not found".into()))?;
        Ok(client.connection())
    }

    async fn is_alive(&self, conn_id: McpConnectionId) -> bool {
        let mut connections = self.connections.lock().await;
        let Some(entry) = connections.get_mut(&conn_id) else {
            return false;
        };
        let alive =
            !entry.client.connection().is_closed() && matches!(entry.child.try_wait(), Ok(None));
        if !alive {
            connections.remove(&conn_id);
        }
        alive
    }

    async fn shutdown(&self, conn_id: McpConnectionId) -> Result<(), LspError> {
        let removed = self.connections.lock().await.remove(&conn_id);
        if let Some(entry) = removed {
            // Polite shutdown sequence; ignore errors (server may already be dead).
            if let Err(e) = entry
                .client
                .shutdown_with_timeout(entry.config.shutdown_timeout)
                .await
            {
                if HIDE_WINDOW {
                    warn!(target: "lingxi_lsp::windows", error = %e, "polite shutdown failed; killing child");
                } else {
                    warn!(target: "lingxi_lsp::posix", error = %e, "polite shutdown failed; killing child");
                }
            }
            // `kill_on_drop(true)` handles the kill when `entry` falls out of scope.
            drop(entry);
        }
        Ok(())
    }

    async fn terminate(&self, conn_id: McpConnectionId) -> Result<(), LspError> {
        self.connections.lock().await.remove(&conn_id);
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// POSIX command policy: no Windows-specific process flags.
pub type PosixLspTransport = DesktopLspTransport<false>;
/// Windows command policy: hide the server console on Windows.
pub type WindowsLspTransport = DesktopLspTransport<true>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn both_desktop_policies_preserve_command_arguments_environment_and_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let config = LspServerConfig {
            command: "language-server".into(),
            args: vec!["--stdio".into(), "two words".into()],
            env: [("HARNESS_LSP_TEST".into(), "expected value".into())]
                .into_iter()
                .collect(),
            workspace_folder: Some(workspace.path().to_str().unwrap().into()),
            ..Default::default()
        };
        for command in [
            server_command::<false>(&config),
            server_command::<true>(&config),
        ] {
            let command = command.as_std();
            assert_eq!(command.get_program(), OsStr::new("language-server"));
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                vec![OsStr::new("--stdio"), OsStr::new("two words")],
            );
            assert!(command.get_envs().any(|(key, value)| {
                key == OsStr::new("HARNESS_LSP_TEST") && value == Some(OsStr::new("expected value"))
            }));
            assert_eq!(command.get_current_dir(), Some(workspace.path()));
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn only_windows_policy_hides_the_server_console() {
        assert_eq!(windows_creation_flags::<false>(), None);
        assert_eq!(windows_creation_flags::<true>(), Some(0x0800_0000));
    }
}
