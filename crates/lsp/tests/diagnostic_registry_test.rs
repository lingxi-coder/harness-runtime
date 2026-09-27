//! Unit tests for `LspDiagnosticRegistry` and a flow test that drives
//! `PassiveDiagnosticSubscriber` end-to-end over an in-memory LSP-framed
//! `Connection`.

use jsonrpc::Connection;
use lsp::passive_feedback::publish_for_test;
use lsp::{DiagnosticEntry, LspDiagnosticRegistry, PassiveDiagnosticSubscriber};
use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{duplex, AsyncWriteExt};

/// Helper: write one `Content-Length`-framed JSON-RPC notification.
async fn write_frame(w: &mut (impl tokio::io::AsyncWrite + Unpin), val: &serde_json::Value) {
    let body = serde_json::to_vec(val).unwrap();
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    w.write_all(header.as_bytes()).await.unwrap();
    w.write_all(&body).await.unwrap();
    w.flush().await.unwrap();
}

fn diag(message: &str, severity: DiagnosticSeverity) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: 0,
                character: 1,
            },
        },
        severity: Some(severity),
        message: message.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn latest_publish_wins() {
    let registry = LspDiagnosticRegistry::new();
    let uri = Url::parse("file:///tmp/a.rs").unwrap();

    publish_for_test(
        &registry,
        uri.clone(),
        vec![diag("first", DiagnosticSeverity::WARNING)],
        Some(1),
    )
    .await;

    publish_for_test(
        &registry,
        uri.clone(),
        vec![diag("second", DiagnosticSeverity::ERROR)],
        Some(2),
    )
    .await;

    let latest = registry.get(&uri).await;
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].message, "second");
    assert_eq!(latest[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(registry.len().await, 1);
}

#[tokio::test]
async fn stale_version_is_dropped() {
    let registry = LspDiagnosticRegistry::new();
    let uri = Url::parse("file:///tmp/a.rs").unwrap();

    registry
        .publish(
            uri.clone(),
            DiagnosticEntry {
                version: Some(5),
                diagnostics: vec![diag("v5", DiagnosticSeverity::ERROR)],
            },
        )
        .await;

    registry
        .publish(
            uri.clone(),
            DiagnosticEntry {
                version: Some(3),
                diagnostics: vec![diag("v3-stale", DiagnosticSeverity::WARNING)],
            },
        )
        .await;

    let latest = registry.get(&uri).await;
    assert_eq!(latest.len(), 1);
    assert_eq!(latest[0].message, "v5");
}

#[tokio::test]
async fn clear_removes_uri() {
    let registry = LspDiagnosticRegistry::new();
    let uri = Url::parse("file:///tmp/a.rs").unwrap();
    publish_for_test(
        &registry,
        uri.clone(),
        vec![diag("x", DiagnosticSeverity::ERROR)],
        None,
    )
    .await;
    assert_eq!(registry.len().await, 1);

    registry.clear(&uri).await;
    assert_eq!(registry.get(&uri).await.len(), 0);
    assert!(registry.is_empty().await);
}

#[tokio::test]
async fn all_diagnostics_returns_every_uri() {
    let registry = LspDiagnosticRegistry::new();
    let uri1 = Url::parse("file:///tmp/a.rs").unwrap();
    let uri2 = Url::parse("file:///tmp/b.rs").unwrap();
    publish_for_test(
        &registry,
        uri1.clone(),
        vec![diag("a", DiagnosticSeverity::ERROR)],
        None,
    )
    .await;
    publish_for_test(
        &registry,
        uri2.clone(),
        vec![diag("b", DiagnosticSeverity::WARNING)],
        None,
    )
    .await;

    let mut all = registry.all_diagnostics().await;
    all.sort_by_key(|(u, _)| u.to_string());
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].0, uri1);
    assert_eq!(all[1].0, uri2);
}

/// End-to-end: spawn a `PassiveDiagnosticSubscriber` over an in-memory
/// LSP-framed Connection, push a `textDocument/publishDiagnostics`
/// notification frame from the "server" side, and verify the diagnostic
/// lands in the registry. Also sends a stray notification on a different
/// method to confirm the filter ignores it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriber_routes_publish_diagnostics_into_registry() {
    const FRAME_BUFFER: usize = 64 * 1024;
    let (client_io, server_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let (_server_read, mut server_write) = tokio::io::split(server_io);

    let connection = Arc::new(Connection::new_lsp(client_read, client_write));
    let registry = LspDiagnosticRegistry::new();
    let subscriber = PassiveDiagnosticSubscriber::spawn(
        &connection,
        "test-server".to_string(),
        registry.clone(),
    );

    // Stray notification on an unrelated method — must NOT touch registry.
    write_frame(
        &mut server_write,
        &json!({
            "jsonrpc": "2.0",
            "method": "window/logMessage",
            "params": {"type": 3, "message": "starting"}
        }),
    )
    .await;

    // The diagnostic notification we actually care about.
    let uri = "file:///tmp/example.rs";
    write_frame(
        &mut server_write,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "version": 7,
                "diagnostics": [{
                    "range": {
                        "start": {"line": 10, "character": 4},
                        "end":   {"line": 10, "character": 12}
                    },
                    "severity": 1,
                    "message": "unused import"
                }]
            }
        }),
    )
    .await;

    // Poll the registry until the entry shows up (or fail after a timeout).
    let parsed_uri = Url::parse(uri).unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let entries = registry.get(&parsed_uri).await;
        if !entries.is_empty() {
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].message, "unused import");
            assert_eq!(entries[0].severity, Some(DiagnosticSeverity::ERROR));
            // The stray window/logMessage notification must not have
            // created any other URI entries.
            assert_eq!(registry.len().await, 1);
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "diagnostic never landed in registry"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    subscriber.abort();
}
