//! Verifies `connect_http` wire format against a real HTTP server:
//! - POST request bears `Accept: application/json, text/event-stream` and
//!   `Content-Type: application/json`.
//! - Outbound JSON-RPC frame is serialized as the POST body.
//! - Response body is parsed as JSON (single object) and routed back as inbound.

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::HeaderMap,
    response::Response,
    routing::post,
    Json, Router,
};
use jsonrpc::{ConnectionError, RouterError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default, Clone)]
struct MockState {
    captured_headers: Arc<Mutex<HeaderMap>>,
    captured_body: Arc<Mutex<Option<Value>>>,
}

async fn http_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    *state.captured_headers.lock().await = headers;
    let id = body.get("id").cloned().unwrap_or(json!(0));
    *state.captured_body.lock().await = Some(body);
    Json(json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"echo": "ok"}
    }))
}

async fn spawn_mock() -> (String, MockState) {
    let state = MockState::default();
    let app = Router::new()
        .route("/mcp", post(http_handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), state)
}

/// Handler that stalls before responding, so a short fetch timeout aborts the
/// POST before any response headers arrive.
async fn slow_handler(Json(body): Json<Value>) -> Json<Value> {
    if body["method"] == "ping" {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    let id = body.get("id").cloned().unwrap_or(json!(0));
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": {"echo": "ok"} }))
}

async fn spawn_slow_mock() -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new().route("/mcp", post(slow_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), server)
}

/// Native 2.1.322's fetch wrapper aborts before headers and its HTTP send rejects
/// the same failure. Rust represents that rejection with a local JSON-RPC error,
/// as it already did before the 2.1.322 transport alignment. The connection must
/// remain usable rather than leaving the call pending until the RPC deadline.
#[tokio::test]
async fn connect_http_fetch_timeout_fails_call_and_keeps_connection_usable() {
    let (url, server) = spawn_slow_mock().await;
    let conn = platform_common::connect_http(
        &url,
        None,
        &HashMap::new(),
        Some(std::time::Duration::from_millis(50)),
    )
    .await
    .expect("connect_http should succeed");

    let error = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        conn.call::<_, Value>("ping", json!({"a": 1})),
    )
    .await
    .expect("the header timeout must fail the call before the RPC deadline")
    .expect_err("the delayed headers must exceed the 50ms fetch timeout");
    let ConnectionError::Router(RouterError::Remote(error)) = error else {
        panic!("expected the local transport error, got {error:?}");
    };
    assert_eq!(error.code, -32001);
    assert_eq!(error.message, "POST timed out awaiting response headers");
    assert!(error.data.is_none());

    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        conn.call::<_, Value>("after-timeout", json!({})),
    )
    .await
    .expect("a failed POST must not close the connection")
    .expect("the next POST must succeed");
    assert_eq!(response, json!({"echo":"ok"}));
    conn.close();
    server.abort();
}

async fn slow_body_handler(Json(body): Json<Value>) -> Response {
    let frame = json!({"jsonrpc":"2.0","id":body["id"],"result":{"echo":"ok"}});
    let stream = futures::stream::once(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        Ok::<_, std::convert::Infallible>(Bytes::from(frame.to_string()))
    });
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// The native fetch timer is cleared once headers arrive; response bodies may
/// take longer than the fetch budget without aborting the JSON-RPC request.
#[tokio::test]
async fn connect_http_fetch_timeout_does_not_bound_response_body() {
    let app = Router::new().route("/mcp", post(slow_body_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let conn = platform_common::connect_http(
        &url,
        None,
        &HashMap::new(),
        Some(std::time::Duration::from_millis(50)),
    )
    .await
    .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        conn.call::<_, Value>("ping", json!({})),
    )
    .await
    .expect("the slow body must still complete")
    .expect("the fetch timer must stop after the headers");
    assert_eq!(response, json!({"echo":"ok"}));
    conn.close();
    server.abort();
}

#[tokio::test]
async fn connect_http_sends_accept_and_content_type() {
    let (url, state) = spawn_mock().await;
    let conn = platform_common::connect_http(&url, None, &HashMap::new(), None)
        .await
        .expect("connect_http should succeed");

    let resp: Value = conn
        .call("ping", json!({"a": 1}))
        .await
        .expect("request should round-trip");
    assert_eq!(resp, json!({"echo": "ok"}));

    let headers = state.captured_headers.lock().await.clone();
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        accept.contains("application/json") && accept.contains("text/event-stream"),
        "Accept must list both application/json and text/event-stream, got {accept}"
    );
    let ct = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.starts_with("application/json"),
        "Content-Type must be application/json, got {ct}"
    );

    let body = state.captured_body.lock().await.clone().unwrap();
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["method"], "ping");
}

#[tokio::test]
async fn connect_http_includes_ide_auth_header_when_provided() {
    let (url, state) = spawn_mock().await;
    let conn = platform_common::connect_http(
        &url,
        Some("deadbeefdeadbeefdeadbeefdeadbeef"),
        &HashMap::new(),
        None,
    )
    .await
    .expect("connect_http should succeed");
    let _: Result<Value, _> = conn.call("noop", json!({})).await;

    let headers = state.captured_headers.lock().await.clone();
    let auth = headers
        .get("x-lingxi-ide-authorization")
        .and_then(|v| v.to_str().ok());
    assert_eq!(auth, Some("deadbeefdeadbeefdeadbeefdeadbeef"));
}
