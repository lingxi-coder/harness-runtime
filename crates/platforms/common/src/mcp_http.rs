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
use lingxi_core::host::mcp_result::CancellationToken;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, USER_AGENT};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tokio::sync::mpsc;

/// Only router-assigned ids can register a POST token. The writer takes each
/// token exactly once; cancelled queued tokens prevent delayed network writes.
#[derive(Default)]
struct HttpRequestCancellation {
    pending: Mutex<HashMap<jsonrpc::Id, CancellationToken>>,
}
impl jsonrpc::router::PerRequestCancellation for HttpRequestCancellation {
    fn prepare(&self, id: &jsonrpc::Id) -> CancellationToken {
        let token = CancellationToken::new();
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), token.clone());
        token
    }
    fn discard(&self, id: &jsonrpc::Id) {
        if let Some(token) = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
        {
            token.cancel();
        }
    }
    fn close(&self) {
        for (_, token) in self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
        {
            token.cancel();
        }
    }
}
impl HttpRequestCancellation {
    fn take(&self, frame: &JsonRpcMessage) -> CancellationToken {
        match frame {
            JsonRpcMessage::Request(request) => self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&request.id)
                .unwrap_or_else(|| {
                    let token = CancellationToken::new();
                    token.cancel();
                    token
                }),
            _ => CancellationToken::new(),
        }
    }
}

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

    let request_cancellation = Arc::new(HttpRequestCancellation::default());
    let writer_request_cancellation = request_cancellation.clone();
    tokio::spawn(async move {
        // A POST owns its response body independently: input-required retries
        // and ordinary calls can proceed while an earlier SSE stream stays open.
        // The connection owns and drains every reader on close.
        let protocol_version = Arc::new(Mutex::new(None::<String>));
        let mut posts = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _=inbound_tx.closed()=>break,
                settled=posts.join_next(), if !posts.is_empty()=>{
                    if let Some(Err(error))=settled {tracing::warn!(%error,"mcp http: POST task failed");}
                }
                frame=outbound_rx.recv()=>{
                    let Some(frame)=frame else {break;};
                    let request_abort=writer_request_cancellation.take(&frame);
                    if request_abort.is_cancelled() {continue;}
                    let client=client.clone();let post_url=post_url.clone();let post_auth=post_auth.clone();let post_extra=post_extra.clone();let writer_sid=writer_sid.clone();let writer_protocol_version=protocol_version.clone();let inbound_tx=inbound_tx.clone();
                    posts.spawn(async move {
                        let process_frame=async move {
                    let frame_value = serde_json::to_value(&frame).unwrap_or_default();
                    let is_probe = frame_value["method"] == "server/discover";
                    let mut headers = {
                        let sid = writer_sid.lock().unwrap();
                        match build_headers(post_auth.as_deref(), &post_extra, sid.as_deref()) {
                            Ok(h) => h,
                            Err(e) => {
                                tracing::error!(error = %e, "mcp http: failed to build POST headers");
                                return;
                            }
                        }
                    };

                    if let Some(version) = writer_protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
                        if let Ok(value) = HeaderValue::from_str(version) {
                            headers.insert(HeaderName::from_static("mcp-protocol-version"), value);
                        }
                    }

                    apply_body_derived_headers(&mut headers, &frame_value);

                    // `jHs`/`YJr`: the fetch timeout bounds only the time-to-response
                    // (`.send()` resolves on headers, like `await fetch(...)`); once we
                    // hold the response the streaming SSE body is read without this bound.
                    let send_fut = client.post(&post_url).headers(headers).json(&frame).send();
                    let sent = match post_fetch_timeout {
                        Some(t) => match tokio::time::timeout(t, send_fut).await {
                            Ok(r) => r,
                            Err(_) => {
                                tracing::warn!(
                                    "mcp http: POST timed out awaiting response headers"
                                );
                                if let Some(error) = probe_transport_error(
                                    &frame,
                                    "POST timed out awaiting response headers",
                                ) {
                                    let _ = inbound_tx.send(error);
                                }
                                return;
                            }
                        },
                        None => send_fut.await,
                    };
                    let response = match sent {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "mcp http: POST failed");
                            if let Some(error) = probe_transport_error(&frame, &e.to_string()) {
                                let _ = inbound_tx.send(error);
                            }
                            return;
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
                        let body = if is_probe || matches!(status, 400 | 404 | 405) {
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
                        return;
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
                        let mut probe_answered = false;
                        while let Some(item) = events.next().await {
                            match item {
                                Ok(ev) => {
                                    if ev.data.is_empty() {
                                        continue;
                                    }
                                    match serde_json::from_str::<JsonRpcMessage>(&ev.data) {
                                        Ok(msg) => {
                                            observe_protocol_response(
                                                &frame_value,
                                                &msg,
                                                &mut writer_protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
                                            );
                                            let value =
                                                serde_json::to_value(&msg).unwrap_or_default();
                                            probe_answered |=
                                                value.get("id") == frame_value.get("id");
                                            if inbound_tx.send(msg).is_err() {
                                                return;
                                            }
                                            if is_probe && probe_answered {
                                                break;
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                error = %e,
                                                data = %ev.data,
                                                "mcp http: malformed SSE JSON frame, skipping"
                                            );
                                            if let Some(error) = probe_compatibility_error(
                                                &frame,
                                                "malformed discovery response",
                                            ) {
                                                let _ = inbound_tx.send(error);
                                                probe_answered = true;
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "mcp http: sse parse error");
                                    break;
                                }
                            }
                        }
                        if !probe_answered {
                            if let Some(error) =
                                probe_compatibility_error(&frame, "discovery response stream ended")
                            {
                                let _ = inbound_tx.send(error);
                            }
                        }
                    } else {
                        // Single JSON object response.
                        match response.json::<JsonRpcMessage>().await {
                            Ok(msg) => {
                                observe_protocol_response(
                                    &frame_value,
                                    &msg,
                                    &mut writer_protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
                                );
                                let _ = inbound_tx.send(msg);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "mcp http: response not valid JSON-RPC");
                                if let Some(error) = probe_compatibility_error(
                                    &frame,
                                    "malformed discovery response",
                                ) {
                                    let _ = inbound_tx.send(error);
                                }
                            }
                        }
                    }
                        };
                        tokio::select! { biased; _=request_abort.cancelled()=>{}, _=process_frame=>{} }
                    });
                }
            }
        }
        jsonrpc::router::PerRequestCancellation::close(writer_request_cancellation.as_ref());
        posts.abort_all();
        while posts.join_next().await.is_some() {}
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

    let connection = Connection::from_message_streams(Box::pin(inbound), Box::pin(outbound));
    connection.set_per_request_cancellation(request_cancellation);
    Ok(connection)
}

