//! MCP Streamable HTTP transport — POST request, JSON or text/event-stream response.
//! Matches claude-code's `StreamableHTTPClientTransport` from
//! `src/services/mcp/client.ts:784-901` (Accept: application/json, text/event-stream).
//!
//! Wire contract (LITERAL):
//! - POST `Content-Type: application/json` body is a single JSON-RPC frame.
//! - `Accept: application/json, text/event-stream` (the literal claude-code
//!   `MCP_STREAMABLE_HTTP_ACCEPT` const from `client.ts:471`).
//! - `User-Agent: claude-code/<CARGO_PKG_VERSION>` (matches `getMCPUserAgent()`
//!   shape from `utils/http.ts:37-50`).
//! - When `auth_token` is `Some`, POST carries
//!   `X-LingXi-Ide-Authorization: <token>` verbatim (no `Bearer ` prefix).
//! - Response body is EITHER a single `application/json` frame OR a
//!   `text/event-stream` body of zero-or-more frames; content-type selects.

use eventsource_stream::Eventsource;
use futures::StreamExt;
use jsonrpc::messages::Message as JsonRpcMessage;
use jsonrpc::{BrokerError, Connection, ConnectionError};
use lingxi_core::host::mcp::McpError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, USER_AGENT};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::mpsc;

/// Literal `Accept` header value for Streamable HTTP. Matches claude-code's
/// `MCP_STREAMABLE_HTTP_ACCEPT` const (`client.ts:471`) byte-for-byte.
const STREAMABLE_HTTP_ACCEPT: &str = "application/json, text/event-stream";

/// `User-Agent` value emitted by this client. Matches claude-code's
/// `getMCPUserAgent()` shape: `claude-code/<version>`.
#[must_use]
pub fn user_agent() -> String {
    format!("claude-code/{}", env!("CARGO_PKG_VERSION"))
}

/// Errors specific to opening an MCP Streamable HTTP connection.
#[derive(Debug, Error)]
pub enum HttpConnectError {
    /// HTTP request setup failed.
    #[error("http transport error: {0}")]
    Transport(String),
    /// Authorization header value was invalid (non-ASCII, control chars).
    #[error("invalid auth token: {0}")]
    InvalidAuth(String),
    /// Remote response retained for authentication recovery.
    #[error("HTTP {status}{detail}", detail = www_authenticate.as_ref().map(|value| format!(": {value}")).unwrap_or_default())]
    HttpResponse {
        /// HTTP status code.
        status: u16,
        /// `WWW-Authenticate` response header.
        www_authenticate: Option<String>,
    },
}

impl From<HttpConnectError> for McpError {
    fn from(value: HttpConnectError) -> Self {
        match value {
            HttpConnectError::HttpResponse {
                status,
                www_authenticate,
            } => Self::HttpResponse {
                status,
                www_authenticate,
            },
            other => Self::Connection(other.to_string()),
        }
    }
}

fn build_headers<H>(
    auth_token: Option<&str>,
    extra_headers: &H,
    session_id: Option<&str>,
) -> Result<HeaderMap, HttpConnectError>
where
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static(STREAMABLE_HTTP_ACCEPT));
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(
        USER_AGENT,
        HeaderValue::try_from(user_agent())
            .map_err(|e| HttpConnectError::Transport(e.to_string()))?,
    );
    if let Some(token) = auth_token {
        let v = HeaderValue::try_from(token)
            .map_err(|e| HttpConnectError::InvalidAuth(e.to_string()))?;
        h.insert(HeaderName::from_static("x-lingxi-ide-authorization"), v);
    }
    for (k, v) in extra_headers {
        let name = HeaderName::try_from(k.as_str())
            .map_err(|e| HttpConnectError::Transport(format!("bad header name {k}: {e}")))?;
        let val = HeaderValue::try_from(v.as_str())
            .map_err(|e| HttpConnectError::Transport(format!("bad header value: {e}")))?;
        h.insert(name, val);
    }
    if let Some(sid) = session_id {
        let v = HeaderValue::try_from(sid)
            .map_err(|e| HttpConnectError::Transport(format!("bad session id: {e}")))?;
        h.insert(HeaderName::from_static("mcp-session-id"), v);
    }
    Ok(h)
}

