//! `LspClient` — typed JSON-RPC wrapper around a `jsonrpc::Connection`.
//!
//! Mirrors claude-code's `LSPClient` (`claude-code/src/services/lsp/LSPClient.ts`)
//! in shape: typed `initialize`, untyped `request<P, R>` for arbitrary LSP
//! methods, fire-and-forget `notify`, and a `shutdown` sequence that issues
//! the canonical LSP `shutdown` request + `exit` notification.
//!
//! The `Connection` itself is produced by the platform layer (it owns the
//! child process and the stdio pipes); the client does not spawn anything.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jsonrpc::{Connection, ConnectionError};
use lsp_types::{
    InitializeResult, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use platform_api::{LspError, LspServerConfig};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tracing::{debug, warn};

const CONTENT_MODIFIED: i32 = -32801;
const CONTENT_MODIFIED_RETRIES: u32 = 3;
const CONTENT_MODIFIED_BASE_DELAY_MS: u64 = 500;
/// Oracle `clientInfo:{name:"Claude Code",version:{…}.VERSION}` — the same
/// build VERSION the User-Agent and `AI_AGENT` stamps read, so it tracks the
/// one constant instead of keeping a copy that has to be remembered.
const LSP_CLIENT_VERSION: &str = platform_api::CLAUDE_CODE_VERSION;

/// Typed client over a JSON-RPC connection to one LSP server.
pub struct LspClient {
    /// Human-readable server name (for tracing).
    name: String,
    /// JSON-RPC plumbing — handles framing, routing, notifications.
    connection: Arc<Connection>,
    /// Server capabilities populated after `initialize` succeeds.
    capabilities: RwLock<Option<ServerCapabilities>>,
}

impl LspClient {
    /// Build a new client around an already-established `Connection`.
    ///
    /// The connection must have been started (its reader/writer tasks
    /// spawned). The client takes shared ownership and uses it for the
    /// lifetime of the LSP server.
    #[must_use]
    pub fn new(name: String, connection: Connection) -> Self {
        Self {
            name,
            connection: Arc::new(connection),
            capabilities: RwLock::new(None),
        }
    }

    /// Construct an `LspClient` from an already-shared `Arc<Connection>`.
    #[must_use]
    pub fn with_shared(name: String, connection: Arc<Connection>) -> Self {
        Self {
            name,
            connection,
            capabilities: RwLock::new(None),
        }
    }

    /// Borrow the underlying JSON-RPC connection (used by `passive_feedback`
    /// to subscribe to notifications).
    #[must_use]
    pub fn connection(&self) -> Arc<Connection> {
        Arc::clone(&self.connection)
    }

    /// LSP server name (registry key).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Latest cached capabilities; `None` until `initialize` succeeds.
    pub async fn capabilities(&self) -> Option<ServerCapabilities> {
        self.capabilities.read().await.clone()
    }

    /// Register Claude Code's always-present `workspace/configuration`
    /// request handler before the initialize handshake starts.
    pub async fn register_workspace_configuration(&self, settings: Option<Value>) {
        self.connection
            .register_handler(
                "workspace/configuration",
                Arc::new(WorkspaceConfigurationHandler { settings }),
            )
            .await;
    }

    /// Perform the LSP `initialize` handshake.
    ///
    /// Sends an `initialize` request followed by the `initialized`
    /// notification per LSP 3.17 spec. Caches the returned
    /// `ServerCapabilities` for later inspection.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] when the request cannot be delivered,
    /// or [`LspError::ServerError`] when the server returns a JSON-RPC error.
    pub async fn initialize(
        &self,
        root_uri: &str,
        config: &LspServerConfig,
    ) -> Result<ServerCapabilities, LspError> {
        let parsed_root = lsp_types::Url::parse(root_uri)
            .map_err(|e| LspError::Transport(format!("invalid root_uri: {e}")))?;
        let root_path = parsed_root
            .to_file_path()
            .map_err(|()| LspError::Transport(format!("root_uri is not a file URL: {root_uri}")))?;
        let workspace_name = root_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let params = json!({
            "processId": std::process::id(),
            "clientInfo": {
                "name": "Claude Code",
                "version": LSP_CLIENT_VERSION,
            },
            "initializationOptions": config.initialization_options.clone().unwrap_or_else(|| json!({})),
            "workspaceFolders": [{
                "uri": root_uri,
                "name": workspace_name,
            }],
            "rootPath": path_wire_string(&root_path),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "configuration": config.settings.is_some(),
                    "workspaceFolders": false,
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "willSave": false,
                        "willSaveWaitUntil": false,
                        "didSave": true,
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true,
                        "tagSupport": { "valueSet": [1, 2] },
                        "versionSupport": true,
                        "codeDescriptionSupport": true,
                        "dataSupport": false,
                    },
                    "hover": {
                        "dynamicRegistration": false,
                        "contentFormat": ["markdown", "plaintext"],
                    },
                    "definition": {
                        "dynamicRegistration": false,
                        "linkSupport": true,
                    },
                    "references": { "dynamicRegistration": false },
                    "documentSymbol": {
                        "dynamicRegistration": false,
                        "hierarchicalDocumentSymbolSupport": true,
                    },
                    "callHierarchy": { "dynamicRegistration": false },
                },
                "general": { "positionEncodings": ["utf-16"] },
            },
        });

        let result_value = if let Some(timeout_ms) = config.startup_timeout {
            self.connection
                .call_with_timeout("initialize", params, Duration::from_millis(timeout_ms))
                .await
        } else {
            self.connection.call_unbounded("initialize", params).await
        }
        .map_err(|e| self.map_connection_error("initialize", e))?;
        let result: InitializeResult = serde_json::from_value(result_value)
            .map_err(|e| LspError::Transport(format!("decode initialize result: {e}")))?;

        // Cache capabilities then send the `initialized` notification.
        *self.capabilities.write().await = Some(result.capabilities.clone());
        self.notify("initialized", json!({})).await?;
        if let Some(settings) = &config.settings {
            self.notify_did_change_configuration_best_effort(settings)
                .await;
        }
        debug!(
            target: "lingxi_lsp::client",
            server = %self.name,
            "initialize complete"
        );
        Ok(result.capabilities)
    }

    /// Issue a JSON-RPC request and decode the response.
    ///
    /// `method` is the LSP method name (e.g. `"textDocument/hover"`).
    /// `params` is serialized as the request `params` field; the response
    /// `result` is deserialized into `R`.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] on transport failure or serialization
    /// errors, and [`LspError::ServerError`] when the server returns a
    /// JSON-RPC error response.
    pub async fn request<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R, LspError> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| LspError::Transport(format!("serialize {method} params: {e}")))?;
        let mut attempt = 0;
        let result_value: Value = loop {
            match self.connection.call(method, params_value.clone()).await {
                Ok(value) => break value,
                Err(ConnectionError::Router(jsonrpc::router::RouterError::Remote(remote)))
                    if remote.code == CONTENT_MODIFIED && attempt < CONTENT_MODIFIED_RETRIES =>
                {
                    let delay_ms = CONTENT_MODIFIED_BASE_DELAY_MS << attempt;
                    attempt += 1;
                    debug!(
                        target: "lingxi_lsp::client",
                        server = %self.name,
                        method,
                        delay_ms,
                        attempt,
                        max_attempts = CONTENT_MODIFIED_RETRIES,
                        "ContentModified; retrying LSP request"
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Err(error) => return Err(self.map_connection_error(method, error)),
            }
        };
        serde_json::from_value(result_value)
            .map_err(|e| LspError::Transport(format!("decode {method} result: {e}")))
    }

    /// Issue a JSON-RPC notification (fire-and-forget).
    ///
    /// Kept `async` for symmetry with [`Self::request`] / [`Self::shutdown`]
    /// — callers can `.await` notify alongside request calls without
    /// branching their code paths, and the signature is forward-compatible
    /// if the underlying router ever needs to await backpressure.
    ///
    /// # Errors
    /// Returns [`LspError::Transport`] on parameter serialization or delivery
    /// failure.
    #[allow(clippy::unused_async)]
    pub async fn notify<P: Serialize>(&self, method: &str, params: P) -> Result<(), LspError> {
        let params_value = serde_json::to_value(params)
            .map_err(|e| LspError::Transport(format!("serialize {method} notify: {e}")))?;
        self.connection
            .notify(method, params_value)
            .map_err(|error| self.map_notification_error(method, &error))
    }

    /// LSP shutdown sequence — `shutdown` request followed by `exit`
    /// notification, per LSP 3.17 `§Lifecycle Messages`.
    ///
    /// Some servers don't respond cleanly to `shutdown`; we log those
    /// failures and let the transport layer tear the process down instead of
    /// sending a misleading `exit` after the protocol failed.
    ///
    /// # Errors
    /// Returns request / decode errors directly. After a successful `shutdown`
    /// response, failure to deliver `exit` also propagates.
    pub async fn shutdown(&self) -> Result<(), LspError> {
        self.shutdown_with_timeout(None).await
    }

    /// Claude Code-compatible shutdown with an optional server-configured
    /// deadline.
    pub async fn shutdown_with_timeout(&self, timeout_ms: Option<u64>) -> Result<(), LspError> {
        let request = async {
            let raw = if let Some(timeout_ms) = timeout_ms {
                self.connection
                    .call_with_timeout("shutdown", json!({}), Duration::from_millis(timeout_ms))
                    .await
            } else {
                self.connection.call_unbounded("shutdown", json!({})).await
            }
            .map_err(|error| self.map_connection_error("shutdown", error))?;
            serde_json::from_value::<Option<Value>>(raw)
                .map_err(|error| LspError::Transport(format!("decode shutdown result: {error}")))
        };
        match request.await {
            Ok(_) => {
                debug!(
                    target: "lingxi_lsp::client",
                    server = %self.name,
                    "shutdown ack"
                );
                self.notify("exit", json!({})).await
            }
            Err(error) => {
                warn!(
                    target: "lingxi_lsp::client",
                    server = %self.name,
                    error = %error,
                    "shutdown errored; skipping exit"
                );
                Err(error)
            }
        }
    }

    fn map_connection_error(&self, method: &str, err: ConnectionError) -> LspError {
        use jsonrpc::router::RouterError;
        match err {
            ConnectionError::Router(RouterError::Remote(remote)) => LspError::ServerError(format!(
                "LSP request '{method}' failed for server '{}': {}",
                self.name, remote.message
            )),
            other => LspError::Transport(format!(
                "LSP request '{method}' failed for server '{}': {other}",
                self.name
            )),
        }
    }

    fn map_notification_error(&self, method: &str, err: &ConnectionError) -> LspError {
        LspError::Transport(format!(
            "LSP notification '{method}' failed for server '{}': {err}",
            self.name
        ))
    }

    async fn notify_did_change_configuration_best_effort(&self, settings: &Value) {
        if let Err(error) = self
            .notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            )
            .await
        {
            warn!(
                target: "lingxi_lsp::client",
                server = %self.name,
                method = "workspace/didChangeConfiguration",
                error = %error,
                "LSP notification failed; continuing"
            );
        }
    }
}

