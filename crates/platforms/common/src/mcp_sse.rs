//! MCP SSE transport: GET text/event-stream from `url` for inbound JSON-RPC
//! messages, POST outbound frames back.
//!
//! 🚨 **Where the POST goes depends on [`SseEndpointMode`], and the two modes
//! are different protocols.** An earlier version of this file POSTed to the GET
//! url unconditionally and described that as claude-code's `SSEClientTransport`
//! wire format. It is not: upstream's transport keeps `_url` and `_endpoint`
//! separately and learns the second from a named `endpoint` event. A
//! spec-conformant `type: "sse"` server therefore never received this port's
//! POSTs at all.
//!
//! Wire contract (LITERAL), both modes:
//! - GET `Accept: text/event-stream`
//! - When `auth_token` is `Some`, both GET and POST carry
//!   `X-LingXi-Ide-Authorization: <token>` verbatim (no `Bearer ` prefix).
//! - `Content-Type: application/json` on POST.
//! - A DEFAULT-event frame is one JSON-RPC `Message`: `data: {...}\n\n`
//!   (upstream's `onmessage`).
//!
//! [`SseEndpointMode::EndpointEvent`] adds upstream's handshake:
//! - A named `endpoint` event carries the POST url, resolved RELATIVE to the
//!   stream url, and it must be SAME-ORIGIN.
//! - The connect does not complete until that event arrives — upstream
//!   resolves its connect promise from inside the listener, and refuses to
//!   send before then (`NotConnected`).

use eventsource_stream::Eventsource;
use futures::StreamExt;
use jsonrpc::messages::Message as JsonRpcMessage;
use jsonrpc::{BrokerError, Connection, ConnectionError};
use platform_api::mcp::McpError;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE, USER_AGENT};
use thiserror::Error;
use tokio::sync::mpsc;

/// Header name used by claude-code IDE plugins for the auth token.
/// LITERAL — must match claude-code byte-for-byte.
/// Source: `claude-code/src/services/mcp/client.ts:713`.
pub const IDE_AUTH_HEADER: &str = "X-LingXi-Ide-Authorization";

/// `User-Agent` value emitted by this client.
/// Matches claude-code's `getMCPUserAgent()` shape: `claude-code/<version>`.
#[must_use]
pub fn user_agent() -> String {
    format!("claude-code/{}", env!("CARGO_PKG_VERSION"))
}

/// How the POST url for outbound frames is determined.
///
/// These are two different protocols sharing one transport, so the caller says
/// which one it is speaking rather than the transport guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SseEndpointMode {
    /// The MCP legacy HTTP+SSE contract (`McpTransportSpec::Sse`): the server
    /// names the POST url in a named `endpoint` event, resolved relative to the
    /// stream url and required to be same-origin. The connect completes only
    /// once that event has arrived.
    EndpointEvent,
    /// POST to the same url the stream was opened on
    /// (`McpTransportSpec::SseIde`). This is the IDE contract, where the
    /// extension serves both on one url and sends no `endpoint` event.
    SameUrl,
    /// [`Self::EndpointEvent`], plus upstream's 401 guard on the stream GET.
    /// Used ONLY when rescuing a streamable-HTTP server whose `initialize`
    /// POST was rejected — never for a directly configured `type: "sse"`
    /// server, which must stay free to authenticate normally.
    ///
    /// Upstream puts the guard inside the fallback-only transport factory:
    ///
    /// ```js
    /// if(!Te && !Xe){                       // Te = postMethodNotAllowed
    ///   if(ft.status===401) throw Error("legacy HTTP+SSE stream GET answered 401     ///     after the initialize POST was not a 405; not starting OAuth against this URL");
    ///   Xe = ft.ok }
    /// ```
    ///
    /// A 405 says "wrong method here", which is real evidence the url is an SSE
    /// endpoint. A 400 or 404 is not, so a 401 on the stream GET must not be
    /// allowed to point an OAuth flow at a url that may not be an MCP endpoint
    /// at all.
    LegacyRescue {
        /// Whether the streamable `initialize` POST was rejected with 405.
        post_method_not_allowed: bool,
    },
}

/// Errors specific to opening an MCP SSE connection.
#[derive(Debug, Error)]
pub enum SseConnectError {
    /// HTTP request setup or send failed.
    #[error("sse transport error: {0}")]
    Transport(String),
    /// Authorization header value was invalid (non-ASCII, control chars).
    #[error("invalid auth token: {0}")]
    InvalidAuth(String),
    /// The stream ended, or produced a bad `endpoint` event, before the POST
    /// url was known. Upstream rejects its connect promise from inside the
    /// `endpoint` listener for exactly these cases.
    #[error("sse endpoint handshake failed: {0}")]
    Endpoint(String),
    /// Remote response retained for authentication recovery.
    #[error("HTTP {status}{detail}", detail = www_authenticate.as_ref().map(|value| format!(": {value}")).unwrap_or_default())]
    HttpResponse {
        /// HTTP status code.
        status: u16,
        /// `WWW-Authenticate` response header.
        www_authenticate: Option<String>,
    },
}

