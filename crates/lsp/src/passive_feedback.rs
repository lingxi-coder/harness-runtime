//! `PassiveDiagnosticSubscriber` — listens for
//! `textDocument/publishDiagnostics` on a `jsonrpc::Connection` and
//! drains the diagnostics into an [`LspDiagnosticRegistry`].
//!
//! Modelled after claude-code, where the LSP `LSPServerInstance` registers
//! a notification handler that forwards diagnostics to the assistant's
//! "current file diagnostics" cache. The Rust shape here is a background
//! tokio task spawned per LSP server connection.
//!
//! Implementation note: `jsonrpc::Connection` exposes a single
//! `notifications()` broadcast receiver that fans out *every* inbound
//! notification (no per-method subscription). We resubscribe to a fresh
//! receiver and filter for `textDocument/publishDiagnostics` ourselves —
//! the broadcast queue is sized inside `lingxi-jsonrpc`, so a slow
//! subscriber will eventually report `Lagged`; we log and continue rather
//! than aborting (other diagnostics may still be useful).

use crate::diagnostic_registry::{DiagnosticEntry, LspDiagnosticRegistry};
use jsonrpc::Connection;
use lsp_types::{PublishDiagnosticsParams, Url};
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// Method name the subscriber listens for. Exposed for tests / docs.
pub const PUBLISH_DIAGNOSTICS_METHOD: &str = "textDocument/publishDiagnostics";

/// Background drain of `publishDiagnostics` notifications into a registry.
pub struct PassiveDiagnosticSubscriber {
    handle: JoinHandle<()>,
    registry: LspDiagnosticRegistry,
}

impl PassiveDiagnosticSubscriber {
    /// Subscribe to `textDocument/publishDiagnostics` on `connection` and
    /// drain into `registry`. Returns immediately; a background tokio task
    /// continues until the connection's broker is closed.
    ///
    /// The `connection` argument is taken by reference because the
    /// subscriber only needs it to acquire a fresh notifications receiver
    /// (the receiver itself is decoupled from the `Arc`). Pass
    /// `&Arc::clone(&conn)` or just `&conn` — both work.
    #[must_use]
    pub fn spawn(
        connection: &Arc<Connection>,
        server_name: String,
        registry: LspDiagnosticRegistry,
    ) -> Self {
        let mut receiver = connection.notifications();
        let drain_registry = registry.clone();
        let drain_name = server_name;
        let handle = tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(notification) => {
                        if notification.method != PUBLISH_DIAGNOSTICS_METHOD {
                            continue;
                        }
                        let params_value = notification.params.unwrap_or(serde_json::Value::Null);
                        match serde_json::from_value::<PublishDiagnosticsParams>(params_value) {
                            Ok(params) => {
                                let entry = DiagnosticEntry {
                                    version: params.version,
                                    diagnostics: params.diagnostics,
                                };
                                drain_registry.publish(params.uri, entry).await;
                            }
                            Err(err) => {
                                warn!(
                                    target: "lingxi_lsp::passive_feedback",
                                    server = %drain_name,
                                    error = %err,
                                    "failed to decode publishDiagnostics params"
                                );
                            }
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        warn!(
                            target: "lingxi_lsp::passive_feedback",
                            server = %drain_name,
                            skipped = skipped,
                            "publishDiagnostics broadcast lagged; some notifications dropped"
                        );
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        debug!(
                            target: "lingxi_lsp::passive_feedback",
                            server = %drain_name,
                            "publishDiagnostics broker closed; subscriber exiting"
                        );
                        break;
                    }
                }
            }
        });
        Self { handle, registry }
    }

    /// Borrow the diagnostic registry being drained into.
    #[must_use]
    pub fn registry(&self) -> &LspDiagnosticRegistry {
        &self.registry
    }

    /// Abort the background task. Safe to call multiple times.
    pub fn abort(&self) {
        self.handle.abort();
    }
}

impl Drop for PassiveDiagnosticSubscriber {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Helper test-only shim that lets unit tests publish synthetic
/// notifications without driving a real LSP server. Decodes the params and
/// writes directly to the registry.
#[doc(hidden)]
pub async fn publish_for_test(
    registry: &LspDiagnosticRegistry,
    uri: Url,
    diagnostics: Vec<lsp_types::Diagnostic>,
    version: Option<i32>,
) {
    registry
        .publish(
            uri,
            DiagnosticEntry {
                version,
                diagnostics,
            },
        )
        .await;
}
