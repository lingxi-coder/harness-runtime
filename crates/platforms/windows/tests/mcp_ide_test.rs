//! Windows IDE MCP transport contracts.
//!
//! These tests exercise the Windows platform adapter against local endpoints,
//! rather than testing the shared connector in isolation. That keeps the
//! platform dispatch, endpoint URL path, local auth header, and disconnect
//! ownership contract covered together.

use axum::extract::{ws::WebSocketUpgrade, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use futures::{stream, Stream, StreamExt};
use lingxi_core::host::{McpError, McpTransport, McpTransportKind, McpTransportSpec};
use platform_windows::WindowsMcpTransport;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;

#[derive(Clone, Default)]
struct EndpointState {
    sse_headers: Arc<Mutex<Option<HeaderMap>>>,
    ws_headers: Arc<Mutex<Option<HeaderMap>>>,
    ws_closed: Arc<Notify>,
}

async fn sse_handler(
    State(state): State<EndpointState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    *state.sse_headers.lock().expect("sse header lock") = Some(headers);
    let initial =
        stream::once(async { Ok::<Event, Infallible>(Event::default().comment("ready")) });
    // Keep the GET alive until the transport explicitly disconnects it. This
    // makes the post-disconnect notification assertion meaningful.
    Sse::new(initial.chain(stream::pending::<Result<Event, Infallible>>()))
}

async fn ws_handler(
    State(state): State<EndpointState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    *state.ws_headers.lock().expect("websocket header lock") = Some(headers);
    let closed = state.ws_closed.clone();
    ws.protocols(["mcp"])
        .on_upgrade(move |mut socket| async move {
            while let Some(message) = socket.recv().await {
                if message.is_err() {
                    break;
                }
            }
            closed.notify_one();
        })
}

async fn spawn_endpoint() -> (SocketAddr, EndpointState) {
    let state = EndpointState::default();
    let app = Router::new()
        // The nested path is intentional: a Windows-hosted IDE endpoint must
        // be dialled exactly as discovered, without dropping or rewriting its
        // URL path when `ide_running_in_windows` is true.
        .route("/ide/windows/sse", get(sse_handler))
        .route("/ide/windows/ws", get(ws_handler))
        .with_state(state.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    (address, state)
}

fn header_value(headers: &Option<HeaderMap>, name: &str) -> String {
    headers
        .as_ref()
        .and_then(|headers| headers.get(name))
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_sse_ide_uses_raw_local_token_and_preserves_windows_path() {
    let (address, state) = spawn_endpoint().await;
    let token = "windows-sse-local-secret";
    let spec = McpTransportSpec::SseIde {
        url: format!("http://{address}/ide/windows/sse"),
        ide_name: "VS Code".into(),
        auth_token: Some(token.into()),
        ide_running_in_windows: true,
    };
    let transport = WindowsMcpTransport::new();

    let connection = tokio::time::timeout(Duration::from_secs(5), transport.connect(&spec))
        .await
        .expect("SSE connect timed out")
        .expect("SSE connect failed");

    let headers = state.sse_headers.lock().expect("sse header lock").clone();
    assert_eq!(
        header_value(&headers, "X-LingXi-Ide-Authorization"),
        token,
        "SSE must send the local token verbatim, without Bearer"
    );
    assert_eq!(
        header_value(&headers, "Accept"),
        "text/event-stream",
        "SSE IDE endpoint must use the event-stream GET contract"
    );

    transport
        .disconnect(connection.connection_id)
        .await
        .expect("SSE disconnect failed");
    // A removed remote connection produces an already-closed notification
    // stream; a retained connection would wait forever on its SSE reader.
    let mut notifications = transport
        .notifications(&connection)
        .await
        .expect("notification subscription");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), notifications.next())
            .await
            .expect("notification stream did not close")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_ws_ide_uses_auth_and_mcp_subprotocol_then_closes() {
    let (address, state) = spawn_endpoint().await;
    let token = "windows-ws-local-secret";
    let spec = McpTransportSpec::WsIde {
        url: format!("ws://{address}/ide/windows/ws?session=path-preserved"),
        ide_name: "Cursor".into(),
        auth_token: Some(token.into()),
        ide_running_in_windows: true,
    };
    let transport = WindowsMcpTransport::new();

    let connection = tokio::time::timeout(Duration::from_secs(5), transport.connect(&spec))
        .await
        .expect("WebSocket connect timed out")
        .expect("WebSocket connect failed");
    let headers = state
        .ws_headers
        .lock()
        .expect("websocket header lock")
        .clone();
    assert_eq!(
        header_value(&headers, "X-LingXi-Ide-Authorization"),
        token,
        "WebSocket must send the local token verbatim, without Bearer"
    );
    assert_eq!(
        header_value(&headers, "Sec-WebSocket-Protocol"),
        "mcp",
        "WebSocket IDE endpoint must negotiate the MCP subprotocol"
    );

    let closed = state.ws_closed.notified();
    transport
        .disconnect(connection.connection_id)
        .await
        .expect("WebSocket disconnect failed");
    tokio::time::timeout(Duration::from_secs(5), closed)
        .await
        .expect("WebSocket server did not observe disconnect");

    let mut notifications = transport
        .notifications(&connection)
        .await
        .expect("notification subscription");
    assert!(
        tokio::time::timeout(Duration::from_secs(5), notifications.next())
            .await
            .expect("notification stream did not close")
            .is_none()
    );
}

#[tokio::test]
async fn windows_ide_connect_rejects_invalid_url_and_redacts_tokens() {
    let transport = WindowsMcpTransport::new();
    let token = "windows-invalid-url-secret";
    let invalid_url = McpTransportSpec::WsIde {
        url: "not a URL".into(),
        ide_name: "VS Code".into(),
        auth_token: Some(token.into()),
        ide_running_in_windows: true,
    };
    let error = transport
        .connect(&invalid_url)
        .await
        .expect_err("invalid WebSocket URL must fail");
    let rendered = format!("{error:?}");
    assert!(
        !rendered.contains(token),
        "URL errors must not expose auth tokens"
    );
    assert!(matches!(error, McpError::Connection(_)));

    // HeaderValue rejects non-ASCII/control bytes before any request is sent.
    // The shared connector's error must retain only a safe parser message.
    let token = "windows-invalid-token-\u{00e9}";
    let invalid_token = McpTransportSpec::SseIde {
        url: "http://127.0.0.1:1/ide/windows/sse".into(),
        ide_name: "VS Code".into(),
        auth_token: Some(token.into()),
        ide_running_in_windows: true,
    };
    let error = transport
        .connect(&invalid_token)
        .await
        .expect_err("invalid auth token must fail");
    let rendered = format!("{error:?}");
    assert!(
        !rendered.contains(token),
        "auth errors must redact the token"
    );
    assert!(matches!(error, McpError::Connection(_)));
}

#[test]
fn windows_transport_advertises_both_ide_variants() {
    let kinds = WindowsMcpTransport::new().supported_transports();
    assert!(kinds.contains(&McpTransportKind::SseIde));
    assert!(kinds.contains(&McpTransportKind::WsIde));
}
