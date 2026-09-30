//! LSP (Language Server Protocol) transport abstraction.
//!
//! `LspTransport` is the boundary between `lingxi-lsp` (the registry +
//! per-connection state machine) and the platform-specific transport that
//! actually speaks JSON-RPC to language servers. The engine never speaks
//! the wire protocol directly.
//!
//! See spec §25 (LSP) and D17 (Runtime boundary).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// Configuration describing one LSP server the engine knows how to start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerConfig {
    /// Logical server name (used as registry key).
    ///
    /// Claude Code's public `.lsp.json` shape keys configurations by name and
    /// does not repeat it inside the value.  The plugin loader stamps the map
    /// key here after deserialization.
    #[serde(default)]
    pub name: String,
    /// Executable to spawn (e.g. `rust-analyzer`).
    pub command: String,
    /// CLI arguments for the server.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// Languages this server handles (e.g. `["rust"]`).
    #[serde(default, alias = "trigger_languages")]
    pub trigger_languages: Vec<String>,
    /// Markers used to detect the project root (e.g. `["Cargo.toml"]`).
    #[serde(default, alias = "root_dir_markers")]
    pub root_dir_markers: Vec<String>,
    /// Optional server-specific initialization payload.
    #[serde(default, alias = "initialization_options")]
    pub initialization_options: Option<Value>,
    /// File-extension → `languageId` mapping for `textDocument/didOpen`.
    /// Mirrors claude-code's `LSPServerConfig.extensionToLanguage`. The keys
    /// must include the leading dot (`.rs`, `.ts`) and are case-folded to
    /// lowercase before lookup.
    #[serde(default, alias = "extension_to_language")]
    pub extension_to_language: std::collections::HashMap<String, String>,
    /// Accepted public transport selector. Claude Code 2.1.251 accepts
    /// `"stdio" | "socket"`; its current launcher still uses stdio for both.
    #[serde(default = "default_lsp_transport")]
    pub transport: String,
    /// Server settings exposed through `workspace/configuration` and pushed
    /// once through `workspace/didChangeConfiguration` after initialization.
    #[serde(default)]
    pub settings: Option<Value>,
    /// Workspace folder override. Missing means the live process workspace.
    #[serde(default)]
    pub workspace_folder: Option<String>,
    /// Initialization deadline in milliseconds.
    #[serde(default)]
    pub startup_timeout: Option<u64>,
    /// Graceful shutdown deadline in milliseconds.
    #[serde(default)]
    pub shutdown_timeout: Option<u64>,
    /// Whether a crashed server may be started again. Missing means true.
    #[serde(default)]
    pub restart_on_crash: Option<bool>,
    /// Maximum crash-recovery starts. Missing means three.
    #[serde(default)]
    pub max_restarts: Option<u32>,
    /// Whether passive diagnostics are injected. Missing means true.
    #[serde(default)]
    pub diagnostics: Option<bool>,
}

fn default_lsp_transport() -> String {
    "stdio".to_string()
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            trigger_languages: Vec::new(),
            root_dir_markers: Vec::new(),
            initialization_options: None,
            extension_to_language: std::collections::HashMap::new(),
            transport: default_lsp_transport(),
            settings: None,
            workspace_folder: None,
            startup_timeout: None,
            shutdown_timeout: None,
            restart_on_crash: None,
            max_restarts: None,
            diagnostics: None,
        }
    }
}

/// Handle returned by a successful [`LspTransport::start_server`].
///
/// Reuses [`crate::types::McpConnectionId`] to keep the connection-id
/// namespace shared between MCP and LSP — both are connection-level
/// routing keys handled by the same registry machinery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspRawConnection {
    /// Stable connection identifier.
    pub connection_id: crate::types::McpConnectionId,
}

/// Server capability flags returned by `initialize`.
///
/// Mirrors the LSP capability object — booleans indicate whether the
/// server exposes the corresponding feature category.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // mirrors the LSP wire spec verbatim
pub struct LspServerCapabilities {
    /// Encoded `textDocumentSyncKind` (`"full"`, `"incremental"`, ...).
    pub text_document_sync: Option<String>,
    /// `textDocument/completion` supported.
    pub completion: bool,
    /// `textDocument/hover` supported.
    pub hover: bool,
    /// `textDocument/definition` supported.
    pub definition: bool,
    /// `textDocument/references` supported.
    pub references: bool,
    /// `textDocument/publishDiagnostics` supported.
    pub diagnostics: bool,
    /// `textDocument/documentSymbol` / `workspace/symbol` supported.
    pub symbols: bool,
    /// `textDocument/formatting` supported.
    pub formatting: bool,
    /// `textDocument/rename` supported.
    pub rename: bool,
    /// `textDocument/codeAction` supported.
    pub code_action: bool,
}

