//! Verifies `connect_http` wire format against a real HTTP server:
//! - POST request bears `Accept: application/json, text/event-stream` and
//!   `Content-Type: application/json`.
//! - Outbound JSON-RPC frame is serialized as the POST body.
//! - Response body is parsed as JSON (single object) and routed back as inbound.

use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
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
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let id = body.get("id").cloned().unwrap_or(json!(0));
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": {"echo": "ok"} }))
}

async fn spawn_slow_mock() -> String {
    let app = Router::new().route("/mcp", post(slow_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/mcp")
}

/// `jHs`/`YJr`: a POST whose response-headers do not arrive within the fetch
/// timeout is aborted and dropped, so no inbound frame is ever routed back and
/// the awaiting JSON-RPC call never resolves. A generous outer bound (well above
/// the 300ms handler delay) confirms the call was dropped, not merely slow.
#[tokio::test]
async fn connect_http_fetch_timeout_drops_slow_post() {
    let url = spawn_slow_mock().await;
    let conn = platform_common::connect_http(
        &url,
        None,
        &HashMap::new(),
        Some(std::time::Duration::from_millis(50)),
    )
    .await
    .expect("connect_http should succeed");

    let outcome = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        conn.call::<_, Value>("ping", json!({"a": 1})),
    )
    .await;
    assert!(
        outcome.is_err(),
        "the slow POST must be dropped by the 50ms fetch timeout — the call must \
never resolve (got {outcome:?})"
    );
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