/// Open an MCP Streamable HTTP connection.
///
/// Each outbound JSON-RPC frame is `POSTed` to `url`. The server may reply with
/// either a single `application/json` body OR a `text/event-stream` body of
/// zero-or-more frames — both modes are decoded and routed back through
/// `jsonrpc::Connection`.
///
/// `extra_headers` is generic over the map type so both an unordered `HashMap`
/// and the insertion-ordered [`lingxi_core::host::McpHeaders`] (`IndexMap`) the MCP
/// transport specs now carry are accepted (header order is irrelevant to the
/// emitted HTTP request).
///
/// `fetch_timeout` bounds the time-to-response-*headers* of each outbound POST
/// (claude-code `jHs`/`YJr` — the fetch resolves once headers arrive and the
/// timer is cleared, so a streaming `text/event-stream` body is read afterwards
/// WITHOUT this bound). `None` disables the bound (byte-identical to the prior
/// behavior). Callers pass `mcp::client::mcp_http_fetch_timeout_for(..)` (default
/// `60_000`ms). A POST that exceeds it is dropped like any other POST failure.
///
/// # Errors
///
/// - [`HttpConnectError::Transport`] if reqwest client construction fails.
/// - [`HttpConnectError::InvalidAuth`] if `auth_token` contains bytes that
///   cannot be expressed in an HTTP header value (non-ASCII, control chars).
pub async fn connect_http<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    fetch_timeout: Option<std::time::Duration>,
) -> Result<Connection, HttpConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| HttpConnectError::Transport(e.to_string()))?;

    // Validate header construction eagerly so configuration errors surface at
    // connect-time rather than on the first POST.
    let _ = build_headers(auth_token, extra_headers, None)?;

    // Inbound: frames produced by the POST writer task (from JSON or SSE
    // response bodies). Outbound: frames the Connection wants to POST.
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

    let post_url = url.to_string();
    let post_auth = auth_token.map(str::to_string);
    let post_extra = extra_headers.clone();
    let post_fetch_timeout = fetch_timeout;

    // MCP Streamable HTTP session ID: captured from the `mcp-session-id`
    // response header on the initialize response and included as
    // `Mcp-Session-Id` in every subsequent request (1:1 with claude-code's
    // `StreamableHTTPClientTransport` session tracking).
    let session_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let writer_sid = Arc::clone(&session_id);

    tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            let headers = {
                let sid = writer_sid.lock().unwrap();
                match build_headers(post_auth.as_deref(), &post_extra, sid.as_deref()) {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!(error = %e, "mcp http: failed to build POST headers");
                        continue;
                    }
                }
            };

            // `jHs`/`YJr`: the fetch timeout bounds only the time-to-response
            // (`.send()` resolves on headers, like `await fetch(...)`); once we
            // hold the response the streaming SSE body is read without this bound.
            let send_fut = client.post(&post_url).headers(headers).json(&frame).send();
            let sent = match post_fetch_timeout {
                Some(t) => match tokio::time::timeout(t, send_fut).await {
                    Ok(r) => r,
                    Err(_) => {
                        tracing::warn!("mcp http: POST timed out awaiting response headers");
                        continue;
                    }
                },
                None => send_fut.await,
            };
            let response = match sent {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, "mcp http: POST failed");
                    continue;
                }
            };

            // Capture MCP session ID from response headers for subsequent
            // requests (MCP Streamable HTTP §session).
            if let Some(sid_val) = response.headers().get("mcp-session-id") {
                if let Ok(val) = sid_val.to_str() {
                    *writer_sid.lock().unwrap() = Some(val.to_string());
                }
            }

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let www_authenticate = response
                    .headers()
                    .get(reqwest::header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                // Streamable HTTP servers commonly explain a stale session in
                // a 400 response body (for example "Server not initialized").
                // Preserve only a small local diagnostic prefix so the MCP
                // client can apply the SDK's narrow stale-session classifier;
                // this value is never attached to telemetry.
                //
                // 404 and 405 are kept for a second reader: they are how a
                // server that speaks only legacy HTTP+SSE rejects a streamable
                // `initialize` POST, and the legacy fallback decides whether to
                // re-dial by asking whether that body is a JSON-RPC message. A
                // rejection carrying a real JSON-RPC error is a protocol
                // failure and must NOT be re-dialled, so the body is the
                // evidence, not the status alone.
                let body = if matches!(status, 400 | 404 | 405) {
                    bounded_error_body(response, 4_096).await
                } else {
                    String::new()
                };
                tracing::warn!(status, "mcp http: non-success response");
                if let Some(error) =
                    http_error_message(&frame, status, www_authenticate.as_deref(), &body)
                {
                    if inbound_tx.send(error).is_err() {
                        return;
                    }
                }
                continue;
            }

            let content_type = response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_lowercase();

            if content_type.starts_with("text/event-stream") {
                // Streaming body: parse with eventsource-stream.
                let mut events = response.bytes_stream().eventsource();
                while let Some(item) = events.next().await {
                    match item {
                        Ok(ev) => {
                            if ev.data.is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<JsonRpcMessage>(&ev.data) {
                                Ok(msg) => {
                                    if inbound_tx.send(msg).is_err() {
                                        return;
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        data = %ev.data,
                                        "mcp http: malformed SSE JSON frame, skipping"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "mcp http: sse parse error");
                            break;
                        }
                    }
                }
            } else {
                // Single JSON object response.
                match response.json::<JsonRpcMessage>().await {
                    Ok(msg) => {
                        if inbound_tx.send(msg).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "mcp http: response not valid JSON-RPC");
                    }
                }
            }
        }
    });

    // Adapt mpsc channels to the Stream/Sink shape `from_message_streams` wants.
    let inbound = futures::stream::unfold(inbound_rx, |mut rx| async move {
        rx.recv().await.map(|m| (m, rx))
    });
    let outbound = futures::sink::unfold(outbound_tx, |tx, msg: JsonRpcMessage| async move {
        tx.send(msg).map_err(|_| {
            ConnectionError::Broker(BrokerError::Join("http writer task closed".into()))
        })?;
        Ok::<_, ConnectionError>(tx)
    });

    Ok(Connection::from_message_streams(
        Box::pin(inbound),
        Box::pin(outbound),
    ))
}

fn http_error_message(
    request: &JsonRpcMessage,
    status: u16,
    www_authenticate: Option<&str>,
    body: &str,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    let marker = format!(
        "MCP_HTTP_STATUS={status};WWW_AUTHENTICATE={}",
        www_authenticate.unwrap_or_default()
    );
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32001,
            "message": marker,
            "data": {
                "httpStatus": status,
                "wwwAuthenticate": www_authenticate,
                "body": body
            }
        }
    }))
    .ok()
}

async fn bounded_error_body(response: reqwest::Response, max_bytes: usize) -> String {
    let mut bytes = Vec::with_capacity(max_bytes.min(1_024));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = max_bytes.saturating_sub(bytes.len());
        if remaining == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if bytes.len() == max_bytes {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_error_message_retains_local_stale_session_detail() {
        let request: JsonRpcMessage = serde_json::from_value(serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {}
        }))
        .expect("request");
        let error = http_error_message(
            &request,
            400,
            None,
            "Server not initialized for this session",
        )
        .expect("error response");
        let value = serde_json::to_value(error).expect("serialize response");
        assert_eq!(value["error"]["data"]["httpStatus"], 400);
        assert_eq!(
            value["error"]["data"]["body"],
            "Server not initialized for this session"
        );
    }
}
