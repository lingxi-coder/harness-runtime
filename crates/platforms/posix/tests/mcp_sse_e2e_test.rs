//! End-to-end integration test: drive a mock MCP-over-SSE server through the
//! real `PosixMcpTransport` trait surface and assert that the full
//! `connect -> initialize -> list_tools -> call_tool -> disconnect` sequence
//! round-trips over genuine JSON-RPC.
//!
//! This is the SSE sibling of `mcp_stdio_e2e_test.rs` / `mcp_http_e2e_test.rs`.
//! It proves the fix that makes the `Sse` arm RETAIN its `connect_sse`
//! `Connection` (the old code dropped it, tearing down the HTTP+SSE tasks).
//!
//! Wire shape mirrors the real SSE contract: inbound replies travel on the GET
//! event-stream, decoupled from the outbound POSTs. The mock keeps a
//! `tokio::sync::broadcast` channel; the `post("/mcp")` handler parses the
//! JSON-RPC frame, computes the reply keyed on `method` + `id`, and publishes
//! it onto the channel. The `get("/mcp")` handler subscribes to the channel
//! (synchronously, at connection time — before any `call` registers its
//! pending oneshot) and forwards each reply as `data: {json}\n\n`. A tiny
//! startup delay on the stream lets the client register its pending oneshot
//! before the first reply can race past it.

use axum::{
    extract::State,
    response::sse::{Event, Sse},
    routing::{get, post},
    Json, Router,
};
use futures::{stream, Stream, StreamExt};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::time::Duration;
use tokio::sync::broadcast;

use lingxi_core::host::{McpTransport, McpTransportSpec};
use platform_posix::mcp::PosixMcpTransport;

#[derive(Clone)]
struct MockState {
    /// Replies the POST handler publishes; the GET event-stream forwards them.
    replies: broadcast::Sender<Value>,
}

/// GET event-stream: subscribe to the reply channel and forward each published
/// JSON-RPC reply as a single `data:` SSE event. A 50ms head start ensures the
/// client has registered its pending oneshot for the first request before any
/// reply can arrive (a reply received before registration is dropped, which
/// would make the router time out).
async fn sse_handler(
    State(state): State<MockState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.replies.subscribe();
    let warmup = stream::once(async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        None::<Value>
    });
    let replies = stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(v) => return Some((Some(v), rx)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    // A legacy HTTP+SSE server names its POST url first, on a named `endpoint`
    // event, and real ones use a different path from the stream's. Naming
    // `/messages` here means this round trip only passes if the client honours
    // it.
    let endpoint = stream::once(async {
        Ok::<_, Infallible>(Event::default().event("endpoint").data("/messages"))
    });
    // `warmup` yields one `None` (just the delay), then the real replies flow.
    let stream = warmup
        .chain(replies)
        .filter_map(|maybe| async move { maybe.map(|v| Ok(Event::default().data(v.to_string()))) });
    Sse::new(endpoint.chain(stream))
}

/// POST handler: parse the JSON-RPC frame, compute the reply keyed on method +
/// id, and publish it onto the broadcast channel (the reply travels back over
/// the GET event-stream, not in this POST response). Notifications carry no
/// `id` and produce no reply.
async fn post_handler(State(state): State<MockState>, Json(body): Json<Value>) -> &'static str {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = body.get("id").cloned() else {
        // Fire-and-forget notification (e.g. notifications/initialized): no id,
        // no reply. Acknowledge the POST with an empty body.
        return "";
    };

    let result = match method {
        "initialize" => json!({
            "protocolVersion": "2025-11-25",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "mock-sse", "version": "0.0.0" }
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
        // `ping` and any other method: empty result (valid JSON-RPC envelope).
        _ => json!({}),
    };

    let reply = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    // A send error means the GET stream has no subscriber yet; ignore it (the
    // router would time out, which the test's per-call timeout catches).
    let _ = state.replies.send(reply);
    ""
}

async fn spawn_mock() -> String {
    let (tx, _rx) = broadcast::channel::<Value>(64);
    let state = MockState { replies: tx };
    let app = Router::new()
        .route("/mcp", get(sse_handler))
        // No POST on the stream url: the client must use the named endpoint.
        .route("/messages", post(post_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

fn sse_spec(url: String) -> McpTransportSpec {
    McpTransportSpec::Sse {
        url,
        headers: lingxi_core::host::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    }
}

/// Connect, run the core MCP surface, and tear down — all over real JSON-RPC
/// against the mock SSE server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_mcp_surface_roundtrips_over_sse() {
    let transport = PosixMcpTransport::new();
    let url = spawn_mock().await;
    let spec = sse_spec(url);

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
        matches!(after, Err(lingxi_core::host::McpError::Connection(_))),
        "ping after disconnect should report a connection error, got {after:?}"
    );
}
