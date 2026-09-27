//! Minimal NDJSON JSON-RPC echo server used as a stdio MCP test fixture.
//!
//! Reads one JSON-RPC request per line from stdin, writes one JSON-RPC
//! response per line to stdout. Supported methods:
//!
//! - `ping` -> `{ "pong": true }`
//! - `initialize` -> a server capabilities envelope advertising
//!   `tools`/`resources`/`prompts`/`logging` plus an `experimental` map
//! - `notifications/initialized` -> no reply (it is a JSON-RPC notification);
//!   the fixture reacts by emitting one server-initiated
//!   `notifications/message` line to stdout, so parent-side tests can observe
//!   a real server-pushed notification over the broadcast bridge
//! - `tools/list` -> one tool, `echo`
//! - `tools/call` with `name == "echo"` -> echoes its `arguments` back as a
//!   text content block; any other tool name -> a per-tool -32601 error so the
//!   parent's `ToolNotFound` mapping can be exercised
//! - `resources/list` -> one resource
//! - `resources/templates/list` -> one parameterized resource template
//! - `resources/read` -> the resource's text contents
//! - `prompts/list` -> one prompt, `greet`
//! - any other -> `{ "error": { "code": -32601, "message": "method not found" } }`
//!
//! Also emits a single banner line to stderr at startup so integration tests
//! can confirm the parent's `StderrRing` is wired up correctly.
//!
//! Exits when stdin is closed.

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Banner on stderr — lets parent-side StderrRing tests assert at least
    // one byte made it through the stderr pipe.
    let mut stderr = tokio::io::stderr();
    let _ = stderr.write_all(b"mock_stdio_mcp ready\n").await;
    let _ = stderr.flush().await;

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut reader = BufReader::new(stdin).lines();

    while let Ok(Some(line)) = reader.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let parse_err = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("parse error: {e}") }
                });
                let mut line_out = serde_json::to_string(&parse_err).unwrap();
                line_out.push('\n');
                if stdout.write_all(line_out.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
                continue;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        // `notifications/*` are JSON-RPC notifications (no `id`) — the client
        // never expects a reply. Returning `None` here suppresses any output.
        let response: Option<Value> = match method {
            "notifications/initialized" => {
                // React to the client's `initialized` notification by pushing
                // a server-initiated notification (no `id`). This lets the
                // parent's `notifications()` broadcast bridge observe a real
                // server-pushed line end-to-end.
                let notif = json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/message",
                    "params": { "level": "info", "data": "mock server initialized" }
                });
                let mut notif_line = serde_json::to_string(&notif).unwrap();
                notif_line.push('\n');
                if stdout.write_all(notif_line.as_bytes()).await.is_err() {
                    break;
                }
                let _ = stdout.flush().await;
                None
            }
            "ping" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "pong": true }
            })),
            "initialize" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2025-11-25",
                    "capabilities": {
                        "tools": {},
                        "resources": {},
                        "prompts": {},
                        "logging": {},
                        "experimental": { "foo": true }
                    },
                    "serverInfo": { "name": "mock_stdio_mcp", "version": "0.0.0" }
                }
            })),
            "tools/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echo the provided text back.",
                            "inputSchema": {
                                "type": "object",
                                "properties": { "text": { "type": "string" } },
                                "required": ["text"]
                            }
                        }
                    ]
                }
            })),
            "tools/call" => {
                let tool_name = params
                    .pointer("/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if tool_name == "echo" {
                    // Echo the `arguments.text` field back as a text content block.
                    let text = params
                        .pointer("/arguments/text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [ { "type": "text", "text": text } ],
                            "isError": false
                        }
                    }))
                } else {
                    // Unknown tool -> per-tool -32601 so the parent maps it to
                    // `McpError::ToolNotFound`.
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32601,
                            "message": format!("unknown tool: {tool_name}")
                        }
                    }))
                }
            }
            "resources/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resources": [
                        {
                            "uri": "mock://readme",
                            "name": "README",
                            "mimeType": "text/plain"
                        }
                    ]
                }
            })),
            "resources/templates/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "resourceTemplates": [
                        {
                            "uriTemplate": "mock://files/{path}",
                            "name": "file-template",
                            "description": "A file under mock://files",
                            "mimeType": "text/plain"
                        }
                    ]
                }
            })),
            "resources/read" => {
                let uri = params
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("mock://readme")
                    .to_string();
                Some(json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "contents": [
                            {
                                "uri": uri,
                                "mimeType": "text/plain",
                                "text": "hello from mock resource"
                            }
                        ]
                    }
                }))
            }
            "prompts/list" => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "prompts": [
                        {
                            "name": "greet",
                            "description": "Greet someone by name."
                        }
                    ]
                }
            })),
            _ => Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            })),
        };

        let Some(response) = response else {
            // Notification — no reply expected.
            continue;
        };

        let mut line_out = serde_json::to_string(&response).unwrap();
        line_out.push('\n');
        if stdout.write_all(line_out.as_bytes()).await.is_err() {
            break;
        }
        let _ = stdout.flush().await;
    }
}