/// Transport boundary between the engine's LSP layer and the wire protocol.
///
/// Implementations live in platform crates. The engine builds an
/// [`LspServerConfig`], calls [`Self::start_server`], performs the
/// `initialize` handshake via [`Self::initialize`], and then issues
/// requests through [`Self::request`] / [`Self::notify`].
#[async_trait]
pub trait LspTransport: Send + Sync {
    /// Spawn the server described by `config` and return a connection
    /// handle.
    ///
    /// # Errors
    /// Returns [`LspError`] when the transport cannot launch the server or
    /// is unsupported on the current platform.
    async fn start_server(&self, config: &LspServerConfig) -> Result<LspRawConnection, LspError>;

    /// Perform the LSP `initialize` handshake against `root_uri` and return
    /// the server's declared capabilities.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] / [`LspError::ServerError`] on
    /// handshake failure.
    async fn initialize(
        &self,
        conn: &LspRawConnection,
        root_uri: &str,
    ) -> Result<LspServerCapabilities, LspError>;

    /// Issue an LSP request and await the JSON-RPC response.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] when the channel breaks, or
    /// [`LspError::ServerError`] when the server returns a JSON-RPC error.
    async fn request(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Value,
    ) -> Result<Value, LspError>;

    /// Issue an LSP notification (no response expected).
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] when the notification cannot be
    /// delivered.
    async fn notify(
        &self,
        conn: &LspRawConnection,
        method: &str,
        params: Value,
    ) -> Result<(), LspError>;

    /// Return the live `Arc<jsonrpc::Connection>` for `conn_id` so the registry
    /// can wrap it in an `LspClient` over the SAME shared connection the
    /// transport drives (the bridge that lets `ensure_server_for_file` cache a
    /// client for the LSP tool). Defaults to [`LspError::Unavailable`] for
    /// transports that don't expose a connection.
    ///
    /// # Errors
    /// Returns [`LspError`] when the connection is unknown or unavailable.
    async fn connection(
        &self,
        _conn_id: crate::types::McpConnectionId,
    ) -> Result<std::sync::Arc<jsonrpc::Connection>, LspError> {
        Err(LspError::Unavailable)
    }

    /// Whether the child/connection is still alive. Transports that cannot
    /// observe process state conservatively return true.
    async fn is_alive(&self, _conn_id: crate::types::McpConnectionId) -> bool {
        true
    }

    /// Shutdown and tear down the server identified by `conn_id`.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] when the shutdown cannot be
    /// completed gracefully.
    async fn shutdown(&self, conn_id: crate::types::McpConnectionId) -> Result<(), LspError>;

    /// Immediately discard a connection that failed before initialization
    /// completed. Platform implementations should remove their connection
    /// entry so dropping the owned child terminates it without another LSP
    /// handshake.
    ///
    /// The default keeps test and minimal transports source-compatible.
    async fn terminate(&self, conn_id: crate::types::McpConnectionId) -> Result<(), LspError> {
        self.shutdown(conn_id).await
    }

    /// Whether this transport can carry LSP traffic on the current platform.
    fn is_available(&self) -> bool;
}

/// Source of the passive `<new-diagnostics>` reminder. The orchestrator polls
/// this once per turn and, when it returns a block, injects it as a transient
/// meta user message — claude-code's `formatDiagnosticsBlock` flow. Implemented
/// by the LSP diagnostic registry (which dedups against already-surfaced
/// diagnostics). A separate trait so the orchestrator need not depend on `lsp`.
#[async_trait]
pub trait NewDiagnosticsSource: Send + Sync {
    /// The next `<new-diagnostics>` block for diagnostics not yet surfaced to
    /// the model, or `None` when there are none.
    async fn take_new_diagnostics_block(&self) -> Option<String>;

    /// Release any workspace-scoped resources owned by this source.
    ///
    /// Most sources are passive views over a process-wide registry and need no
    /// cleanup. Mobile workflow sources override this hook so the final agent
    /// using an app workspace shuts down its LSP child and clears diagnostics.
    async fn close(&self) {}
}

/// Failure modes shared by every [`LspTransport`] method.
#[derive(Debug, Clone, Error)]
pub enum LspError {
    /// Transport is not implemented on the current platform.
    #[error("unavailable")]
    Unavailable,
    /// Underlying transport (stdio / TCP) failed.
    #[error("{0}")]
    Transport(String),
    /// Server returned a JSON-RPC error response.
    #[error("{0}")]
    ServerError(String),
}
