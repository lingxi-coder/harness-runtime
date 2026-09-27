//! Integration tests for `tool_operations` — the nine LSP tool operations.
//!
//! The tests spin up an in-memory peer over `Connection::new_lsp` and
//! verify the wire shape of the requests + the 1-based → 0-based position
//! conversion + the didOpen-gating + the file-size cap.

use jsonrpc::Connection;
use lsp::client::LspClient;
use lsp::tool_operations::{
    document_symbol, find_references, go_to_definition, go_to_implementation, hover,
    incoming_calls, language_id_for, outgoing_calls, position_from_one_based,
    prepare_call_hierarchy, workspace_symbol, LspOperation, LspOperationError,
    MAX_LSP_FILE_SIZE_BYTES,
};
use lsp::{OpenFileTracker, MAX_OPEN_DOCUMENTS};
use platform_api::LspServerConfig;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

const FRAME_BUFFER: usize = 64 * 1024;

async fn read_one_frame(reader: &mut (impl tokio::io::AsyncRead + Unpin)) -> Value {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
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

/// Minimal config with `.rs → rust` mapping for the test fixtures.
fn rust_config() -> LspServerConfig {
    let mut map = HashMap::new();
    map.insert(".rs".to_string(), "rust".to_string());
    LspServerConfig {
        name: "rust-analyzer".to_string(),
        command: "rust-analyzer".to_string(),
        args: vec![],
        env: HashMap::new(),
        trigger_languages: vec!["rust".to_string()],
        root_dir_markers: vec!["Cargo.toml".to_string()],
        initialization_options: None,
        extension_to_language: map,
        ..Default::default()
    }
}

// ---- pure-function unit tests -------------------------------------------

#[test]
fn position_from_one_based_subtracts_one() {
    let p = position_from_one_based(5, 3).expect("ok");
    assert_eq!(p.line, 4);
    assert_eq!(p.character, 2);
}

#[test]
fn position_from_one_based_rejects_zero_line() {
    let err = position_from_one_based(0, 3).expect_err("must error");
    assert!(matches!(err, LspOperationError::InvalidPosition { .. }));
}

#[test]
fn position_from_one_based_rejects_zero_character() {
    let err = position_from_one_based(2, 0).expect_err("must error");
    assert!(matches!(err, LspOperationError::InvalidPosition { .. }));
}

#[test]
fn language_id_for_uses_extension_map() {
    let cfg = rust_config();
    assert_eq!(language_id_for(&cfg, &PathBuf::from("/tmp/x.rs")), "rust");
}

#[test]
fn language_id_for_falls_back_to_plaintext() {
    let cfg = rust_config();
    assert_eq!(
        language_id_for(&cfg, &PathBuf::from("/tmp/x.unknownext")),
        "plaintext"
    );
}

#[test]
fn language_id_for_is_case_insensitive() {
    let cfg = rust_config();
    assert_eq!(language_id_for(&cfg, &PathBuf::from("/tmp/x.RS")), "rust");
}

// ---- end-to-end operation tests -----------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_sends_did_open_then_hover_with_zero_based_position() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"fn main() {}\n")
        .await
        .unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        // First frame: didOpen notification.
        let did_open = read_one_frame(&mut peer_io).await;
        assert_eq!(did_open["method"], "textDocument/didOpen");
        assert!(did_open.get("id").is_none(), "notify has no id");
        assert_eq!(did_open["params"]["textDocument"]["languageId"], "rust");
        assert_eq!(did_open["params"]["textDocument"]["version"], 1);
        assert!(did_open["params"]["textDocument"]["text"]
            .as_str()
            .unwrap()
            .contains("fn main"));

        // Second frame: hover request.
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/hover");
        // 1-based input (5, 3) → 0-based on wire (4, 2).
        assert_eq!(req["params"]["position"]["line"], 4);
        assert_eq!(req["params"]["position"]["character"], 2);

        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "result": {"contents": "hi"}
            }),
        )
        .await;
    });

    let result = hover(&client, &tracker, &cfg, &file_path, 5, 3)
        .await
        .expect("hover ok");
    assert_eq!(result.operation, LspOperation::Hover);
    assert!(result.raw["contents"].is_string());

    peer.await.expect("peer ok");
    // Tracker now knows the file was opened on rust-analyzer.
    assert!(
        tracker
            .is_open(
                "rust-analyzer",
                &lsp_types::Url::from_file_path(&file_path).unwrap()
            )
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_first_requests_queue_did_open_before_both_operations() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"fn main() {}\n")
        .await
        .unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let client = Arc::new(LspClient::new(
        "rust-analyzer".to_string(),
        Connection::new_lsp(client_read, client_write),
    ));
    let tracker = OpenFileTracker::new();
    let config = rust_config();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));

    let peer = tokio::spawn(async move {
        let first = read_one_frame(&mut peer_io).await;
        assert_eq!(
            first["method"], "textDocument/didOpen",
            "no concurrent operation may overtake the first didOpen"
        );
        for _ in 0..2 {
            let request = read_one_frame(&mut peer_io).await;
            assert_eq!(request["method"], "textDocument/hover");
            write_frame(
                &mut peer_io,
                &json!({
                    "jsonrpc": "2.0",
                    "id": request["id"],
                    "result": {"contents": "ok"}
                }),
            )
            .await;
        }
    });

    let mut requests = Vec::new();
    for _ in 0..2 {
        let client = Arc::clone(&client);
        let tracker = tracker.clone();
        let config = config.clone();
        let path = file_path.clone();
        let barrier = Arc::clone(&barrier);
        requests.push(tokio::spawn(async move {
            barrier.wait().await;
            hover(&client, &tracker, &config, &path, 1, 1).await
        }));
    }
    barrier.wait().await;
    for request in requests {
        request
            .await
            .expect("request task")
            .expect("hover succeeds");
    }
    peer.await.expect("peer task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn did_open_lru_sends_did_close_for_evicted_document() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for idx in 0..=MAX_OPEN_DOCUMENTS {
        let path = dir.path().join(format!("{idx}.rs"));
        tokio::fs::write(&path, format!("fn f{idx}() {{}}\n"))
            .await
            .unwrap();
        files.push(path);
    }

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        let mut first_uri = None;
        for idx in 0..=MAX_OPEN_DOCUMENTS {
            let did_open = read_one_frame(&mut peer_io).await;
            assert_eq!(did_open["method"], "textDocument/didOpen");
            if idx == 0 {
                first_uri = did_open["params"]["textDocument"]["uri"]
                    .as_str()
                    .map(str::to_string);
            }

            if idx == MAX_OPEN_DOCUMENTS {
                let did_close = read_one_frame(&mut peer_io).await;
                assert_eq!(did_close["method"], "textDocument/didClose");
                assert_eq!(
                    did_close["params"]["textDocument"]["uri"].as_str(),
                    first_uri.as_deref(),
                    "the oldest opened URI should be closed first"
                );
            }

            let req = read_one_frame(&mut peer_io).await;
            assert_eq!(req["method"], "textDocument/hover");
            write_frame(
                &mut peer_io,
                &json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "result": {"contents": "ok"}
                }),
            )
            .await;
        }
    });

    for file in &files {
        hover(&client, &tracker, &cfg, file, 1, 1)
            .await
            .expect("hover ok");
    }
    assert_eq!(tracker.len().await, MAX_OPEN_DOCUMENTS);
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn global_lru_closes_an_eviction_on_its_owning_server() {
    let dir = tempfile::tempdir().unwrap();
    let oldest = dir.path().join("oldest.rs");
    tokio::fs::write(&oldest, "fn oldest() {}\n").await.unwrap();
    let mut newer = Vec::new();
    for idx in 0..MAX_OPEN_DOCUMENTS {
        let path = dir.path().join(format!("newer-{idx}.rs"));
        tokio::fs::write(&path, format!("fn newer_{idx}() {{}}\n"))
            .await
            .unwrap();
        newer.push(path);
    }

    let tracker = OpenFileTracker::new();
    let (owner_io, mut owner_peer) = duplex(FRAME_BUFFER);
    let (owner_read, owner_write) = tokio::io::split(owner_io);
    let owner = Arc::new(LspClient::new(
        "owner-server".to_string(),
        Connection::new_lsp(owner_read, owner_write),
    ));
    let mut owner_config = rust_config();
    owner_config.name = "owner-server".to_string();

    let (current_io, mut current_peer) = duplex(FRAME_BUFFER);
    let (current_read, current_write) = tokio::io::split(current_io);
    let current = Arc::new(LspClient::new(
        "current-server".to_string(),
        Connection::new_lsp(current_read, current_write),
    ));
    let mut current_config = rust_config();
    current_config.name = "current-server".to_string();

    let expected_oldest = lsp_types::Url::from_file_path(&oldest).unwrap().to_string();
    let owner_task = tokio::spawn(async move {
        let did_open = read_one_frame(&mut owner_peer).await;
        assert_eq!(did_open["method"], "textDocument/didOpen");
        let request = read_one_frame(&mut owner_peer).await;
        write_frame(
            &mut owner_peer,
            &json!({"jsonrpc":"2.0", "id":request["id"], "result":{}}),
        )
        .await;

        let did_close = read_one_frame(&mut owner_peer).await;
        assert_eq!(did_close["method"], "textDocument/didClose");
        assert_eq!(did_close["params"]["textDocument"]["uri"], expected_oldest);
    });
    let current_task = tokio::spawn(async move {
        for _ in 0..MAX_OPEN_DOCUMENTS {
            let did_open = read_one_frame(&mut current_peer).await;
            assert_eq!(did_open["method"], "textDocument/didOpen");
            let request = read_one_frame(&mut current_peer).await;
            write_frame(
                &mut current_peer,
                &json!({"jsonrpc":"2.0", "id":request["id"], "result":{}}),
            )
            .await;
        }
    });

    hover(&owner, &tracker, &owner_config, &oldest, 1, 1)
        .await
        .expect("oldest file opens on owner server");
    for path in &newer {
        hover(&current, &tracker, &current_config, path, 1, 1)
            .await
            .expect("newer file opens on current server");
    }

    owner_task.await.expect("owner peer");
    current_task.await.expect("current peer");
    assert!(
        !tracker
            .is_open(
                "owner-server",
                &lsp_types::Url::from_file_path(&oldest).unwrap()
            )
            .await
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_skips_did_open_when_already_tracked() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"fn main() {}\n")
        .await
        .unwrap();
    let file_path = temp.path().to_path_buf();
    let uri = lsp_types::Url::from_file_path(&file_path).unwrap();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let _ = tracker.mark_open("rust-analyzer", uri.clone()).await;
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        // Should only see the hover request, no didOpen first.
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/hover");
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": {"contents": ""}}),
        )
        .await;
    });

    let _ = hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("hover ok");
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hover_rejects_a_stale_client_after_server_clear() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"fn main() {}\n")
        .await
        .unwrap();
    let file_path = temp.path().to_path_buf();
    let uri = lsp_types::Url::from_file_path(&file_path).unwrap();

    let (client_io, _peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let client = Arc::new(LspClient::new(
        "rust-analyzer".to_string(),
        Connection::new_lsp(client_read, client_write),
    ));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let (bootstrap_io, mut bootstrap_peer) = duplex(FRAME_BUFFER);
    let (bootstrap_read, bootstrap_write) = tokio::io::split(bootstrap_io);
    let bootstrap_client = Arc::new(LspClient::new(
        "rust-analyzer".to_string(),
        Connection::new_lsp(bootstrap_read, bootstrap_write),
    ));
    let bootstrap_peer_task = tokio::spawn(async move {
        let did_open = read_one_frame(&mut bootstrap_peer).await;
        assert_eq!(did_open["method"], "textDocument/didOpen");
        let request = read_one_frame(&mut bootstrap_peer).await;
        assert_eq!(request["method"], "textDocument/hover");
        write_frame(
            &mut bootstrap_peer,
            &json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {"contents": "ok"}
            }),
        )
        .await;
    });
    hover(&bootstrap_client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("first hover installs the active connection");
    bootstrap_peer_task.await.expect("bootstrap peer");
    tracker.clear_server("rust-analyzer").await;

    let error = hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect_err("stale client must not recreate tracker state after clear");
    assert!(matches!(
        error,
        LspOperationError::Lsp(platform_api::LspError::Unavailable)
    ));
    assert!(!tracker.is_open("rust-analyzer", &uri).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changed_open_file_sends_full_did_change_and_did_save_with_next_version() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"fn before() {}\n")
        .await
        .unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        let did_open = read_one_frame(&mut peer_io).await;
        assert_eq!(did_open["method"], "textDocument/didOpen");
        assert_eq!(did_open["params"]["textDocument"]["version"], 1);
        let first = read_one_frame(&mut peer_io).await;
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc":"2.0","id":first["id"],"result":{"contents":"one"}}),
        )
        .await;

        let did_change = read_one_frame(&mut peer_io).await;
        assert_eq!(did_change["method"], "textDocument/didChange");
        assert_eq!(did_change["params"]["textDocument"]["version"], 2);
        assert_eq!(
            did_change["params"]["contentChanges"],
            json!([{ "text": "fn after() {}\n" }])
        );
        let did_save = read_one_frame(&mut peer_io).await;
        assert_eq!(did_save["method"], "textDocument/didSave");
        let second = read_one_frame(&mut peer_io).await;
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc":"2.0","id":second["id"],"result":{"contents":"two"}}),
        )
        .await;
    });

    hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("initial hover");
    tokio::fs::write(&file_path, b"fn after() {}\n")
        .await
        .unwrap();
    hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("hover after edit");
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn definition_references_implementation_send_expected_method() {
    // Drive all three single-position operations through one peer.
    for (method, op_name) in [
        ("textDocument/definition", "definition"),
        ("textDocument/references", "references"),
        ("textDocument/implementation", "implementation"),
    ] {
        let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        tokio::fs::write(temp.path(), b"// a\n").await.unwrap();
        let file_path = temp.path().to_path_buf();

        let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
        let (client_read, client_write) = tokio::io::split(client_io);
        let connection = Connection::new_lsp(client_read, client_write);
        let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
        let tracker = OpenFileTracker::new();
        let cfg = rust_config();

        let expected_method = method.to_string();
        let peer = tokio::spawn(async move {
            // didOpen.
            let _ = read_one_frame(&mut peer_io).await;
            // request.
            let req = read_one_frame(&mut peer_io).await;
            assert_eq!(
                req["method"], expected_method,
                "method should be {expected_method}"
            );
            if expected_method == "textDocument/references" {
                assert_eq!(req["params"]["context"]["includeDeclaration"], true);
            }
            write_frame(
                &mut peer_io,
                &json!({"jsonrpc": "2.0", "id": req["id"], "result": []}),
            )
            .await;
        });

        match op_name {
            "definition" => {
                go_to_definition(&client, &tracker, &cfg, &file_path, 1, 1)
                    .await
                    .unwrap();
            }
            "references" => {
                find_references(&client, &tracker, &cfg, &file_path, 1, 1, true)
                    .await
                    .unwrap();
            }
            "implementation" => {
                go_to_implementation(&client, &tracker, &cfg, &file_path, 1, 1)
                    .await
                    .unwrap();
            }
            other => panic!("unhandled op: {other}"),
        }
        peer.await.expect("peer ok");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn document_symbol_omits_position() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"// a\n").await.unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        let _ = read_one_frame(&mut peer_io).await; // didOpen
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/documentSymbol");
        assert!(req["params"]["textDocument"]["uri"]
            .as_str()
            .unwrap()
            .starts_with("file:///"));
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": []}),
        )
        .await;
    });

    let res = document_symbol(&client, &tracker, &cfg, &file_path)
        .await
        .expect("documentSymbol ok");
    assert_eq!(res.operation, LspOperation::DocumentSymbol);
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_symbol_skips_did_open_gate() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));

    let peer = tokio::spawn(async move {
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "workspace/symbol");
        assert_eq!(req["params"]["query"], "fn ");
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": []}),
        )
        .await;
    });

    let res = workspace_symbol(&client, Some("fn ".to_string()))
        .await
        .expect("workspace symbol ok");
    assert_eq!(res.operation, LspOperation::WorkspaceSymbol);
    assert!(res.file_uri.is_empty());
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_symbol_default_empty_query() {
    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));

    let peer = tokio::spawn(async move {
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["params"]["query"], "");
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": []}),
        )
        .await;
    });

    workspace_symbol(&client, None).await.expect("ok");
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepare_call_hierarchy_sends_position() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"// a\n").await.unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        let _ = read_one_frame(&mut peer_io).await; // didOpen
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/prepareCallHierarchy");
        assert_eq!(req["params"]["position"]["line"], 9);
        assert_eq!(req["params"]["position"]["character"], 0);
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": req["id"], "result": []}),
        )
        .await;
    });

    let res = prepare_call_hierarchy(&client, &tracker, &cfg, &file_path, 10, 1)
        .await
        .expect("ok");
    assert_eq!(res.operation, LspOperation::PrepareCallHierarchy);
    peer.await.expect("peer ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_outgoing_short_circuit_when_no_prepare_items() {
    for which in ["incoming", "outgoing"] {
        let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        tokio::fs::write(temp.path(), b"// a\n").await.unwrap();
        let file_path = temp.path().to_path_buf();

        let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
        let (client_read, client_write) = tokio::io::split(client_io);
        let connection = Connection::new_lsp(client_read, client_write);
        let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
        let tracker = OpenFileTracker::new();
        let cfg = rust_config();

        let peer = tokio::spawn(async move {
            let _ = read_one_frame(&mut peer_io).await; // didOpen
            let prepare = read_one_frame(&mut peer_io).await;
            assert_eq!(prepare["method"], "textDocument/prepareCallHierarchy");
            // Return empty array — incoming/outgoing should not be sent.
            write_frame(
                &mut peer_io,
                &json!({"jsonrpc": "2.0", "id": prepare["id"], "result": []}),
            )
            .await;
        });

        let res = if which == "incoming" {
            incoming_calls(&client, &tracker, &cfg, &file_path, 1, 1)
                .await
                .unwrap()
        } else {
            outgoing_calls(&client, &tracker, &cfg, &file_path, 1, 1)
                .await
                .unwrap()
        };
        assert!(res.raw.is_array() && res.raw.as_array().unwrap().is_empty());
        peer.await.expect("peer ok");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_calls_two_step_round_trip() {
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    tokio::fs::write(temp.path(), b"// a\n").await.unwrap();
    let file_path = temp.path().to_path_buf();
    let uri_string = lsp_types::Url::from_file_path(&file_path)
        .unwrap()
        .to_string();

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let uri_for_peer = uri_string.clone();
    let peer = tokio::spawn(async move {
        let _ = read_one_frame(&mut peer_io).await; // didOpen
        let prepare = read_one_frame(&mut peer_io).await;
        assert_eq!(prepare["method"], "textDocument/prepareCallHierarchy");
        write_frame(
            &mut peer_io,
            &json!({
                "jsonrpc": "2.0",
                "id": prepare["id"],
                "result": [{
                    "name": "foo",
                    "kind": 12, // Function
                    "uri": uri_for_peer,
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    },
                    "selectionRange": {
                        "start": {"line": 0, "character": 0},
                        "end":   {"line": 0, "character": 3}
                    }
                }]
            }),
        )
        .await;

        let incoming = read_one_frame(&mut peer_io).await;
        assert_eq!(incoming["method"], "callHierarchy/incomingCalls");
        assert_eq!(incoming["params"]["item"]["name"], "foo");
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc": "2.0", "id": incoming["id"], "result": []}),
        )
        .await;
    });

    let res = incoming_calls(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("ok");
    assert_eq!(res.operation, LspOperation::IncomingCalls);
    peer.await.expect("peer ok");
}

#[tokio::test]
async fn file_too_large_is_rejected_before_did_open() {
    use std::os::unix::fs::FileExt;
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    // Sparse file: seek + write a single byte > limit.
    let f = std::fs::File::options()
        .write(true)
        .open(temp.path())
        .unwrap();
    f.write_at(b"x", MAX_LSP_FILE_SIZE_BYTES + 1).unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, _peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let err = hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect_err("must reject");
    match err {
        LspOperationError::FileTooLarge { size, limit } => {
            assert_eq!(limit, MAX_LSP_FILE_SIZE_BYTES);
            assert!(size > MAX_LSP_FILE_SIZE_BYTES);
        }
        other => panic!("expected FileTooLarge, got {other:?}"),
    }
    // Did NOT mark the tracker because we never sent didOpen.
    assert!(
        !tracker
            .is_open(
                "rust-analyzer",
                &lsp_types::Url::from_file_path(&file_path).unwrap()
            )
            .await
    );
}

#[tokio::test]
async fn zero_position_rejected_before_io() {
    // No need for a peer — validation happens before any network I/O.
    let (client_io, _peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    // Use a path that does NOT exist — if validation ran AFTER stat, this
    // would surface as Io, not InvalidPosition.
    let path = PathBuf::from("/tmp/this/should/never/exist.rs");
    let err = hover(&client, &tracker, &cfg, &path, 0, 1)
        .await
        .expect_err("must reject");
    assert!(matches!(err, LspOperationError::InvalidPosition { .. }));
}

// Pin the 10 MB cap to the exact value from claude-code
// (LSPTool.ts:53 — `MAX_LSP_FILE_SIZE_BYTES = 10_000_000`).
#[test]
fn max_lsp_file_size_bytes_constant_matches_claude_code() {
    assert_eq!(MAX_LSP_FILE_SIZE_BYTES, 10_000_000);
}

// Cross-operation check: the cap is enforced by *all* file-bound operations,
// not just `hover`. We exercise `go_to_definition` to ensure the guard lives
// in the shared `ensure_file_under_limit` helper, not per call site.
#[tokio::test]
async fn go_to_definition_also_rejects_oversize_file() {
    use std::os::unix::fs::FileExt;
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    let f = std::fs::File::options()
        .write(true)
        .open(temp.path())
        .unwrap();
    f.write_at(b"x", MAX_LSP_FILE_SIZE_BYTES + 1).unwrap();
    let file_path = temp.path().to_path_buf();

    let (client_io, _peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let err = go_to_definition(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect_err("size cap applies to definition too");
    assert!(matches!(err, LspOperationError::FileTooLarge { .. }));
}

// Boundary: a file exactly one byte UNDER the cap is accepted (didOpen sent).
// Pairs with `file_too_large_is_rejected_before_did_open` (+1 byte rejected)
// to pin the inclusive/exclusive semantics: `size > limit` rejects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_one_byte_under_cap_is_accepted() {
    use std::os::unix::fs::FileExt;
    let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
    let f = std::fs::File::options()
        .write(true)
        .open(temp.path())
        .unwrap();
    // Sparse file of size exactly MAX - 1 byte: write one byte at offset MAX-2
    // so the resulting file length is MAX_LSP_FILE_SIZE_BYTES - 1.
    f.write_at(b"x", MAX_LSP_FILE_SIZE_BYTES - 2).unwrap();
    let file_path = temp.path().to_path_buf();
    // Sanity-check the on-disk size before the assertion runs.
    let meta = std::fs::metadata(&file_path).unwrap();
    assert_eq!(meta.len(), MAX_LSP_FILE_SIZE_BYTES - 1);

    let (client_io, mut peer_io) = duplex(FRAME_BUFFER);
    let (client_read, client_write) = tokio::io::split(client_io);
    let connection = Connection::new_lsp(client_read, client_write);
    let client = Arc::new(LspClient::new("rust-analyzer".to_string(), connection));
    let tracker = OpenFileTracker::new();
    let cfg = rust_config();

    let peer = tokio::spawn(async move {
        // didOpen MUST be sent — proving the cap did not reject us.
        let did_open = read_one_frame(&mut peer_io).await;
        assert_eq!(did_open["method"], "textDocument/didOpen");
        let req = read_one_frame(&mut peer_io).await;
        assert_eq!(req["method"], "textDocument/hover");
        write_frame(
            &mut peer_io,
            &json!({"jsonrpc":"2.0","id":req["id"],"result":{"contents":""}}),
        )
        .await;
    });

    hover(&client, &tracker, &cfg, &file_path, 1, 1)
        .await
        .expect("file just under the cap is accepted");
    peer.await.expect("peer ok");
}