impl From<SseConnectError> for McpError {
    fn from(value: SseConnectError) -> Self {
        match value {
            SseConnectError::HttpResponse {
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

/// Resolve an `endpoint` event's data against the stream url.
///
/// Upstream, verbatim:
///
/// ```js
/// this._endpoint = new URL(o.data, this._url);
/// if (this._endpoint.origin !== this._url.origin)
///   throw Error(`Endpoint origin does not match connection origin: ${this._endpoint.origin}`);
/// ```
///
/// The same-origin check is the load-bearing half: without it a server could
/// name any host and this transport would POST the session's JSON-RPC — tools,
/// arguments and all — to it. `Url::join` performs the relative resolution, so
/// a bare path like `/messages?sessionId=…` lands on the stream's own origin.
fn resolve_endpoint(stream_url: &str, data: &str) -> Result<String, SseConnectError> {
    let base = url::Url::parse(stream_url)
        .map_err(|e| SseConnectError::Endpoint(format!("stream url is not a url: {e}")))?;
    let resolved = base
        .join(data.trim())
        .map_err(|e| SseConnectError::Endpoint(format!("endpoint {data:?} is not a url: {e}")))?;
    if resolved.origin() != base.origin() {
        return Err(SseConnectError::Endpoint(format!(
            "Endpoint origin does not match connection origin: {}",
            resolved.origin().ascii_serialization()
        )));
    }
    Ok(resolved.to_string())
}

fn build_headers<H>(
    auth_token: Option<&str>,
    extra_headers: &H,
    accept_value: &'static str,
) -> Result<HeaderMap, SseConnectError>
where
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut h = HeaderMap::new();
    h.insert(ACCEPT, HeaderValue::from_static(accept_value));
    h.insert(
        USER_AGENT,
        HeaderValue::try_from(user_agent())
            .map_err(|e| SseConnectError::Transport(e.to_string()))?,
    );
    if let Some(token) = auth_token {
        let v = HeaderValue::try_from(token)
            .map_err(|e| SseConnectError::InvalidAuth(e.to_string()))?;
        h.insert(HeaderName::from_static("x-lingxi-ide-authorization"), v);
    }
    for (k, v) in extra_headers {
        let name = HeaderName::try_from(k.as_str())
            .map_err(|e| SseConnectError::Transport(format!("bad header name {k}: {e}")))?;
        let val = HeaderValue::try_from(v.as_str())
            .map_err(|e| SseConnectError::Transport(format!("bad header value: {e}")))?;
        h.insert(name, val);
    }
    Ok(h)
}

/// Open an MCP SSE connection.
///
/// `url` is the URL to GET (for events) AND to POST (for outbound requests).
/// `auth_token`, if `Some`, becomes the `X-LingXi-Ide-Authorization`
/// header on BOTH the GET and the POST. `extra_headers` are applied to both
/// directions verbatim. It is generic over the map type so both an unordered
/// `HashMap` and the insertion-ordered [`platform_api::McpHeaders`] (`IndexMap`)
/// the MCP transport specs now carry are accepted (header order is irrelevant
/// to the emitted HTTP request).
///
/// # Errors
///
/// - [`SseConnectError::Transport`] if reqwest client construction or the
///   initial GET request fails, or the server returns a non-2xx status on
///   the event-stream GET.
/// - [`SseConnectError::InvalidAuth`] if `auth_token` contains bytes that
///   cannot be expressed in an HTTP header value (non-ASCII, control chars).
pub async fn connect_sse<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    endpoint_mode: SseEndpointMode,
) -> Result<Connection, SseConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| SseConnectError::Transport(e.to_string()))?;

    // ---- Open the SSE event-stream GET ----
    let get_headers = build_headers(auth_token, extra_headers, "text/event-stream")?;
    let response = client
        .get(url)
        .headers(get_headers)
        .send()
        .await
        .map_err(|e| SseConnectError::Transport(e.to_string()))?;

    if response.status() == reqwest::StatusCode::UNAUTHORIZED
        && matches!(
            endpoint_mode,
            SseEndpointMode::LegacyRescue {
                post_method_not_allowed: false
            }
        )
    {
        return Err(SseConnectError::Endpoint(
            "legacy HTTP+SSE stream GET answered 401 after the initialize POST was not a 405; \
             not starting OAuth against this URL"
                .to_string(),
        ));
    }
    if !response.status().is_success() {
        return Err(SseConnectError::HttpResponse {
            status: response.status().as_u16(),
            www_authenticate: response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
        });
    }

    let byte_stream = response.bytes_stream();
    let mut event_stream = byte_stream.eventsource();

    // Channels: `inbound_tx` ships parsed JSON-RPC `Message`s from the SSE
    // reader task to the Connection. `outbound_rx` receives outbound
    // `Message`s the Connection wants to POST.
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();

