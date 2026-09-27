//! Integration test: connect to an in-process axum WebSocket server via
//! `connect_ws`, roundtrip a JSON-RPC `ping`, and assert the server saw the
//! `X-LingXi-Ide-Authorization` header *and* the `mcp` subprotocol
//! LITERALLY (no `Bearer ` prefix, exact subprotocol name).

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use platform_posix::connect_ws;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use url::Url;

#[derive(Default, Clone)]
struct CapturedHeaders {
    inner: Arc<Mutex<Option<HeaderMap>>>,
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    axum::extract::State(captured): axum::extract::State<CapturedHeaders>,
) -> impl IntoResponse {
    // Stash the handshake headers so the test can assert against them after
    // the request has completed.
    *captured.inner.lock().unwrap() = Some(headers);
    // Negotiate the `mcp` subprotocol on the response — required to match
    // claude-code's behavior where the client sends `Sec-WebSocket-Protocol: mcp`
    // and expects the server to confirm the same value.
    ws.protocols(["mcp"]).on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    while let Some(Ok(msg)) = socket.recv().await {
        if let Message::Text(text) = msg {
            // Parse the inbound JSON-RPC frame and reply to `ping` with a
            // sentinel result the test can match.
            let req: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let reply = match method {
                "ping" => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "pong": true }
                }),
                _ => serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "method not found" }
                }),
            };
            let _ = socket
                .send(Message::Text(serde_json::to_string(&reply).unwrap()))
                .await;
        }
    }
}

async fn spawn_server() -> (SocketAddr, CapturedHeaders) {
    let captured = CapturedHeaders::default();
    let app = Router::new()
        .route("/mcp", get(ws_handler))
        .with_state(captured.clone());

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service())
            .await
            .unwrap();
    });
    // Tiny wait so the OS-accept loop has a tick to come up before the
    // client opens its TCP connection.
    tokio::time::sleep(Duration::from_millis(20)).await;
    (addr, captured)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ws_roundtrips_and_sends_auth_header() {
    let (addr, captured) = spawn_server().await;
    let url = Url::parse(&format!("ws://{addr}/mcp")).unwrap();

    let conn = connect_ws(url, "secret-token-xyz")
        .await
        .expect("connect_ws failed");

    // `call` is generic over the response type; the mock server returns
    // `{ "pong": true }` so deserialize into a `serde_json::Value`.
    let result: serde_json::Value = tokio::time::timeout(
        Duration::from_secs(5),
        conn.call("ping", serde_json::json!({})),
    )
    .await
    .expect("ping request timed out")
    .expect("ping returned error");

    assert_eq!(result, serde_json::json!({"pong": true}));

    // Verify the server saw our auth header LITERALLY (no `Bearer ` prefix).
    let headers = captured
        .inner
        .lock()
        .unwrap()
        .clone()
        .expect("server captured no headers");
    let token = headers
        .get("X-LingXi-Ide-Authorization")
        .expect("X-LingXi-Ide-Authorization header not received by server");
    assert_eq!(token.to_str().unwrap(), "secret-token-xyz");
    assert!(
        !token.to_str().unwrap().starts_with("Bearer"),
        "auth header must be raw token, not `Bearer <token>`"
    );

    // Verify subprotocol negotiation worked (server saw `mcp` in the
    // `Sec-WebSocket-Protocol` request header).
    let protos = headers
        .get("Sec-WebSocket-Protocol")
        .expect("Sec-WebSocket-Protocol header not received by server");
    assert_eq!(protos.to_str().unwrap(), "mcp");

    // `Connection::close()` returns `()` — there is no Result. Calling it
    // aborts the broker tasks so the test exits cleanly without waiting on
    // the spawned axum server.
    conn.close();
}
