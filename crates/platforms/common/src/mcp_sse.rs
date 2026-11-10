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

use futures::StreamExt;
use jsonrpc::messages::Message as JsonRpcMessage;
use jsonrpc::{BrokerError, Connection, ConnectionError};
use lingxi_core::host::mcp::McpError;
use lingxi_llm_client::transport::http_backend as reqwest;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Default)]
struct LegacyRequestTracking(crate::mcp_http::HttpRequestCancellation);
impl jsonrpc::router::PerRequestCancellation for LegacyRequestTracking {
    fn prepare(&self, id: &jsonrpc::Id) -> lingxi_core::host::mcp_result::CancellationToken {
        jsonrpc::router::PerRequestCancellation::prepare(&self.0, id)
    }
    fn discard(&self, id: &jsonrpc::Id) {
        jsonrpc::router::PerRequestCancellation::discard(&self.0, id);
    }
    fn close(&self) {
        jsonrpc::router::PerRequestCancellation::close(&self.0);
    }
    fn aborts_request_stream(&self) -> bool {
        false
    }
}

/// Time allowed to receive response headers. The SSE body itself stays open.
const MCP_SSE_HEADER_TIMEOUT: Duration = Duration::from_secs(60);

/// Header name used by claude-code IDE plugins for the auth token.
/// LITERAL — must match claude-code byte-for-byte.
/// Source: `claude-code/src/services/mcp/client.ts:713`.
pub const IDE_AUTH_HEADER: &str = "X-LingXi-Ide-Authorization";

/// `User-Agent` value emitted by this client.
/// Retains LingXi product identity with the native MCP CLI shape.
#[must_use]
pub fn user_agent() -> String {
    crate::mcp_http::user_agent()
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
    /// EventSource rejects a successful response with the wrong media type.
    #[error("SSE error: Invalid content type, expected \"text/event-stream\"")]
    InvalidContentType,
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
            other => Self::Connection(other.to_string().into()),
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
    protocol_version: Option<&str>,
) -> Result<HeaderMap, SseConnectError>
where
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let mut h =
        crate::mcp_http::build_common_headers(auth_token, extra_headers, None, protocol_version)
            .map_err(|error| match error {
                crate::mcp_http::HttpConnectError::InvalidAuth(detail) => {
                    SseConnectError::InvalidAuth(detail)
                }
                crate::mcp_http::HttpConnectError::Transport(detail) => {
                    SseConnectError::Transport(detail)
                }
                other => SseConnectError::Transport(other.to_string()),
            })?;
    // EventSource replaces configured Accept. The legacy POST fetch wrapper
    // supplies the default only when the caller did not configure Accept.
    if accept_value == "text/event-stream" || !h.contains_key(reqwest::header::ACCEPT) {
        h.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static(accept_value),
        );
    }
    Ok(h)
}

/// Open an MCP SSE connection.
///
/// `url` is the URL to GET (for events) AND to POST (for outbound requests).
/// `auth_token`, if `Some`, becomes the `X-LingXi-Ide-Authorization`
/// header on BOTH the GET and the POST. `extra_headers` are applied to both
/// directions verbatim. It is generic over the map type so both an unordered
/// `HashMap` and the insertion-ordered [`lingxi_core::host::McpHeaders`] (`IndexMap`)
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
    connect_sse_with_proxy_auth_helper(url, auth_token, extra_headers, endpoint_mode, None).await
}

