//! Integration tests for `McpEndpoint` and `IdeBridge`:
//! - rejects WebSocket upgrades that lack `X-LingXi-Ide-Authorization`,
//! - rejects upgrades whose token does NOT match the lockfile authToken,
//! - happy path: `IdeBridge::start` writes the lockfile, the bound port
//!   accepts the WS upgrade with the matching token, and `shutdown` removes
//!   the lockfile via the embedded `LockfileGuard`.

use bridge::wire::Frame;
use bridge::{FramePump, FrameSink, IdeBridge, IdeLockfile, McpEndpoint};
use client_protocol::events::{ClientEvent, ErrorKindDto};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn rejects_upgrade_without_auth_header() {
    let endpoint = McpEndpoint::start_on_ephemeral_port()
        .await
        .expect("endpoint must start");
    endpoint.set_auth_token("expected-token-1234567890abcdef0123".into());

    let url = format!("http://127.0.0.1:{}/mcp", endpoint.port());
    let res = reqwest::Client::new()
        .get(&url)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(res.status(), 401, "missing auth header must yield 401");
    let body = res.text().await.unwrap();
    assert_eq!(
        body, "unauthorized\n",
        "401 body must be literal 'unauthorized\\n'"
    );

    endpoint.shutdown().await;
}

#[tokio::test]
async fn rejects_upgrade_with_wrong_token() {
    let endpoint = McpEndpoint::start_on_ephemeral_port()
        .await
        .expect("endpoint must start");
    endpoint.set_auth_token("the-correct-token-32chars0000000".into());

    let url = format!("http://127.0.0.1:{}/mcp", endpoint.port());
    let res = reqwest::Client::new()
        .get(&url)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .header("x-lingxi-ide-authorization", "WRONG-TOKEN")
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(res.status(), 401, "wrong token must yield 401");

    endpoint.shutdown().await;
}

#[tokio::test]
async fn bridge_writes_lockfile_then_round_trips_ws_upgrade() {
    let bridge = IdeBridge::start(vec![std::env::current_dir().unwrap()])
        .await
        .expect("start bridge");
    let path = bridge.lockfile_path().clone();
    let expected_token = bridge.auth_token().to_string();
    let port = bridge.port();

    // 1. Lockfile exists on disk with the auth token we expect.
    assert!(path.exists(), "lockfile must exist after start");
    let (body, port_from_filename) = IdeLockfile::read(&path).expect("read lockfile");
    assert_eq!(
        port_from_filename, port,
        "filename port must match bind port"
    );
    assert_eq!(body.auth_token, expected_token);
    assert_eq!(body.transport, "ws");
    assert_eq!(body.ide_name, "LingXi");

    // 2. WebSocket upgrade with the right token succeeds. We build the
    // request via `http::Request` so we can attach the auth header
    // (`tokio_tungstenite::connect_async` accepts anything that implements
    // `IntoClientRequest`, and `http::Request<()>` is the canonical form).
    let url = format!("ws://127.0.0.1:{port}/mcp");
    let req = http::Request::builder()
        .method("GET")
        .uri(&url)
        .header("host", format!("127.0.0.1:{port}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("sec-websocket-protocol", "mcp")
        .header("x-lingxi-ide-authorization", expected_token.as_str())
        .body(())
        .unwrap();
    let (ws, response) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws upgrade must succeed");
    assert_eq!(response.status(), 101, "upgrade must return 101");
    // Echo of the subprotocol back from the server.
    let proto = response
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|v| v.to_str().ok());
    assert_eq!(proto, Some("mcp"));
    drop(ws);

    // 3. Bridge shutdown removes the lockfile.
    bridge.shutdown().await;
    // Give Drop a moment (LockfileGuard is sync but runs on the task that
    // drops `self`).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !path.exists(),
        "lockfile must be removed after IdeBridge::shutdown"
    );
}

/// A stub [`FramePump`] that, on every inbound [`Frame`], pushes a single
/// deterministic [`Frame::Event`] back out through the connection's sink — the
/// minimal proof that the generalized read/write pump (F2-03) is wired:
/// inbound frame deserialized → pump invoked → outbound frame delivered.
struct EchoPump;

#[async_trait::async_trait]
impl FramePump for EchoPump {
    async fn on_frame(&self, _frame: Frame, out: FrameSink) {
        let reply = Frame::Event(ClientEvent::TextDelta {
            text: "pong".to_string(),
        });
        let _ = out.send(reply);
    }
}

#[tokio::test]
async fn frame_pump_invoked_on_inbound_frame() {
    let endpoint = McpEndpoint::start_on_ephemeral_port_with_pump(std::sync::Arc::new(EchoPump))
        .await
        .expect("endpoint must start");
    endpoint.set_auth_token("pump-token-32chars000000000000000".into());

    let port = endpoint.port();
    let url = format!("ws://127.0.0.1:{port}/mcp");
    let req = http::Request::builder()
        .method("GET")
        .uri(&url)
        .header("host", format!("127.0.0.1:{port}"))
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", generate_key())
        .header("sec-websocket-protocol", "mcp")
        .header(
            "x-lingxi-ide-authorization",
            "pump-token-32chars000000000000000",
        )
        .body(())
        .unwrap();
    let (mut ws, response) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws upgrade must succeed");
    assert_eq!(response.status(), 101, "upgrade must return 101");

    // Send an inbound frame. The pump should fire and echo a frame back.
    let inbound = Frame::Event(ClientEvent::Error {
        kind: ErrorKindDto::Transport,
        message: "ping".to_string(),
    });
    let inbound_text = serde_json::to_string(&inbound).expect("serialize inbound frame");
    ws.send(Message::Text(inbound_text))
        .await
        .expect("send inbound frame");

    // Read the echoed frame back (bounded so the test cannot hang forever).
    let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next())
        .await
        .expect("pump must echo a frame within the timeout")
        .expect("stream must yield a message")
        .expect("message must not be an error");
    let text = match msg {
        Message::Text(t) => t,
        other => panic!("expected a text frame, got {other:?}"),
    };
    let echoed: Frame = serde_json::from_str(&text).expect("deserialize echoed frame");
    assert_eq!(
        echoed,
        Frame::Event(ClientEvent::TextDelta {
            text: "pong".to_string()
        }),
        "the stub pump must have echoed its deterministic reply frame"
    );

    endpoint.shutdown().await;
}