fn path_wire_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Stable label for the server's advertised text-document synchronization
/// mode, preserving `none`/`full`/`incremental` instead of collapsing every
/// capability to full sync.
#[must_use]
pub fn text_document_sync_label(capability: Option<&TextDocumentSyncCapability>) -> Option<String> {
    let kind = match capability? {
        TextDocumentSyncCapability::Kind(kind) => Some(*kind),
        TextDocumentSyncCapability::Options(options) => options.change,
    }?;
    Some(
        if kind == TextDocumentSyncKind::NONE {
            "none"
        } else if kind == TextDocumentSyncKind::FULL {
            "full"
        } else if kind == TextDocumentSyncKind::INCREMENTAL {
            "incremental"
        } else {
            "unknown"
        }
        .to_string(),
    )
}

struct WorkspaceConfigurationHandler {
    settings: Option<Value>,
}

#[async_trait::async_trait]
impl jsonrpc::InboundHandler for WorkspaceConfigurationHandler {
    async fn handle(&self, request: jsonrpc::Request) -> jsonrpc::Response {
        let values = request
            .params
            .as_ref()
            .and_then(|params| params.get("items"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        let section = item.get("section").and_then(Value::as_str);
                        configuration_section(self.settings.as_ref(), section)
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        jsonrpc::Response::success(request.id, Value::Array(values))
    }
}

fn configuration_section(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(mut current) = settings else {
        return Value::Null;
    };
    let Some(section) = section.filter(|section| !section.is_empty()) else {
        return current.clone();
    };
    for part in section.split('.') {
        let Some(next) = current.as_object().and_then(|object| object.get(part)) else {
            return Value::Null;
        };
        current = next;
    }
    current.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::duplex;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn did_change_configuration_remains_best_effort() {
        let (client_io, _peer_io) = duplex(1024);
        let (client_read, client_write) = tokio::io::split(client_io);
        let connection = Connection::new_lsp(client_read, client_write);
        let client = LspClient::new("test-server".to_string(), connection);
        client.connection().close();

        let error = client
            .notify(
                "workspace/didChangeConfiguration",
                json!({ "settings": {} }),
            )
            .await
            .expect_err("ordinary notifications should surface delivery errors");
        assert!(
            matches!(error, LspError::Transport(ref message) if message.contains("workspace/didChangeConfiguration")),
            "expected didChangeConfiguration notify error, got {error:?}"
        );

        client
            .notify_did_change_configuration_best_effort(&json!({ "test-server": true }))
            .await;
    }
}
