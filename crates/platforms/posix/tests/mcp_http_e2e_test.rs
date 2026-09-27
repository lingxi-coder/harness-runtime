//! End-to-end integration test: drive a mock MCP-over-Streamable-HTTP server
//! through the real `PosixMcpTransport` trait surface and assert that the full
//! `connect -> initialize -> list_tools -> call_tool -> disconnect` sequence
//! round-trips over genuine JSON-RPC.
//!
//! This is the HTTP sibling of `mcp_stdio_e2e_test.rs`. It proves the fix that
//! makes the `Http` arm RETAIN its `connect_http` `Connection` (the old code
//! dropped it, so every request method failed). The mock is a single
//! `post("/mcp")` axum handler that switches on the JSON-RPC `method` and
//! returns the appropriate result envelope, echoing the request `id` back.
//! HTTP is 1:1 request/response, so a single handler suffices (no SSE channel).

use axum::{routing::post, Json, Router};
use serde_json::{json, Value};
use std::time::Duration;

use platform_api::{McpTransport, McpTransportSpec};
use platform_posix::mcp::PosixMcpTransport;

/// Method-aware mock: switch on `body["method"]`, echo the `id`, return the
/// matching MCP result envelope. `notifications/*` arrive without an `id`
/// (fire-and-forget) and are acknowledged with an empty `200 OK` body.
async fn http_handler(Json(body): Json<Value>) -> Json<Value> {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let id = body.get("id").cloned();

    // Notifications carry no `id`; the client's `notify` is fire-and-forget.
    // Reply with a benign empty JSON value (the transport ignores the body of
    // a notification POST — there is no pending call awaiting it).
    let Some(id) = id else {
        return Json(json!({}));
    };

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-http", "version": "0.0.0" }
        }),
        "tools/list" => json!({
            "tools": [{
                "name": "echo",
                "description": "Echo the provided text back.",
                "inputSchema": { "type": "object" }
            }]
        }),
        "tools/call" => {
            let text = body
                .pointer("/params/arguments/text")
                .and_then(Value::as_str)
                .unwrap_or("");
            json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            })
        }
        // `ping` and any other method: an empty result keeps the JSON-RPC
        // envelope valid (the transport's `ping` discards the body).
        _ => json!({}),
    };

    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn spawn_mock() -> String {
    let app = Router::new().route("/mcp", post(http_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

fn http_spec(url: String) -> McpTransportSpec {
    McpTransportSpec::Http {
        url,
        headers: platform_api::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    }
}

/// Connect, run the core MCP surface, and tear down — all over real JSON-RPC
/// against the mock Streamable HTTP server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_mcp_surface_roundtrips_over_http() {
    let transport = PosixMcpTransport::new();
    let url = spawn_mock().await;
    let spec = http_spec(url);

    // --- connect (must RETAIN the Connection) --------------------------
    let conn = tokio::time::timeout(Duration::from_secs(5), transport.connect(&spec))
        .await
        .expect("connect timed out")
        .expect("connect failed");

    // --- initialize ----------------------------------------------------
    let caps = tokio::time::timeout(Duration::from_secs(5), transport.initialize(&conn))
        .await
        .expect("initialize timed out")
        .expect("initialize failed");
    assert!(caps.tools, "mock advertises the `tools` capability");
    assert!(!caps.resources, "mock advertises no `resources` capability");
    assert!(!caps.prompts, "mock advertises no `prompts` capability");

    // --- tools/list ----------------------------------------------------
    let tools = tokio::time::timeout(Duration::from_secs(5), transport.list_tools(&conn))
        .await
        .expect("list_tools timed out")
        .expect("list_tools failed");
    assert_eq!(tools.len(), 1, "mock exposes exactly one tool");
    assert_eq!(tools[0].tool_name(), "echo");
    // No logical server name is threaded through `connect`, so `full_name` is
    // the unprefixed `mcp____<tool>` form (the lingxi-mcp layer rewrites it).
    assert_eq!(tools[0].full_name, "mcp____echo");

    // --- tools/call ----------------------------------------------------
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        transport.call_tool(&conn, "echo", json!({ "text": "hi there" })),
    )
    .await
    .expect("call_tool timed out")
    .expect("call_tool failed");
    assert!(!result.is_error, "echo should not flag an error");
    assert_eq!(
        result.content.pointer("/0/text").and_then(Value::as_str),
        Some("hi there"),
        "echo must round-trip the input text in content[0].text"
    );

    // --- ping ----------------------------------------------------------
    tokio::time::timeout(Duration::from_secs(5), transport.ping(conn.connection_id))
        .await
        .expect("ping timed out")
        .expect("ping failed");

    // --- disconnect ----------------------------------------------------
    tokio::time::timeout(
        Duration::from_secs(5),
        transport.disconnect(conn.connection_id),
    )
    .await
    .expect("disconnect timed out")
    .expect("disconnect failed");

    // After disconnect the connection id is removed from the map: a follow-up
    // ping must fail with a connection error rather than hang.
    let after = transport.ping(conn.connection_id).await;
    assert!(
        matches!(after, Err(platform_api::McpError::Connection(_))),
        "ping after disconnect should report a connection error, got {after:?}"
    );
}
