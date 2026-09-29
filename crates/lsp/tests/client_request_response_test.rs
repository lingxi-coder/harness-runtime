//! Integration test: `LspClient::initialize` over an in-memory duplex pipe.
//!
//! Wire-shape verification: the client must send a JSON-RPC request named
//! `"initialize"` with `process_id`, `root_uri`, `capabilities` per the LSP
//! 3.17 spec, framed with `Content-Length` headers, then follow with an
//! `"initialized"` notification once the server has responded.

use jsonrpc::Connection;
use lingxi_core::host::LspServerConfig;
use lsp::client::LspClient;
use lsp_types::ServerCapabilities;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::{BytesCodec, FramedWrite};
use tokio_util::io::ReaderStream;

const FRAME_BUFFER: usize = 64 * 1024;

/// Read one `Content-Length`-framed JSON-RPC message off a reader.
async fn read_one_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Value {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    // Read until \r\n\r\n
    loop {
        reader.read_exact(&mut byte).await.expect("read header");
        header.push(byte[0]);
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let header_str = std::str::from_utf8(&header).expect("utf8 header");
    let len: usize = header_str
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length: "))
        .expect("content-length present")
        .trim()
        .parse()
        .expect("parse length");
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.expect("read body");
    serde_json::from_slice(&body).expect("parse json")
}

/// Write a `Content-Length`-framed JSON-RPC response.
async fn write_frame(writer: &mut (impl tokio::io::AsyncWrite + Unpin), val: &Value) {
    let body = serde_json::to_vec(val).expect("serialize");
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer
        .write_all(header.as_bytes())
        .await
        .expect("write header");
    writer.write_all(&body).await.expect("write body");
    writer.flush().await.expect("flush");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_sends_canonical_lsp_params_and_receives_capabilities() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);

    let connection = Connection::new_lsp(client_read, client_write);
    let client = LspClient::new("rust-analyzer".to_string(), connection);
    let config = LspServerConfig {
        name: "rust-analyzer".to_string(),
        command: "rust-analyzer".to_string(),
        extension_to_language: [(".rs".to_string(), "rust".to_string())]
            .into_iter()
            .collect(),
        initialization_options: Some(json!({ "checkOnSave": true })),
        settings: Some(json!({
            "rust-analyzer": { "check": { "command": "clippy" } }
        })),
        ..Default::default()
    };
    client
        .register_workspace_configuration(config.settings.clone())
        .await;

    // Spawn the peer: read initialize, respond with capabilities.
    let peer_task = tokio::spawn(async move {
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "initialize");
        let id = req["id"].clone();
        assert!(id.is_number() || id.is_string(), "id present");
        let params = &req["params"];
        assert!(params["processId"].is_number());
        assert_eq!(params["clientInfo"]["name"], "Claude Code");
        // Derived, not spelled: the oracle reads this from the same build
        // VERSION as the User-Agent and `AI_AGENT` stamps, and a literal here
        // is how the LSP copy drifted behind the constant before 2026-09-10.
        assert_eq!(
            params["clientInfo"]["version"],
            lingxi_core::host::CLAUDE_CODE_VERSION
        );
        assert_eq!(
            params["initializationOptions"],
            json!({ "checkOnSave": true })
        );
        assert_eq!(params["rootPath"], "/tmp/workspace");
        assert_eq!(params["rootUri"], "file:///tmp/workspace");
        assert_eq!(
            params["workspaceFolders"],
            json!([{
                "uri": "file:///tmp/workspace",
                "name": "workspace",
            }])
        );
        assert_eq!(
            params["capabilities"]["general"]["positionEncodings"],
            json!(["utf-16"])
        );
        assert_eq!(
            params["capabilities"]["workspace"],
            json!({
                "configuration": true,
                "workspaceFolders": false,
            })
        );
        assert_eq!(
            params["capabilities"]["textDocument"]["synchronization"],
            json!({
                "dynamicRegistration": false,
                "willSave": false,
                "willSaveWaitUntil": false,
                "didSave": true,
            })
        );
        assert_eq!(
            params["capabilities"]["textDocument"]["publishDiagnostics"],
            json!({
                "relatedInformation": true,
                "tagSupport": { "valueSet": [1, 2] },
                "versionSupport": true,
                "codeDescriptionSupport": true,
                "dataSupport": false,
            })
        );

        // Claude registers this handler before initialize so servers can pull
        // nested settings during their own startup.
        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": 77,
                "method": "workspace/configuration",
                "params": { "items": [
                    { "section": "rust-analyzer.check" },
                    { "section": "missing" },
                    {}
                ] }
            }),
        )
        .await;
        let configuration = read_one_frame(&mut peer_io).await;
        assert_eq!(configuration["id"], 77);
        assert_eq!(
            configuration["result"],
            json!([
                { "command": "clippy" },
                null,
                { "rust-analyzer": { "check": { "command": "clippy" } } }
            ])
        );

        let response = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "capabilities": {
                    "hoverProvider": true,
                    "definitionProvider": true,
                    "referencesProvider": true,
                }
            }
        });
        write_frame(&mut peer_io, &response).await;

        // Drain the follow-up `initialized` notification so the connection
        // does not hang waiting to write into a full buffer.
        let notif = read_one_frame(&mut peer_io).await;
        assert_eq!(notif["method"], "initialized");
        assert!(notif.get("id").is_none(), "notifications have no id");

        let settings = read_one_frame(&mut peer_io).await;
        assert_eq!(settings["method"], "workspace/didChangeConfiguration");
        assert_eq!(
            settings["params"],
            json!({ "settings": {
                "rust-analyzer": { "check": { "command": "clippy" } }
            }})
        );
    });

    let caps: ServerCapabilities = client
        .initialize("file:///tmp/workspace", &config)
        .await
        .expect("initialize ok");
    assert!(caps.hover_provider.is_some(), "hover advertised");
    assert!(caps.definition_provider.is_some(), "definition advertised");
    assert_eq!(client.name(), "rust-analyzer");

    peer_task.await.expect("peer task ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_hover_round_trips_and_deserializes() {
    use lsp_types::{
        Hover, HoverContents, MarkedString, Position, TextDocumentIdentifier,
        TextDocumentPositionParams,
    };

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = LspClient::new("test-server".to_string(), connection);

    let peer_task = tokio::spawn(async move {
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/hover");
        assert_eq!(req["params"]["textDocument"]["uri"], "file:///tmp/foo.rs");
        assert_eq!(req["params"]["position"]["line"], 4);
        assert_eq!(req["params"]["position"]["character"], 2);

        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"contents": "Hello hover"}
            }),
        )
        .await;
    });

    let params = TextDocumentPositionParams {
        text_document: TextDocumentIdentifier {
            uri: lsp_types::Url::parse("file:///tmp/foo.rs").unwrap(),
        },
        position: Position {
            line: 4,
            character: 2,
        },
    };
    let hover: Option<Hover> = client
        .request("textDocument/hover", params)
        .await
        .expect("hover ok");
    let hover = hover.expect("hover present");
    match hover.contents {
        HoverContents::Scalar(MarkedString::String(s)) => assert_eq!(s, "Hello hover"),
        other => panic!("unexpected hover contents shape: {other:?}"),
    }

    peer_task.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notify_surfaces_closed_connection_error() {
    let (client_io, _peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = LspClient::new("test-server".to_string(), connection);
    client.connection().close();

    let error = client
        .notify("initialized", json!({}))
        .await
        .expect_err("closed connection should reject notifications");
    assert!(
        matches!(error, lingxi_core::host::LspError::Transport(ref message) if message.contains("LSP notification 'initialized' failed") && message.contains("writer closed")),
        "expected closed-notify transport error, got {error:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn content_modified_retries_three_times_with_stable_params() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = LspClient::new("test-server".to_string(), connection);

    let peer_task = tokio::spawn(async move {
        for attempt in 0..=3 {
            let request = read_one_frame(&mut peer_io).await;
            assert_eq!(request["method"], "textDocument/hover");
            assert_eq!(request["params"], json!({"stable": true}));
            let response = if attempt < 3 {
                json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "error": { "code": -32801, "message": "content changed" }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": { "ok": true }
                })
            };
            write_frame(&mut peer_io, &response).await;
        }
    });

    let result: Value = client
        .request("textDocument/hover", json!({"stable": true}))
        .await
        .expect("fourth attempt succeeds");
    assert_eq!(result, json!({"ok": true}));
    peer_task.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_without_configured_timeout_ignores_connection_default() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::builder(jsonrpc::LspCodec::default())
        .default_timeout(Duration::from_millis(20))
        .build(
            ReaderStream::new(client_read),
            FramedWrite::new(client_write, BytesCodec::new()),
        );
    let client = Arc::new(LspClient::new("test-server".to_string(), connection));

    let peer = tokio::spawn(async move {
        let shutdown = read_one_frame(&mut peer_io).await;
        assert_eq!(shutdown["method"], "shutdown");

        // The connection default is 20ms, but an unset shutdownTimeout must be
        // unbounded. No early `exit` notification may appear while the server
        // is still preparing its shutdown response.
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), read_one_frame(&mut peer_io))
                .await
                .is_err(),
            "default request timeout must not end an unbounded shutdown"
        );

        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": shutdown["id"],
                "result": null
            }),
        )
        .await;
        let exit = read_one_frame(&mut peer_io).await;
        assert_eq!(exit["method"], "exit");
        assert_eq!(exit["params"], json!({}));
    });

    client
        .shutdown_with_timeout(None)
        .await
        .expect("shutdown succeeds after delayed response");
    peer.await.expect("peer task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_without_startup_timeout_ignores_connection_default() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::builder(jsonrpc::LspCodec::default())
        .default_timeout(Duration::from_millis(20))
        .build(
            ReaderStream::new(client_read),
            FramedWrite::new(client_write, BytesCodec::new()),
        );
    let client = LspClient::new("test-server".to_string(), connection);
    let config = LspServerConfig {
        name: "test-server".to_string(),
        command: "test-server".to_string(),
        ..Default::default()
    };

    let peer = tokio::spawn(async move {
        let initialize = read_one_frame(&mut peer_io).await;
        assert_eq!(initialize["method"], "initialize");

        tokio::time::sleep(Duration::from_millis(60)).await;
        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": initialize["id"],
                "result": { "capabilities": {} }
            }),
        )
        .await;

        let initialized =
            tokio::time::timeout(Duration::from_millis(50), read_one_frame(&mut peer_io))
                .await
                .expect("initialized notification follows successful initialize");
        assert_eq!(initialized["method"], "initialized");
    });

    client
        .initialize("file:///tmp/workspace", &config)
        .await
        .expect("initialize succeeds after delayed response");
    peer.await.expect("peer task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_timeout_does_not_send_exit() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("test-server".to_string(), connection));
    let client_task = tokio::spawn({
        let client = Arc::clone(&client);
        async move { client.shutdown_with_timeout(Some(20)).await }
    });

    let peer = tokio::spawn(async move {
        let shutdown = read_one_frame(&mut peer_io).await;
        assert_eq!(shutdown["method"], "shutdown");

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), read_one_frame(&mut peer_io))
                .await
                .is_err(),
            "timed-out shutdown must not send exit"
        );
    });

    let error = client_task
        .await
        .expect("client task")
        .expect_err("shutdown must time out");
    assert!(
        matches!(error, lingxi_core::host::LspError::Transport(ref message) if message.contains("shutdown")),
        "expected transport shutdown timeout, got {error:?}"
    );
    peer.await.expect("peer task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_server_error_does_not_send_exit() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = LspClient::new("test-server".to_string(), connection);

    let peer = tokio::spawn(async move {
        let shutdown = read_one_frame(&mut peer_io).await;
        assert_eq!(shutdown["method"], "shutdown");
        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": shutdown["id"],
                "error": { "code": -32603, "message": "shutdown failed" }
            }),
        )
        .await;

        assert!(
            tokio::time::timeout(Duration::from_millis(20), read_one_frame(&mut peer_io))
                .await
                .is_err(),
            "errored shutdown must not send exit"
        );
    });

    let error = client
        .shutdown()
        .await
        .expect_err("shutdown server error must surface");
    assert!(
        matches!(error, lingxi_core::host::LspError::ServerError(ref message) if message.contains("shutdown failed")),
        "expected server shutdown failure, got {error:?}"
    );
    peer.await.expect("peer task");
}