fn apply_body_derived_headers(headers: &mut HeaderMap, request: &serde_json::Value) {
    let Some(version) = request
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(serde_json::Value::as_str)
    else {
        return;
    };
    let Some(method) = request.get("method").and_then(serde_json::Value::as_str) else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(version) {
        headers.insert(HeaderName::from_static("mcp-protocol-version"), value);
    }
    if let Ok(value) = HeaderValue::from_str(method) {
        headers.insert(HeaderName::from_static("mcp-method"), value);
    }
    let name = if method == "resources/read" {
        request.pointer("/params/uri")
    } else {
        request.pointer("/params/name")
    };
    let name = if method.starts_with("tasks/") {
        request
            .pointer("/params/taskId")
            .filter(|value| value.is_string())
            .or(name)
    } else {
        name
    };
    if let Some(name) = name.and_then(serde_json::Value::as_str) {
        // Upstream Er encodes unsafe, whitespace-delimited or already wrapped
        // strings so mcp-name always round-trips through an HTTP field value.
        let needs_encoding = name.is_empty()
            || name.starts_with("=?base64?") && name.ends_with("?=")
            || name.trim_matches(js_whitespace) != name
            || name
                .bytes()
                .any(|byte| byte != b'\t' && !(32..=126).contains(&byte));
        let value = if needs_encoding {
            use base64::Engine;
            format!(
                "=?base64?{}?=",
                base64::engine::general_purpose::STANDARD.encode(name)
            )
        } else {
            name.to_string()
        };
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(HeaderName::from_static("mcp-name"), value);
        }
    }
}