    // Carries the resolved POST url (or the reason there will not be one) from
    // the reader task to the connect, which cannot complete without it in
    // `EndpointEvent` mode.
    let (endpoint_tx, endpoint_rx) =
        tokio::sync::oneshot::channel::<Result<String, SseConnectError>>();
    let mut endpoint_tx = Some(endpoint_tx);
    let stream_url = url.to_string();

    // SSE reader task: parse `data:` JSON frames and forward as Message.
    let reader_inbound_tx = inbound_tx.clone();
    tokio::spawn(async move {
        while let Some(item) = event_stream.next().await {
            match item {
                Ok(event) => {
                    // A NAMED `endpoint` event is the legacy handshake, not a
                    // JSON-RPC frame: upstream resolves its data against the
                    // stream url and requires the same origin, rejecting the
                    // connect otherwise. Handled before the default-event path
                    // below so it is never parsed as a message.
                    if event.event == "endpoint" {
                        if let Some(tx) = endpoint_tx.take() {
                            let _ = tx.send(resolve_endpoint(&stream_url, &event.data));
                        }
                        continue;
                    }
                    // claude-code's SDK uses default-event SSE (no `event:` name)
                    // with the `data:` field carrying the JSON-RPC frame. Skip
                    // keep-alive comments / empty data lines.
                    if event.data.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<JsonRpcMessage>(&event.data) {
                        Ok(msg) => {
                            if reader_inbound_tx.send(msg).is_err() {
                                // Connection dropped; stop reading.
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                data = %event.data,
                                "mcp sse: malformed JSON frame, skipping"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mcp sse: stream error, terminating reader");
                    if let Some(tx) = endpoint_tx.take() {
                        let _ = tx.send(Err(SseConnectError::Endpoint(format!(
                            "stream ended before an endpoint event: {e}"
                        ))));
                    }
                    break;
                }
            }
        }
        // The stream closed cleanly without ever naming an endpoint. Report it
        // rather than leaving a connect awaiting a sender that has been
        // dropped.
        if let Some(tx) = endpoint_tx.take() {
            let _ = tx.send(Err(SseConnectError::Endpoint(
                "stream closed before an endpoint event".to_string(),
            )));
        }
    });

    // POST writer task: drain outbound frames and POST each as one JSON-RPC
    // body. Reuses the same `reqwest::Client` so the underlying connection
    // pool serves both GET and POST.
    // `SameUrl` is the IDE contract; `EndpointEvent` waits for the server to
    // name the url, which is what makes this a legacy HTTP+SSE client rather
    // than a transport that posts into the stream url and hopes.
    let post_url = match endpoint_mode {
        SseEndpointMode::SameUrl => url.to_string(),
        SseEndpointMode::EndpointEvent | SseEndpointMode::LegacyRescue { .. } => {
            endpoint_rx.await.map_err(|_| {
                SseConnectError::Endpoint("reader stopped before an endpoint event".to_string())
            })??
        }
    };
    let post_auth = auth_token.map(str::to_string);
    let post_extra = extra_headers.clone();
    let post_client = client.clone();
    tokio::spawn(async move {
        while let Some(frame) = outbound_rx.recv().await {
            // claude-code's SSEClientTransport POST sends application/json;
            // the Accept header is also kept on POST for symmetry with the
            // GET, matching claude-code's SDK which sets both transports'
            // Accept to `text/event-stream`.
            let mut headers =
                match build_headers(post_auth.as_deref(), &post_extra, "text/event-stream") {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::error!(error = %e, "mcp sse: failed to build POST headers");
                        break;
                    }
                };
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

            let response = post_client
                .post(&post_url)
                .headers(headers)
                .json(&frame)
                .send()
                .await;
            match response {
                Err(error) => {
                    tracing::warn!(%error, "mcp sse: POST failed");
                }
                Ok(response) if !response.status().is_success() => {
                    let status = response.status().as_u16();
                    let www_authenticate = response
                        .headers()
                        .get(reqwest::header::WWW_AUTHENTICATE)
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    if let Some(error) =
                        http_error_message(&frame, status, www_authenticate.as_deref())
                    {
                        if inbound_tx.send(error).is_err() {
                            break;
                        }
                    }
                }
                Ok(_) => {}
            }
        }
    });

    // Adapt the inbound mpsc into Stream<Item = Message>.
    let inbound = futures::stream::unfold(inbound_rx, |mut rx| async move {
        rx.recv().await.map(|m| (m, rx))
    });

    // Adapt the outbound mpsc into Sink<Message, Error = ConnectionError>.
    let outbound = futures::sink::unfold(outbound_tx, |tx, msg: JsonRpcMessage| async move {
        tx.send(msg).map_err(|_| {
            ConnectionError::Broker(BrokerError::Join("sse writer task closed".into()))
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
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32001,
            "message": format!(
                "MCP_HTTP_STATUS={status};WWW_AUTHENTICATE={}",
                www_authenticate.unwrap_or_default()
            ),
            "data": {
                "httpStatus": status,
                "wwwAuthenticate": www_authenticate
            }
        }
    }))
    .ok()
}
