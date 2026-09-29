//! LSP transport — Windows wiring of `lingxi-lsp::LspClient`.
//!
//! Mirrors the POSIX implementation. On Windows we additionally apply
//! `CommandExt::creation_flags(0x08000000)` (`CREATE_NO_WINDOW`) to keep
//! the LSP server from flashing a console window — matches claude-code's
//! `windowsHide: true` spawn option (`claude-code/src/services/lsp/LSPClient.ts:103`).

use async_trait::async_trait;
use jsonrpc::Connection;
use lingxi_core::host::{
    LspError, LspRawConnection, LspServerCapabilities, LspServerConfig, LspTransport,
};
use lingxi_core::types::McpConnectionId;
use lsp::LspClient;
use serde_json::Value;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::warn;

#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Per-connection bundle: the child process handle + the typed client.
struct ConnectionEntry {
    /// Kept alive so `kill_on_drop(true)` cleans up the child when we drop it.
    #[allow(dead_code)]
    child: Child,
    client: Arc<LspClient>,
    config: LspServerConfig,
}

/// Windows LSP transport — spawns LSP server processes via `tokio::process`
/// with `CREATE_NO_WINDOW` to suppress the console window.
#[derive(Default)]
pub struct WindowsLspTransport {
    connections: Mutex<HashMap<McpConnectionId, ConnectionEntry>>,
}

impl WindowsLspTransport {
    /// Construct a new `WindowsLspTransport`.
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

#[async_trait]
impl LspTransport for WindowsLspTransport {
    async fn start_server(&self, config: &LspServerConfig) -> Result<LspRawConnection, LspError> {
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
        {
            // Suppress the console window. `tokio::process::Command` exposes
            // `creation_flags` as an inherent method on Windows builds.
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

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
        client
            .register_workspace_configuration(config.settings.clone())
            .await;
        let caps = client.initialize(root_uri, &config).await?;
        Ok(LspServerCapabilities {
            text_document_sync: lsp::client::text_document_sync_label(
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
        params: Value,
    ) -> Result<Value, LspError> {
        let client = self
            .lookup_client(conn.connection_id)
            .await
            .ok_or_else(|| LspError::Transport("connection not found".into()))?;
        client.request::<Value, Value>(method, params).await
    }

    async fn notify(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Value,
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
            if let Err(e) = entry
                .client
                .shutdown_with_timeout(entry.config.shutdown_timeout)
                .await
            {
                warn!(target: "lingxi_lsp::windows", error = %e, "polite shutdown failed; killing child");
            }
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