pub(crate) async fn connect_sse_with_proxy_auth_helper<H>(
    url: &str,
    auth_token: Option<&str>,
    extra_headers: &H,
    endpoint_mode: SseEndpointMode,
    proxy_auth_helper: Option<Arc<dyn lingxi_llm_client::transport::ProxyAuthHelper>>,
) -> Result<Connection, SseConnectError>
where
    H: Clone + Send + 'static,
    for<'a> &'a H: IntoIterator<Item = (&'a String, &'a String)>,
{
    let client = crate::mcp_proxy::build_client_with_proxy_auth_helper(url, proxy_auth_helper)
        .map_err(|e| SseConnectError::Transport(e.to_string()))?;

    // ---- Open the SSE event-stream GET ----
    let get_headers = build_headers(auth_token, extra_headers, "text/event-stream", None)?;
    let response = match tokio::time::timeout(
        MCP_SSE_HEADER_TIMEOUT,
        client.get(url).headers(get_headers).send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(SseConnectError::Transport(error.to_string())),
        Err(_) => {
            return Err(SseConnectError::Transport(
                "timed out waiting for SSE response headers".into(),
            ))
        }
    };

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

    // The legacy EventSource implementation uses this case-sensitive prefix
    // test. Streamable HTTP's POST MIME parser is a different contract.
    if !response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        return Err(SseConnectError::InvalidContentType);
    }

    // Channels: `inbound_tx` ships parsed JSON-RPC `Message`s from the SSE
    // reader task to the Connection. `outbound_rx` receives outbound
    // `Message`s the Connection wants to POST.
    let (inbound_tx, inbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let (outbound_tx, mut outbound_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
    let initialize_request = Arc::new(Mutex::new(None::<serde_json::Value>));
    let protocol_version = Arc::new(Mutex::new(None::<String>));
    let reader_initialize_request = initialize_request.clone();
    let reader_protocol_version = protocol_version.clone();

    // Carries the resolved POST url (or the reason there will not be one) from
    // the reader task to the connect, which cannot complete without it in
    // `EndpointEvent` mode.
    let (endpoint_tx, endpoint_rx) =
        tokio::sync::oneshot::channel::<Result<String, SseConnectError>>();
    let mut endpoint_tx = Some(endpoint_tx);
    let stream_url = url.to_string();
    let current_endpoint = Arc::new(Mutex::new(match endpoint_mode {
        SseEndpointMode::SameUrl => Some(url.to_owned()),
        _ => None,
    }));
    let active = Arc::new(Mutex::new(std::collections::HashMap::<
        jsonrpc::Id,
        (
            JsonRpcMessage,
            lingxi_core::host::mcp_result::CancellationToken,
        ),
    >::new()));
    let tracking = Arc::new(LegacyRequestTracking::default());
    let writer_tracking = tracking.clone();
    let stopped = lingxi_core::host::mcp_result::CancellationToken::new();
    let reader_stopped = stopped.clone();
    let reader_endpoint = current_endpoint.clone();
    let reader_active = active.clone();
    let reader_client = client.clone();
    let reader_auth = auth_token.map(str::to_owned);
    let extra: lingxi_core::host::McpHeaders = extra_headers
        .into_iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let reader_extra = extra.clone();
    let reader_inbound_tx = inbound_tx.clone();
    tokio::spawn(async move {
        let mut response = Some(response);
        let mut last_event_id = None::<String>;
        let mut delay = Duration::from_secs(3);
        'reader: loop {
            if let Some(response) = response.take() {
                let streaming = response
                    .headers()
                    .get(CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(crate::mcp_http::body_budget::is_event_stream);
                let mut budget = crate::mcp_http::body_budget::Budget::new(
                    crate::mcp_http::body_budget::MESSAGE_BYTES,
                    if streaming && response.status().as_u16() == 200 {
                        None
                    } else {
                        Some(crate::mcp_http::body_budget::MESSAGE_BYTES)
                    },
                );
                let mut bytes = response.bytes_stream();
                let mut decoder = crate::mcp_http::sse_codec::Decoder::default();
                loop {
                    let item = tokio::select! { biased; _=reader_inbound_tx.closed()=>break 'reader, _=reader_stopped.cancelled()=>break 'reader, item=bytes.next()=>item };
                    let Some(item) = item else {
                        break;
                    };
                    let chunk = match item {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            tracing::warn!(%error,"mcp sse: stream disconnected");
                            break;
                        }
                    };
                    if let Err(error) = budget.push(&chunk) {
                        for (request, _) in reader_active
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .values()
                        {
                            if let Some(message) = crate::mcp_http::native_frame_error(
                                request,
                                "syntax",
                                &error.diagnostic(),
                            ) {
                                let _ = reader_inbound_tx.send(message);
                            }
                        }
                        break 'reader;
                    }
                    for item in decoder.push(&chunk) {
                        let event = match item {
                            crate::mcp_http::sse_codec::Item::Retry(ms) => {
                                delay = Duration::from_millis(ms);
                                continue;
                            }
                            crate::mcp_http::sse_codec::Item::Event(event) => event,
                        };
                        if let Some(id) = &event.id {
                            last_event_id = (!id.is_empty()).then(|| id.clone());
                        }
                        if event.event == "endpoint" {
                            match resolve_endpoint(&stream_url, &event.data) {
                                Ok(endpoint) => {
                                    *reader_endpoint
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                                        Some(endpoint.clone());
                                    if let Some(tx) = endpoint_tx.take() {
                                        let _ = tx.send(Ok(endpoint));
                                    }
                                }
                                Err(error) => {
                                    if let Some(tx) = endpoint_tx.take() {
                                        let _ = tx.send(Err(error));
                                    }
                                    break 'reader;
                                }
                            }
                            continue;
                        }
                        if event.data.is_empty() || !crate::mcp_http::is_message_event(&event.event)
                        {
                            continue;
                        }
                        match mcp::decode_native_mcp_frame(event.data.as_bytes()) {
                            Ok(message) => {
                                if let Some(request) = reader_initialize_request
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .as_ref()
                                {
                                    crate::mcp_http::observe_protocol_response(
                                        request,
                                        &message,
                                        &mut reader_protocol_version
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                                    );
                                }
                                if let JsonRpcMessage::Response(response) = &message {
                                    if let Some(id) = &response.id {
                                        if let Some((_, completion)) = reader_active
                                            .lock()
                                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                                            .remove(id)
                                        {
                                            completion.cancel();
                                        }
                                    }
                                }
                                if reader_inbound_tx.send(message).is_err() {
                                    break 'reader;
                                }
                            }
                            Err(error) => {
                                let syntax =
                                    lingxi_core::types::utf16_json::Utf16JsonProjection::parse(
                                        &event.data,
                                    )
                                    .is_err();
                                tracing::warn!(%error,syntax,"mcp sse: invalid message");
                                if syntax {
                                    for (request, _) in reader_active
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .values()
                                    {
                                        if let Some(message) = crate::mcp_http::native_frame_error(
                                            request,
                                            "syntax",
                                            &crate::mcp_http::frame_diagnostic(&error),
                                        ) {
                                            let _ = reader_inbound_tx.send(message);
                                        }
                                    }
                                    break 'reader;
                                }
                            }
                        }
                    }
                }
            }
            tokio::select! { biased; _=reader_inbound_tx.closed()=>break, _=reader_stopped.cancelled()=>break, _=tokio::time::sleep(delay)=>{} }
            let version = reader_protocol_version
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let mut headers = match build_headers::<lingxi_core::host::McpHeaders>(
                reader_auth.as_deref(),
                &reader_extra,
                "text/event-stream",
                version.as_deref(),
            ) {
                Ok(headers) => headers,
                Err(_) => break,
            };
            // EventSource replaces configured Accept and owns the replay cursor.
            headers.insert(
                reqwest::header::ACCEPT,
                HeaderValue::from_static("text/event-stream"),
            );
            if let Some(cursor) = &last_event_id {
                if let Ok(value) = HeaderValue::from_str(cursor) {
                    headers.insert("last-event-id", value);
                }
            }
            let fetch = reader_client.get(&stream_url).headers(headers).send();
            let next = tokio::select! { biased; _=reader_inbound_tx.closed()=>break, _=reader_stopped.cancelled()=>break, response=fetch=>response };
            match next {
                Ok(next) if next.status().is_success() => {
                    if !next
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.starts_with("text/event-stream"))
                    {
                        break;
                    }
                    response = Some(next);
                }
                Ok(next) if matches!(next.status().as_u16(), 400 | 401 | 403 | 404 | 410) => break,
                _ => {}
            }
        }
        reader_stopped.cancel();
        if let Some(tx) = endpoint_tx.take() {
            let _ = tx.send(Err(SseConnectError::Endpoint(
                "stream closed before an endpoint event".into(),
            )));
        }
    });

    if !matches!(endpoint_mode, SseEndpointMode::SameUrl) {
        endpoint_rx.await.map_err(|_| {
            SseConnectError::Endpoint("reader stopped before an endpoint event".into())
        })??;
    }
    let post_auth = auth_token.map(str::to_owned);
    tokio::spawn(async move {
        let mut posts = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                biased;
                _=inbound_tx.closed()=>break,
                _=stopped.cancelled()=>break,
                settled=posts.join_next(),if !posts.is_empty()=>{if let Some(Err(error))=settled{tracing::warn!(%error,"mcp sse: POST task failed");}},
                frame=outbound_rx.recv()=>{
                    let Some(frame)=frame else{break;};
                    let request_cleanup=writer_tracking.0.take(&frame);
                    let watcher_active=active.clone();let watcher_stopped=stopped.clone();
                    if let JsonRpcMessage::Request(request)=&frame {
                        let id=request.id.clone();let completion=lingxi_core::host::mcp_result::CancellationToken::new();
                        active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(id.clone(),(frame.clone(),completion.clone()));
                        tokio::spawn(async move {tokio::select! {biased; _=completion.cancelled()=>{}, _=watcher_stopped.cancelled()=>{}, _=request_cleanup.cancelled()=>{watcher_active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&id);}}});
                    }
                    let client=client.clone();let post_auth=post_auth.clone();let extra=extra.clone();let current_endpoint=current_endpoint.clone();let protocol_version=protocol_version.clone();let initialize_request=initialize_request.clone();let inbound_tx=inbound_tx.clone();let active=active.clone();
                    posts.spawn(async move {
                        let version=protocol_version.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                        let mut headers=match build_headers::<lingxi_core::host::McpHeaders>(post_auth.as_deref(),&extra,"application/json, text/event-stream",version.as_deref()){Ok(headers)=>headers,Err(_)=>return};
                        headers.insert(CONTENT_TYPE,HeaderValue::from_static("application/json"));
                        let value=serde_json::to_value(&frame).unwrap_or_default();
                        if value["method"]=="initialize"{*initialize_request.lock().unwrap_or_else(std::sync::PoisonError::into_inner)=Some(value);}
                        let endpoint=current_endpoint.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
                        let Some(endpoint)=endpoint else{return;};
                        let response=tokio::time::timeout(MCP_SSE_HEADER_TIMEOUT,client.post(endpoint).headers(headers).json(&frame).send()).await;
                        let error=match response {
                            Err(_)=>transport_error_message(&frame,"MCP SSE POST timed out"),
                            Ok(Err(error))=>transport_error_message(&frame,&error.to_string()),
                            Ok(Ok(response))=>{
                                let status=response.status().as_u16();
                                let authenticate=response.headers().get(reqwest::header::WWW_AUTHENTICATE).and_then(|value|value.to_str().ok()).map(str::to_owned);
                                if status==202 || ((200..300).contains(&status)&&!matches!(&frame,JsonRpcMessage::Request(_))){return;}
                                let body=crate::mcp_http::read_native_body(response).await.map(|body|String::from_utf8_lossy(&body).into_owned()).unwrap_or_else(|_|"null".to_owned());
                                if !(200..300).contains(&status){http_error_message(&frame,status,authenticate.as_deref(),&body)}else{None}
                            }
                        };
                        if let Some(error)=error{
                            if let JsonRpcMessage::Request(request)=&frame {if let Some((_,completion))=active.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&request.id){completion.cancel();}}
                            let _=inbound_tx.send(error);
                        }
                    });
                }
            }
        }
        stopped.cancel();
        jsonrpc::router::PerRequestCancellation::close(writer_tracking.as_ref());
        posts.abort_all();
        while posts.join_next().await.is_some() {}
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

    let connection = Connection::from_message_streams_with_initial_request_id(
        Box::pin(inbound),
        Box::pin(outbound),
        Some("Method not found".into()),
        0,
    );
    connection.set_per_request_cancellation(tracking);
    Ok(connection)
}

fn transport_error_message(request: &JsonRpcMessage, message: &str) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    if id.is_null() {
        return None;
    }
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32001, "message": message }
    }))
    .ok()
}

fn http_error_message(
    request: &JsonRpcMessage,
    status: u16,
    www_authenticate: Option<&str>,
    body: &str,
) -> Option<JsonRpcMessage> {
    let request = serde_json::to_value(request).ok()?;
    let id = request.get("id")?.clone();
    serde_json::from_value(serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": -32001,
            "message": format!("Error POSTing to endpoint (HTTP {status}): {body}"),
            "data": {
                "httpStatus": status,
                "wwwAuthenticate": www_authenticate, "body": body,
                "nativeFailure":"transport",
                "diagnostic":format!("Error POSTing to endpoint (HTTP {status}): {body}")
            }
        }
    }))
    .ok()
}
