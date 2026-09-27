//! End-to-end integration test: a Streamable HTTP MCP server that rejects the
//! very first `initialize` POST with `403 Forbidden` + a `WWW-Authenticate`
//! challenge must surface as a *structural* `McpError::HttpResponse` out of
//! `PosixMcpTransport::initialize`, not a stringified `Handshake` error.
//!
//! This proves the full production pipe end to end: `connect_http`'s writer
//! task turns the non-2xx response into a synthetic JSON-RPC error carrying
//! `data: {httpStatus, wwwAuthenticate}` (`mcp_http.rs::http_error_message`),
//! the `jsonrpc` router surfaces it as `RouterError::Remote`, and
//! `handshake_error` (`platform_posix::mcp`) unwraps that `data` shape back
//! into `McpError::HttpResponse` — the same variant SSE's pre-flight GET
//! already returns directly. `mcp::registry`'s 401/403 auth classification
//! (§19) and OAuth `resource_metadata` extraction (§24c) both key off this
//! carrier at the exact call site `McpRegistry::connect_attempt` uses
//! (`transport.connect` + `transport.initialize`).

use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{routing::post, Json};
use serde_json::{json, Value};
use std::time::Duration;

use platform_api::{McpError, McpTransport, McpTransportSpec};
use platform_posix::mcp::PosixMcpTransport;

/// Literal `WWW-Authenticate` challenge carrying an RFC 6750 `insufficient_scope`
/// error, an elevated `scope`, and an RFC 9728 `resource_metadata` pointer —
/// the exact three params §19 and §24c need to extract.
const WWW_AUTHENTICATE: &str = "Bearer error=\"insufficient_scope\", scope=\"mcp:elevated\", \
resource_metadata=\"https://mock.example/.well-known/oauth-protected-resource\"";

/// Reject `initialize` with `403` + the challenge above; anything else is
/// unreachable in this test (the client never gets past `initialize`).
async fn rejecting_http_handler(Json(body): Json<Value>) -> Response {
    let method = body.get("method").and_then(Value::as_str).unwrap_or("");
    assert_eq!(
        method, "initialize",
        "test only exercises the very first POST"
    );
    let mut response = (StatusCode::FORBIDDEN, Json(json!({}))).into_response();
    response.headers_mut().insert(
        axum::http::header::WWW_AUTHENTICATE,
        HeaderValue::from_static(WWW_AUTHENTICATE),
    );
    response
}

async fn spawn_mock() -> String {
    let app = axum::Router::new().route("/mcp", post(rejecting_http_handler));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_surfaces_structured_403_with_www_authenticate() {
    let transport = PosixMcpTransport::new();
    let url = spawn_mock().await;
    let spec = http_spec(url);

    // `connect_http` only opens channels — it never touches the network
    // itself, so `connect()` succeeds even though the mock will reject the
    // very next request.
    let conn = tokio::time::timeout(Duration::from_secs(5), transport.connect(&spec))
        .await
        .expect("connect timed out")
        .expect("connect failed");

    let err = tokio::time::timeout(Duration::from_secs(5), transport.initialize(&conn))
        .await
        .expect("initialize timed out")
        .expect_err("a 403 response must fail initialize");

    match err {
        McpError::HttpResponse {
            status,
            www_authenticate,
        } => {
            assert_eq!(status, 403, "exact HTTP status must survive the round trip");
            let waa = www_authenticate
                .expect("WWW-Authenticate header must be carried through structurally");
            assert_eq!(
                waa, WWW_AUTHENTICATE,
                "the full challenge value must round-trip byte-for-byte, not just a substring"
            );
        }
        other => panic!(
            "expected structural McpError::HttpResponse, got {other:?} \
             (the 403/WWW-Authenticate carrier regressed to a stringified error)"
        ),
    }
}
