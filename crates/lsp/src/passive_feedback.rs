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
use crate::diagnostics_format::{DiagnosticCodeSource, SourceDiagnostic};
use jsonrpc::Connection;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lsp_types::{DiagnosticSeverity, Range, Url};
use serde::de::DeserializeOwned;
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
                        let Some(params) = notification.params else {
                            warn!(
                                target: "lingxi_lsp::passive_feedback",
                                server = %drain_name,
                                "publishDiagnostics notification omitted params"
                            );
                            continue;
                        };
                        match parse_publish_diagnostics(params.as_json()) {
                            Ok((uri, entry)) => drain_registry.publish(uri, entry).await,
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

fn parse_publish_diagnostics(raw_json: &str) -> Result<(Url, DiagnosticEntry), String> {
    let source = Utf16JsonProjection::parse(raw_json).map_err(|error| error.to_string())?;
    if source
        .at_pointer("")
        .and_then(|value| value.object_entries())
        .is_none()
    {
        return Err("publishDiagnostics params must be an object".to_string());
    }

    // LSP URIs are parsed at their protocol boundary and used as the registry
    // identity. An unpaired surrogate cannot be a valid Rust URL, so reject it
    // here rather than caching under a display-safe replacement string.
    let uri_units = source
        .string_units("/uri")
        .ok_or_else(|| "publishDiagnostics uri must be a string".to_string())?;
    let uri_text = String::from_utf16(&uri_units)
        .map_err(|_| "publishDiagnostics uri contains invalid UTF-16".to_string())?;
    let uri = Url::parse(&uri_text)
        .map_err(|error| format!("invalid publishDiagnostics uri: {error}"))?;

    let version = match source.get_property("version") {
        None => None,
        Some(value) if value.is_null() => None,
        Some(_) => Some(parse_subprojection::<i32>(&source, "/version")?),
    };

    let diagnostics = source
        .get_property("diagnostics")
        .and_then(|value| value.array_len())
        .ok_or_else(|| "publishDiagnostics diagnostics must be an array".to_string())?;
    let mut parsed_diagnostics = Vec::with_capacity(diagnostics);
    for index in 0..diagnostics {
        let pointer = format!("/diagnostics/{index}");
        let diagnostic = source
            .subprojection(&pointer)
            .map_err(|error| error.to_string())?;
        parsed_diagnostics.push(parse_source_diagnostic(&diagnostic)?);
    }

    Ok((
        uri,
        DiagnosticEntry {
            version,
            diagnostics: parsed_diagnostics,
        },
    ))
}

fn parse_source_diagnostic(source: &Utf16JsonProjection) -> Result<SourceDiagnostic, String> {
    if source
        .at_pointer("")
        .and_then(|value| value.object_entries())
        .is_none()
    {
        return Err("diagnostic entry must be an object".to_string());
    }

    let range = parse_subprojection::<Range>(source, "/range")?;
    let message = source
        .subprojection("/message")
        .map_err(|_| "diagnostic message is missing".to_string())?;
    if message.string_units("").is_none() {
        return Err("diagnostic message must be a string".to_string());
    }
    let severity = match source.get_property("severity") {
        None => None,
        Some(value) if value.is_null() => None,
        Some(_) => Some(parse_subprojection::<DiagnosticSeverity>(
            source,
            "/severity",
        )?),
    };
    let source_text = optional_source_string(source, "source")?;
    let code = match source.get_property("code") {
        None => None,
        Some(value) if value.is_null() => None,
        Some(value) if value.code_units().is_some() => {
            let code = source
                .subprojection("/code")
                .map_err(|error| error.to_string())?;
            Some(DiagnosticCodeSource::String(code))
        }
        Some(_) => Some(DiagnosticCodeSource::Number(parse_subprojection::<i32>(
            source, "/code",
        )?)),
    };

    Ok(SourceDiagnostic {
        range,
        severity,
        message,
        source: source_text,
        code,
    })
}

fn optional_source_string(
    source: &Utf16JsonProjection,
    property: &str,
) -> Result<Option<Utf16JsonProjection>, String> {
    match source.get_property(property) {
        None => Ok(None),
        Some(value) if value.is_null() => Ok(None),
        Some(value) if value.code_units().is_some() => source
            .subprojection(&format!("/{property}"))
            .map(Some)
            .map_err(|error| error.to_string()),
        Some(_) => Err(format!("diagnostic {property} must be a string")),
    }
}

fn parse_subprojection<T: DeserializeOwned>(
    source: &Utf16JsonProjection,
    pointer: &str,
) -> Result<T, String> {
    let field = source
        .subprojection(pointer)
        .map_err(|error| format!("invalid diagnostic field {pointer}: {error}"))?;
    let json = field.to_json_string().map_err(|error| error.to_string())?;
    serde_json::from_str(&json)
        .map_err(|error| format!("invalid diagnostic field {pointer}: {error}"))
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
    diagnostics: Vec<SourceDiagnostic>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_diagnostics_preserves_exact_message_source_and_code_units() {
        let (uri, entry) = parse_publish_diagnostics(
            r#"{"uri":"file:///workspace/a.ts","version":3,"diagnostics":[{"range":{"start":{"line":1,"character":2},"end":{"line":1,"character":3}},"severity":1,"message":"bad\ud800","source":"lsp\udfff","code":"E\ud800"}]}"#,
        )
        .expect("source notification parses");
        assert_eq!(uri.as_str(), "file:///workspace/a.ts");
        assert_eq!(entry.version, Some(3));
        let diagnostic = &entry.diagnostics[0];
        assert_eq!(
            diagnostic.message.string_units(""),
            Some(vec![b'b' as u16, b'a' as u16, b'd' as u16, 0xd800])
        );
        assert_eq!(
            diagnostic.source.as_ref().unwrap().string_units(""),
            Some(vec![b'l' as u16, b's' as u16, b'p' as u16, 0xdfff])
        );
        let Some(DiagnosticCodeSource::String(code)) = &diagnostic.code else {
            panic!("expected source string code")
        };
        assert_eq!(code.string_units(""), Some(vec![b'E' as u16, 0xd800]));
    }

    #[test]
    fn publish_diagnostics_rejects_invalid_uri_units_at_url_boundary() {
        let error = parse_publish_diagnostics(r#"{"uri":"file:///bad\ud800.ts","diagnostics":[]}"#)
            .expect_err("unpaired URI is not a valid URL");
        assert!(error.contains("invalid UTF-16"));
    }
}
