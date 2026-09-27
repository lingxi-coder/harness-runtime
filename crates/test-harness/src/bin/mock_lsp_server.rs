//! Minimal canned-response LSP server used by integration tests.
//!
//! Speaks `Content-Length`-framed JSON-RPC on stdin/stdout. Recognizes:
//! - `initialize` -> returns minimal `InitializeResult` with hoverProvider.
//! - `initialized` (notification) -> no-op.
//! - `textDocument/didOpen` (notification) -> no-op (logged to stderr).
//! - `textDocument/hover` -> returns `{contents: "mock hover"}`.
//! - `shutdown` -> returns `null`.
//! - `exit` (notification) -> exits with status 0.
//!
//! Unknown methods return JSON-RPC error -32601 (method not found).

use serde_json::{json, Value};
use std::io::{BufReader, Read, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut reader = BufReader::new(stdin.lock());
    let mut out = stdout.lock();
    let mut errw = stderr.lock();

    loop {
        // Read headers up to \r\n\r\n.
        let mut header = Vec::<u8>::new();
        let mut last_four = [0u8; 4];
        loop {
            let mut buf = [0u8; 1];
            if reader.read(&mut buf).unwrap_or(0) == 0 {
                writeln!(errw, "mock_lsp_server: EOF on stdin; exiting").ok();
                return;
            }
            header.push(buf[0]);
            last_four.rotate_left(1);
            last_four[3] = buf[0];
            if &last_four == b"\r\n\r\n" {
                break;
            }
        }
        let header_str = std::str::from_utf8(&header).unwrap_or("");
        let content_length: usize = header_str
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        if content_length == 0 {
            writeln!(errw, "mock_lsp_server: missing Content-Length; exiting").ok();
            return;
        }
        let mut body = vec![0u8; content_length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let msg: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                writeln!(errw, "mock_lsp_server: bad json: {e}").ok();
                continue;
            }
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();

        match method {
            "initialize" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "capabilities": {
                            "hoverProvider": true,
                            "definitionProvider": true,
                        }
                    }
                });
                write_frame(&mut out, &resp);
            }
            "textDocument/hover" => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {"contents": "mock hover"}
                });
                write_frame(&mut out, &resp);
            }
            "shutdown" => {
                let resp = json!({"jsonrpc": "2.0", "id": id, "result": null});
                write_frame(&mut out, &resp);
            }
            "exit" => {
                std::process::exit(0);
            }
            "initialized"
            | "textDocument/didOpen"
            | "textDocument/didChange"
            | "textDocument/didClose" => {
                // Notifications: no response.
            }
            _ if id.is_some() => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": format!("method not found: {method}")}
                });
                write_frame(&mut out, &resp);
            }
            _ => {}
        }
    }
}

fn write_frame(out: &mut impl Write, val: &Value) {
    let body = serde_json::to_vec(val).unwrap();
    write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
    out.write_all(&body).unwrap();
    out.flush().unwrap();
}
