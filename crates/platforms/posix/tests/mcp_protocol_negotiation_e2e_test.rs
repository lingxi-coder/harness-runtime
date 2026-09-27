//! Real JSON-RPC/HTTP protocol-era negotiation tests.
//!
//! These tests intentionally drive `PosixMcpTransport` through an axum mock,
//! rather than testing only the pure envelope helpers. This catches probe
//! redial, compatibility fallback, corrective retry, and the live modern
//! request/result contract together.

use axum::{extract::State, routing::post, Json, Router};
use platform_api::{McpConnectOptions, McpProtocolEra, McpTransport, McpTransportSpec};
use platform_posix::mcp::PosixMcpTransport;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy)]
enum DiscoveryMode {
    Modern,
    LegacyFallback,
    CorrectiveRetry,
    InvalidResult,
    Auth,
    RemoteTimeout,
}

#[derive(Clone)]
struct MockState {
    mode: DiscoveryMode,
    requests: Arc<Mutex<Vec<Value>>>,
    discover_calls: Arc<std::sync::atomic::AtomicUsize>,
}

async fn handler(State(state): State<MockState>, Json(request): Json<Value>) -> Json<Value> {
    state.requests.lock().unwrap().push(request.clone());
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "server/discover" => {
            if matches!(state.mode, DiscoveryMode::RemoteTimeout) {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let attempt = state
                .discover_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match state.mode {
                DiscoveryMode::Modern => json!({
                    "protocolVersion": "2026-07-28"
                }),
                DiscoveryMode::InvalidResult => json!({
                    "protocolVersion": "2026-07-28"
                }),
                DiscoveryMode::LegacyFallback => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32601, "message": "method not found" }
                    }));
                }
                DiscoveryMode::CorrectiveRetry if attempt == 0 => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32022,
                            "message": "unsupported revision",
                            "data": { "supported": ["2026-07-28"] }
                        }
                    }));
                }
                DiscoveryMode::CorrectiveRetry => json!({
                    "protocolVersion": "2026-07-28"
                }),
                DiscoveryMode::Auth => {
                    return Json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": -32001,
                            "message": "HTTP 401",
                            "data": { "httpStatus": 401 }
                        }
                    }));
                }
                DiscoveryMode::RemoteTimeout => json!({}),
            }
        }
        "initialize" => json!({
            "protocolVersion": "2026-07-28",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "protocol-mock", "version": "1" }
        }),
        "tools/list" => {
            let mut result = json!({
                "tools": [{
                    "name": "echo",
                    "description": "echo",
                    "inputSchema": { "type": "object" }
                }]
            });
            if !matches!(state.mode, DiscoveryMode::LegacyFallback) {
                result["resultType"] = json!("complete");
            }
            if matches!(state.mode, DiscoveryMode::InvalidResult) {
                result["resultType"] = json!("partial");
            }
            result
        }
        _ => json!({}),
    };
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

async fn spawn_mock(mode: DiscoveryMode) -> (String, MockState) {
    let state = MockState {
        mode,
        requests: Arc::new(Mutex::new(Vec::new())),
        discover_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route("/mcp", post(handler))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/mcp"), state)
}

fn spec(url: String) -> McpTransportSpec {
    McpTransportSpec::Http {
        url,
        headers: platform_api::McpHeaders::new(),
        headers_helper: None,
        oauth: None,
    }
}

fn options() -> McpConnectOptions {
    options_with_deadline(5_000)
}

fn options_with_deadline(deadline_ms: u64) -> McpConnectOptions {
    McpConnectOptions {
        expected_era: Some(McpProtocolEra::Modern),
        deadline_ms,
        probe_timeout_ms: Some(3_000),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_success_injects_meta_and_strips_complete_result_type() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::Modern).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("modern handshake");
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    assert_eq!(result.negotiated.version, "2026-07-28");

    let tools = transport
        .list_tools(&result.connection)
        .await
        .expect("tools/list");
    assert_eq!(tools.len(), 1);
    let requests = state.requests.lock().unwrap().clone();
    let discover = requests
        .iter()
        .find(|r| r["method"] == "server/discover")
        .unwrap();
    assert_eq!(discover["params"]["_meta"].as_object().unwrap().len(), 3);
    let initialize = requests
        .iter()
        .find(|r| r["method"] == "initialize")
        .unwrap();
    assert!(initialize["params"].get("_meta").is_none());
    let list = requests
        .iter()
        .find(|r| r["method"] == "tools/list")
        .unwrap();
    assert_eq!(list["params"]["_meta"].as_object().unwrap().len(), 3);
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_not_found_closes_probe_and_redials_legacy() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::LegacyFallback).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("legacy fallback");
    assert_eq!(result.negotiated.era, McpProtocolEra::Legacy);
    transport
        .list_tools(&result.connection)
        .await
        .expect("legacy tools/list");
    let requests = state.requests.lock().unwrap().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "server/discover")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r["method"] == "initialize")
            .count(),
        1,
        "legacy fallback must initialize a fresh live connection"
    );
    let list = requests
        .iter()
        .find(|r| r["method"] == "tools/list")
        .unwrap();
    assert!(list["params"].get("_meta").is_none());
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_revision_gets_one_corrective_retry() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::CorrectiveRetry).await;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        transport.connect_and_initialize(&spec(url), options()),
    )
    .await
    .expect("corrective retry deadline")
    .expect("corrective retry handshake");
    assert_eq!(result.negotiated.era, McpProtocolEra::Modern);
    assert_eq!(
        state
            .discover_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_probe_timeout_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let (url, _state) = spawn_mock(DiscoveryMode::RemoteTimeout).await;
    let error = transport
        .connect_and_initialize(&spec(url), options_with_deadline(100))
        .await
        .expect_err("remote timeout must be reported");
    assert!(
        matches!(&error, platform_api::McpError::Internal(message) if message.contains("timed out")),
        "unexpected remote timeout classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_network_failure_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let error = transport
        .connect_and_initialize(
            &spec("http://127.0.0.1:9/mcp".to_string()),
            options_with_deadline(100),
        )
        .await
        .expect_err("remote network failure must be reported");
    assert!(
        matches!(&error, platform_api::McpError::Internal(message) if message.contains("timed out") || message.contains("writer")),
        "unexpected remote network classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_auth_error_is_not_treated_as_compatibility() {
    let transport = PosixMcpTransport::new();
    let (url, _state) = spawn_mock(DiscoveryMode::Auth).await;
    let error = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect_err("remote auth must be reported");
    assert!(
        matches!(
            error,
            platform_api::McpError::HttpResponse { status: 401, .. }
        ),
        "unexpected remote auth classification: {error:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn modern_catalog_rejects_non_complete_result_type() {
    let transport = PosixMcpTransport::new();
    let (url, state) = spawn_mock(DiscoveryMode::InvalidResult).await;
    let result = transport
        .connect_and_initialize(&spec(url), options())
        .await
        .expect("modern initialize");
    let error = transport
        .list_tools(&result.connection)
        .await
        .expect_err("partial modern catalog result must fail");
    assert!(
        matches!(&error, platform_api::McpError::Handshake(message) if message.contains("resultType")),
        "unexpected modern result classification: {error:?}"
    );
    assert!(state.requests.lock().unwrap().iter().any(|request| {
        request["method"] == "tools/list"
            && request["params"]["_meta"]
                .as_object()
                .map_or(false, |meta| meta.len() == 3)
    }));
    transport
        .disconnect(result.connection.connection_id)
        .await
        .unwrap();
}