fn js_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

fn observe_protocol_response(
    request: &serde_json::Value,
    response: &JsonRpcMessage,
    protocol_version: &mut Option<String>,
) {
    let response = serde_json::to_value(response).unwrap_or_default();
    if response.get("id") != request.get("id") {
        return;
    }
    if request["method"] == "initialize" {
        if let Some(version) = response
            .pointer("/result/protocolVersion")
            .and_then(serde_json::Value::as_str)
        {
            *protocol_version = Some(version.to_string());
        }
    } else if request["method"] == "server/discover" {
        if let Some(discovery) = response
            .get("result")
            .cloned()
            .and_then(super::mcp_remote::parse_modern_discovery)
        {
            *protocol_version = Some(discovery.version);
        }
    }
}

fn probe_error(
    request: &JsonRpcMessage,
    message: &str,
    transport_failure: bool,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    if request["method"] != "server/discover" {
        return None;
    }
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0", "id": request.get("id")?,
        "error": { "code": -32001, "message": message, "data": {"probeTransportFailure": transport_failure} }
    })).ok()
}

fn probe_transport_error(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    probe_error(request, message, true)
}

fn probe_compatibility_error(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    probe_error(request, message, false)
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

#[cfg(test)]
mod modern_headers_oracle_tests {
    use super::*;

    #[test]
    fn derived_headers_match_pinned_upstream_execution() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../posix/tests/fixtures/mcp_negotiation_286_oracle.json"
        ))
        .unwrap();
        for case in fixture["headerCases"].as_array().unwrap() {
            let mut headers = HeaderMap::new();
            if let Some(initial) = case["initialHeaders"].as_object() {
                for (name, value) in initial {
                    headers.insert(
                        HeaderName::from_bytes(name.as_bytes()).unwrap(),
                        HeaderValue::from_str(value.as_str().unwrap()).unwrap(),
                    );
                }
            }
            apply_body_derived_headers(&mut headers, &case["request"]);
            let actual: serde_json::Map<String, serde_json::Value> = headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.to_string(),
                        serde_json::Value::String(value.to_str().unwrap().to_string()),
                    )
                })
                .collect();
            assert_eq!(
                serde_json::Value::Object(actual),
                case["expected"],
                "{}",
                case["name"]
            );
        }
    }
}

#[cfg(test)]
mod input_required_cancel_tests {
    use super::*;
    use jsonrpc::router::PerRequestCancellation;

    #[test]
    fn request_tokens_are_bounded_by_queued_frames_and_released_when_taken() {
        let cancellation = HttpRequestCancellation::default();
        let id = jsonrpc::Id::Number(1);
        let token = cancellation.prepare(&id);
        assert_eq!(cancellation.pending.lock().unwrap().len(), 1);
        token.cancel();
        let frame = JsonRpcMessage::Request(jsonrpc::Request::new("tools/call", None, id));
        assert!(cancellation.take(&frame).is_cancelled());
        assert!(cancellation.pending.lock().unwrap().is_empty());
        // A replayed or closed queued request cannot mint a fresh live token.
        assert!(cancellation.take(&frame).is_cancelled());
    }

    #[test]
    fn failed_enqueue_and_connection_close_release_all_queued_tokens() {
        let cancellation = HttpRequestCancellation::default();
        let first = jsonrpc::Id::Number(1);
        let second = jsonrpc::Id::Number(2);
        let a = cancellation.prepare(&first);
        let b = cancellation.prepare(&second);
        cancellation.discard(&first);
        assert!(a.is_cancelled());
        assert_eq!(cancellation.pending.lock().unwrap().len(), 1);
        cancellation.close();
        assert!(b.is_cancelled());
        assert!(cancellation.pending.lock().unwrap().is_empty());
        let frame = JsonRpcMessage::Request(jsonrpc::Request::new("tools/call", None, second));
        assert!(cancellation.take(&frame).is_cancelled());
    }
}
