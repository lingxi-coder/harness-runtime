//! Shared `reqwest`-backed [`HttpTransport`] for native hosts.
//!
//! `platforms/posix` (desktop) re-exports [`ReqwestHttp`] as `PosixHttp`, and
//! the mobile device platforms (`platforms/ios`, `platforms/android`) use it
//! directly so a keyed conversation streams against the real provider.
//!
//! Implements request/response via `reqwest::Client`. SSE streaming is wired
//! through `reqwest::Response::bytes_stream()` and a buffered `\n\n` /
//! `\r\n\r\n` boundary scanner that parses SSE events via the local
//! `parse_sse_chunks` helper (ported verbatim from the former api-client crate).

use async_trait::async_trait;
use bytes::BytesMut;
use futures_core::stream::Stream;
use futures_util::stream::StreamExt;
use lingxi_core::host::http::{
    RawByteStream, RawByteStreamWithMeta, ResolvedAddressOverride, SseStream, SseStreamWithMeta,
};
use lingxi_core::host::{HttpError, HttpTransport};
use lingxi_core::types::{HttpRequest, HttpResponse, SseEvent};
use std::error::Error as StdError;
use std::sync::Arc;

/// TCP/TLS connect budget. Applied on the `reqwest::Client`, never as a
/// request-level timeout (that would kill long-lived SSE bodies).
const DEFAULT_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Cap on buffered (non-SSE) HTTP response bodies. Matches JSON-RPC/MCP
/// `DEFAULT_MAX_FRAME_SIZE` so a runaway peer cannot exhaust memory.
const MAX_HTTP_RESPONSE_BODY: usize = 16 * 1024 * 1024;

/// Production HTTP transport using `reqwest::Client`.
///
/// Built on `reqwest` with the `rustls-tls` backend (no OpenSSL), which
/// cross-compiles to `aarch64-apple-ios` and Android targets. Shared by all
/// native platforms.
pub struct ReqwestHttp {
    /// Default client — follows redirects (reqwest's default policy). Backs
    /// [`HttpTransport::request`] / `stream_sse` / `stream_raw_bytes`.
    client: reqwest::Client,
    /// No-redirect client built with `reqwest::redirect::Policy::none()`. Backs
    /// [`HttpTransport::request_no_follow`] so a 3xx is surfaced to the caller
    /// verbatim (status + `Location`) — mirrors claude-code's `maxRedirects: 0`.
    no_redirect_client: reqwest::Client,
    /// Whether connection failures include their nested reqwest/hyper cause
    /// chain. Mobile enables this so DNS/TLS failures can be rendered as
    /// actionable UI copy; the default stays `false` to preserve desktop/CLI
    /// error text exactly.
    detailed_connection_errors: bool,
    /// Snapshot of the TLS / CA settings so per-request DNS pinning can build a
    /// client with identical trust and mTLS material.
    tls: crate::tls_config::TlsSettings,
    /// WebSocket rustls config assembled from the same root stores and client
    /// identity as `tls`. Configuration errors are deferred until a WebSocket
    /// is actually opened so ordinary HTTP remains available for diagnostics.
    websocket_tls: Result<Arc<rustls::ClientConfig>, String>,
    monitor_proxy: Option<Arc<dyn lingxi_core::host::http::MonitorWebSocketProxy>>,
}

impl ReqwestHttp {
    /// Build a new `ReqwestHttp` with a fresh `reqwest::Client`.
    ///
    /// # Panics
    /// Panics if the underlying TLS stack cannot be initialised — this is a
    /// fatal startup error and the process should not continue.
    #[must_use]
    pub fn new() -> Self {
        Self::build(false)
    }

    /// Build a transport that retains nested DNS/TCP/TLS failure details.
    ///
    /// This is an explicit mobile diagnostic mode. [`Self::new`] deliberately
    /// keeps the legacy one-line reqwest message so desktop and CLI behavior do
    /// not change when mobile opts into actionable network errors.
    #[must_use]
    pub fn new_with_detailed_connection_errors() -> Self {
        Self::build(true)
    }

    fn build(detailed_connection_errors: bool) -> Self {
        // mTLS client identity + custom CA-trust (`CLAUDE_CODE_CLIENT_CERT` /
        // `_KEY` / `_KEY_PASSPHRASE` / `CERT_STORE`, `NODE_EXTRA_CA_CERTS`) is
        // read once here and applied identically to BOTH clients so a corporate
        // mutual-TLS / custom-CA endpoint is reachable on every request path.
        let tls = crate::tls_config::TlsSettings::from_env();
        let websocket_tls = tls.websocket_client_config();
        Self {
            client: tls
                .apply_to_builder(
                    reqwest::Client::builder().connect_timeout(DEFAULT_CONNECT_TIMEOUT),
                )
                .build()
                .expect("reqwest client init"),
            no_redirect_client: tls
                .apply_to_builder(
                    reqwest::Client::builder()
                        .connect_timeout(DEFAULT_CONNECT_TIMEOUT)
                        .redirect(reqwest::redirect::Policy::none()),
                )
                .build()
                .expect("reqwest no-redirect client init"),
            detailed_connection_errors,
            tls,
            websocket_tls,
            monitor_proxy: None,
        }
    }

    /// Inject the host's existing HTTP(S) CONNECT policy for Monitor sockets.
    #[must_use]
    pub fn with_monitor_proxy(
        mut self,
        proxy: Arc<dyn lingxi_core::host::http::MonitorWebSocketProxy>,
    ) -> Self {
        self.monitor_proxy = Some(proxy);
        self
    }

    fn client_for_resolved_request(
        &self,
        resolved: &ResolvedAddressOverride,
        no_redirect: bool,
    ) -> Result<reqwest::Client, HttpError> {
        let mut builder = reqwest::Client::builder().connect_timeout(DEFAULT_CONNECT_TIMEOUT);
        if no_redirect {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        self.tls
            .apply_to_builder(builder)
            .resolve_to_addrs(&resolved.domain, &resolved.addrs)
            .build()
            .map_err(|err| HttpError::InvalidRequest(err.to_string()))
    }

    async fn send_request(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
        no_redirect: bool,
    ) -> Result<HttpResponse, HttpError> {
        let client = match resolved.as_ref() {
            Some(resolved) if resolved.addrs.is_empty() => {
                return Err(HttpError::InvalidRequest(
                    "pre-resolved address override must contain at least one address".to_string(),
                ));
            }
            Some(resolved) => self.client_for_resolved_request(resolved, no_redirect)?,
            _ if no_redirect => self.no_redirect_client.clone(),
            _ => self.client.clone(),
        };
        let is_head = matches!(req.method, lingxi_core::types::HttpMethod::Head);
        let resp = build_reqwest(&client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        if !is_head
            && resp
                .content_length()
                .is_some_and(|len| len > MAX_HTTP_RESPONSE_BODY as u64)
        {
            return Err(HttpError::InvalidResponse(format!(
                "response body exceeds {MAX_HTTP_RESPONSE_BODY} bytes"
            )));
        }
        let mut raw = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| HttpError::InvalidResponse(e.to_string()))?;
            if raw.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BODY {
                return Err(HttpError::InvalidResponse(format!(
                    "response body exceeds {MAX_HTTP_RESPONSE_BODY} bytes"
                )));
            }
            raw.extend_from_slice(&chunk);
        }
        let body = String::from_utf8_lossy(&raw).into_owned();
        Ok(HttpResponse {
            status,
            headers,
            body,
            body_bytes: raw,
        })
    }
}

impl Default for ReqwestHttp {
    fn default() -> Self {
        Self::new()
    }
}

fn to_reqwest_method(method: lingxi_core::types::HttpMethod) -> reqwest::Method {
    match method {
        lingxi_core::types::HttpMethod::Get => reqwest::Method::GET,
        lingxi_core::types::HttpMethod::Post => reqwest::Method::POST,
        lingxi_core::types::HttpMethod::Put => reqwest::Method::PUT,
        lingxi_core::types::HttpMethod::Patch => reqwest::Method::PATCH,
        lingxi_core::types::HttpMethod::Delete => reqwest::Method::DELETE,
        lingxi_core::types::HttpMethod::Head => reqwest::Method::HEAD,
        lingxi_core::types::HttpMethod::Options => reqwest::Method::OPTIONS,
    }
}

/// Build a `reqwest::RequestBuilder` from a protocol `HttpRequest`.
///
/// Shared by all `HttpTransport` method impls on [`ReqwestHttp`] so headers,
/// body, and timeout are applied identically regardless of which method
/// ([`HttpTransport::request`] / [`HttpTransport::stream_sse`] /
/// [`HttpTransport::stream_sse_with_meta`] / [`HttpTransport::stream_raw_bytes`])
/// calls it.
fn build_reqwest(client: &reqwest::Client, req: HttpRequest) -> reqwest::RequestBuilder {
    build_reqwest_inner(client, req, true)
}

/// Streaming constructors must not apply `HttpRequest::timeout` — that budget
/// covers the whole body, which for SSE is unbounded. Connect is still bounded
/// by [`DEFAULT_CONNECT_TIMEOUT`] on the client.
fn build_reqwest_stream(client: &reqwest::Client, req: HttpRequest) -> reqwest::RequestBuilder {
    build_reqwest_inner(client, req, false)
}

fn build_reqwest_inner(
    client: &reqwest::Client,
    req: HttpRequest,
    apply_request_timeout: bool,
) -> reqwest::RequestBuilder {
    let mut rb = client.request(to_reqwest_method(req.method), &req.url);
    for (k, v) in &req.headers {
        rb = rb.header(k, v);
    }
    // Raw bytes take precedence over the string body (see `HttpRequest::body_bytes`).
    if let Some(bytes) = req.body_bytes {
        rb = rb.body(bytes);
    } else if let Some(body) = req.body {
        rb = rb.body(body);
    }
    if apply_request_timeout {
        if let Some(timeout) = req.timeout {
            rb = rb.timeout(timeout);
        }
    }
    rb
}

fn render_error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut messages = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !message.is_empty() && !messages.iter().any(|seen| seen == &message) {
            messages.push(message);
        }
        source = cause.source();
    }
    messages.join(": ")
}

fn map_reqwest_connection_error(error: reqwest::Error, detailed: bool) -> HttpError {
    let message = if detailed {
        render_error_chain(&error)
    } else {
        error.to_string()
    };
    HttpError::Connection(message)
}

#[async_trait]
impl HttpTransport for ReqwestHttp {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_request(req, None, false).await
    }

    async fn request_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<HttpResponse, HttpError> {
        self.send_request(req, resolved, false).await
    }

    /// Override that sends via the [`Self::no_redirect_client`]
    /// (`redirect::Policy::none()`) so a 3xx response is surfaced to the caller
    /// as `Ok(status=3xx)` with its `Location` header intact — instead of being
    /// transparently followed by reqwest's default client.
    ///
    /// Mirrors claude-code's `axios.get(..., { maxRedirects: 0 })`: callers such
    /// as `WebFetchTool` apply their own permitted-redirect policy on the 3xx.
    /// Response mapping is identical to [`Self::request`]; only the client (and
    /// thus the redirect policy) differs.
    async fn request_no_follow(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.send_request(req, None, true).await
    }

    async fn request_no_follow_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<HttpResponse, HttpError> {
        self.send_request(req, resolved, true).await
    }

    async fn stream_sse(&self, req: HttpRequest) -> Result<SseStream, HttpError> {
        let resp = build_reqwest_stream(&self.client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            // Surface the status to the caller — claude-code returns this as
            // an error event on the same stream, but we keep them separate
            // because the trait promises `Result<SseStream, HttpError>` at the
            // open boundary, not after the first event.
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError::Status { status, body });
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = sse_event_stream(byte_stream, self.detailed_connection_errors);
        Ok(Box::pin(event_stream))
    }

    /// Override that captures the real HTTP status and response headers
    /// (lowercased) before the SSE event stream begins.
    ///
    /// Unlike the default (which loses metadata by delegating to
    /// [`Self::stream_sse`]), this override reads the response line and
    /// headers before handing the byte-stream to the SSE decoder — so callers
    /// can immediately inspect rate-limit headers such as `retry-after`.
    ///
    /// # ≥400 error-arm behaviour
    ///
    /// For non-2xx responses this method returns `Ok(SseStreamWithMeta{status:
    /// 4xx, headers: <real headers>, stream: <body-as-single-raw-data-frame>})`
    /// so the bridge's existing ≥400 drain path receives real response headers
    /// (including `retry-after`, `anthropic-ratelimit-*`, etc.) instead of an
    /// empty `BTreeMap`. The body is emitted as a single SSE-`data:`-style raw
    /// frame that the bridge drains and passes to the codec.
    ///
    /// The bare `Err(HttpError::Status)` arm is now only reached by transports
    /// that do NOT override `stream_sse_with_meta` (i.e. the default
    /// implementation in `traits`). Those callers produce empty headers as
    /// before — no behaviour change for default-impl transports.
    async fn stream_sse_with_meta(&self, req: HttpRequest) -> Result<SseStreamWithMeta, HttpError> {
        let resp = build_reqwest_stream(&self.client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        // Capture headers (lowercased) BEFORE consuming the body, whether the
        // response is a success or an error — this is the key improvement over
        // `stream_sse`: error responses now carry real headers.
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        if status >= 400 {
            // Return Ok so the bridge receives real headers alongside the body.
            // The body is delivered as a single raw SSE data frame so the
            // bridge's drain loop (which calls `next_frame()` until `None`)
            // accumulates it into the JSON body handed to `decode_response`.
            let body = resp.bytes().await.unwrap_or_default();
            let body_bytes = body.to_vec();
            let stream = body_error_stream(body_bytes);
            return Ok(SseStreamWithMeta {
                status,
                headers,
                stream: Box::pin(stream),
            });
        }

        let byte_stream = resp.bytes_stream();
        let event_stream = sse_event_stream(byte_stream, self.detailed_connection_errors);
        Ok(SseStreamWithMeta {
            status,
            headers,
            stream: Box::pin(event_stream),
        })
    }

    async fn stream_raw_bytes(&self, req: HttpRequest) -> Result<RawByteStream, HttpError> {
        let resp = build_reqwest_stream(&self.client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(HttpError::Status { status, body });
        }
        // Map reqwest's `Bytes` chunks to owned `Vec<u8>` for true incremental
        // streaming (AWS event-stream frames arrive across chunks).
        let detailed = self.detailed_connection_errors;
        let s = resp.bytes_stream().map(move |r| {
            r.map(|b| b.to_vec())
                .map_err(|e| map_reqwest_connection_error(e, detailed))
        });
        Ok(Box::pin(s))
    }

    /// Override that captures the real HTTP status and response headers before
    /// the raw byte stream begins.
    ///
    /// Mirrors the [`Self::stream_sse_with_meta`] override: status and headers
    /// are captured from the response line before any body bytes are consumed,
    /// so callers can immediately inspect rate-limit headers.
    ///
    /// # ≥400 error-arm behaviour
    ///
    /// For non-2xx responses this method returns
    /// `Ok(RawByteStreamWithMeta{status: 4xx, headers: <real headers>, stream:
    /// <body-as-one-chunk>})` so the bridge's drain path receives real response
    /// headers instead of an empty set.  The body is emitted as a single chunk.
    async fn stream_raw_bytes_with_meta(
        &self,
        req: HttpRequest,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        let resp = build_reqwest_stream(&self.client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        // Capture headers (lowercased) BEFORE consuming the body.
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        if status >= 400 {
            // Return Ok so the bridge can receive real headers alongside the body.
            let body_bytes = resp.bytes().await.unwrap_or_default().to_vec();
            let stream: RawByteStream = Box::pin(futures_util::stream::once(async move {
                Ok::<Vec<u8>, HttpError>(body_bytes)
            }));
            return Ok(RawByteStreamWithMeta {
                status,
                headers,
                stream,
            });
        }

        // Success: map reqwest's `Bytes` chunks to owned `Vec<u8>`.
        let detailed = self.detailed_connection_errors;
        let byte_stream = resp.bytes_stream().map(move |r| {
            r.map(|b| b.to_vec())
                .map_err(|e| map_reqwest_connection_error(e, detailed))
        });
        Ok(RawByteStreamWithMeta {
            status,
            headers,
            stream: Box::pin(byte_stream),
        })
    }

    async fn stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        let client = match resolved.as_ref() {
            Some(resolved) if resolved.addrs.is_empty() => {
                return Err(HttpError::InvalidRequest(
                    "pre-resolved address override must contain at least one address".to_string(),
                ));
            }
            Some(resolved) => self.client_for_resolved_request(resolved, true)?,
            _ => self.no_redirect_client.clone(),
        };
        let resp = build_reqwest_stream(&client, req)
            .send()
            .await
            .map_err(|e| map_reqwest_connection_error(e, self.detailed_connection_errors))?;
        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();

        // The shared SDK bounds error-body collection itself. Preserve this
        // stream for every status instead of eagerly collecting error bodies.
        let detailed = self.detailed_connection_errors;
        let byte_stream = resp.bytes_stream().map(move |r| {
            r.map(|b| b.to_vec())
                .map_err(|e| map_reqwest_connection_error(e, detailed))
        });
        Ok(RawByteStreamWithMeta {
            status,
            headers,
            stream: Box::pin(byte_stream),
        })
    }

    async fn preflight_monitor_websocket(&self, url: &str) -> Result<(), HttpError> {
        crate::monitor_websocket::preflight(url).await.map(|_| ())
    }

    async fn monitor_websocket(
        &self,
        url: String,
        protocols: Vec<String>,
    ) -> Result<lingxi_core::host::http::MonitorWebSocketReceiver, HttpError> {
        let tls = self
            .websocket_tls
            .as_ref()
            .map_err(|error| HttpError::InvalidRequest(error.clone()))?
            .clone();
        crate::monitor_websocket::connect(url, protocols, tls, self.monitor_proxy.clone()).await
    }
}

/// Parse one or more complete SSE events out of a raw chunk.
///
/// The chunk MUST end with `\n\n` to terminate the last event; partial events
/// are dropped. Conforms to the HTML SSE spec (event / data / id fields;
/// comment lines starting with `:` are skipped).
///
/// Ported verbatim from the former `api-client/src/sse.rs` so the wire
/// behaviour is byte-identical after the api-client crate is removed.
///
/// NOTE: duplicated in `platforms/windows/src/http.rs` — keep in sync.
fn parse_sse_chunks(raw: &str) -> Vec<SseEvent> {
    let mut events = Vec::new();
    for block in raw.split("\n\n") {
        if block.trim().is_empty() {
            continue;
        }
        let mut event_type: Option<String> = None;
        let mut data_lines: Vec<String> = Vec::new();
        let mut id: Option<String> = None;
        for line in block.lines() {
            if line.starts_with(':') {
                continue; // comment / keepalive
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event_type = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("id:") {
                id = Some(rest.trim().to_string());
            }
        }
        if !data_lines.is_empty() {
            events.push(SseEvent {
                event_type,
                data: data_lines.join("\n"),
                id,
            });
        }
    }
    events
}

/// Adapt a byte stream into a stream of complete `SseEvent`s.
///
/// Buffers raw bytes until an event boundary (`\n\n` or `\r\n\r\n`) is found,
/// then hands the buffered slice off to `parse_sse_chunks`. Each emitted
/// element is a single fully-formed `SseEvent` (or a connection error).
pub(crate) fn sse_event_stream<S>(
    byte_stream: S,
    detailed_connection_errors: bool,
) -> impl Stream<Item = Result<SseEvent, HttpError>> + Send
where
    S: Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
{
    let buffer = BytesMut::new();
    futures_util::stream::unfold(
        (Box::pin(byte_stream), buffer),
        move |(mut s, mut buf)| async move {
            loop {
                // First, drain any complete events already in the buffer.
                if let Some((event_len, boundary_len)) = find_event_boundary(&buf) {
                    let event_bytes = buf.split_to(event_len).to_vec();
                    // Consume the boundary itself.
                    drop(buf.split_to(boundary_len));
                    let chunk = String::from_utf8_lossy(&event_bytes).to_string();
                    let events = parse_sse_chunks(&format!("{chunk}\n\n"));
                    if let Some(ev) = events.into_iter().next() {
                        return Some((Ok(ev), (s, buf)));
                    }
                    // Empty event (comment / keepalive only); keep draining.
                    continue;
                }
                // Otherwise pull more bytes.
                match s.next().await {
                    Some(Ok(bytes)) => buf.extend_from_slice(&bytes),
                    Some(Err(e)) => {
                        return Some((
                            Err(map_reqwest_connection_error(e, detailed_connection_errors)),
                            (s, buf),
                        ));
                    }
                    None => return None,
                }
            }
        },
    )
}

/// Adapt an error-response body (raw bytes) into a single-item `SseEvent` stream.
///
/// The body is delivered as one `SseEvent{data: <body-as-utf8>, …}` frame so the
/// bridge's `SseFrames::next_frame` path yields the raw bytes that the codec
/// `decode_response` call expects.  The stream terminates immediately after that
/// single frame (returns `None` on the second poll) so the bridge drain loop sees
/// exactly one frame.
///
/// This is the companion to the `stream_sse_with_meta` ≥400 path — it lets the
/// bridge receive real headers and the error body without an `Err(HttpError::Status)`
/// short-circuit.
pub(crate) fn body_error_stream(
    body: Vec<u8>,
) -> impl Stream<Item = Result<SseEvent, HttpError>> + Send {
    futures_util::stream::once(async move {
        let data = String::from_utf8_lossy(&body).into_owned();
        Ok(SseEvent {
            event_type: None,
            data,
            id: None,
        })
    })
}

/// Locate the first SSE event boundary in `buf`.
///
/// Returns `(event_len, boundary_len)` where `event_len` is the byte length of
/// the event payload (everything before the boundary) and `boundary_len` is
/// the length of the boundary itself — `2` for `\n\n` or `4` for `\r\n\r\n`.
///
/// `\n\n` and `\r\n\r\n` are searched independently; the earlier match wins so
/// a server that mixes line endings is still framed correctly.
pub(crate) fn find_event_boundary(buf: &BytesMut) -> Option<(usize, usize)> {
    let bytes = buf.as_ref();
    let lf_pos = bytes.windows(2).position(|w| w == b"\n\n");
    let crlf_pos = bytes.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf_pos, crlf_pos) {
        (Some(lf), Some(crlf)) => {
            if crlf <= lf {
                Some((crlf, 4))
            } else {
                Some((lf, 2))
            }
        }
        (Some(lf), None) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::http::SseStreamWithMeta;
    use std::fmt;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<Box<TestError>>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    #[test]
    fn detailed_error_chain_retains_nested_dns_cause_without_duplicates() {
        let error = TestError {
            message: "error sending request for url",
            source: Some(Box::new(TestError {
                message: "client error (Connect)",
                source: Some(Box::new(TestError {
                    message: "client error (Connect)",
                    source: Some(Box::new(TestError {
                        message: "dns error: failed to lookup address information",
                        source: None,
                    })),
                })),
            })),
        };

        assert_eq!(
            render_error_chain(&error),
            "error sending request for url: client error (Connect): \
             dns error: failed to lookup address information",
        );
    }

    #[test]
    fn detailed_connection_errors_are_opt_in_for_mobile() {
        assert!(!ReqwestHttp::new().detailed_connection_errors);
        let mobile = ReqwestHttp::new_with_detailed_connection_errors();
        assert!(mobile.detailed_connection_errors);
    }

    /// `ReqwestHttp::new()` must build BOTH clients — the default (follow) and
    /// the no-redirect override — so `request_no_follow` has its own
    /// `redirect::Policy::none()` client. A smoke test that construction
    /// succeeds and both `request` / `request_no_follow` are callable.
    #[tokio::test]
    async fn new_builds_both_clients_and_no_follow_is_callable() {
        use lingxi_core::types::HttpMethod;
        let transport = ReqwestHttp::new();
        // A connection failure (unroutable host) is fine — we only need the
        // no-follow client to exist and the method to dispatch through it. The
        // point of this test is construction + dispatch, not network behaviour.
        let req = HttpRequest {
            method: HttpMethod::Get,
            // RFC 5737 TEST-NET-1, reserved for documentation — never routable.
            url: "http://192.0.2.1:9/no-follow-smoke".to_string(),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: Some(std::time::Duration::from_millis(200)),
        };
        let result = transport.request_no_follow(req).await;
        // Must be a transport error (connection/timeout), NOT a panic and NOT a
        // success — proving the no-redirect client was built and used.
        assert!(
            result.is_err(),
            "unroutable host must error, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn empty_resolved_address_override_fails_closed() {
        use lingxi_core::types::HttpMethod;

        let request = HttpRequest {
            method: HttpMethod::Get,
            url: "https://example.com/image.png".to_string(),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };
        let resolved = Some(ResolvedAddressOverride {
            domain: "example.com".to_string(),
            addrs: Vec::new(),
        });
        let transport = ReqwestHttp::new();

        let request_error = transport
            .request_no_follow_with_resolved_addrs(request.clone(), resolved.clone())
            .await
            .expect_err("an empty DNS pin must never fall back to a fresh lookup");
        assert!(request_error.to_string().contains("at least one address"));

        let stream_error = match transport
            .stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(request, resolved)
            .await
        {
            Ok(_) => panic!("the streaming seam must fail closed too"),
            Err(error) => error,
        };
        assert!(stream_error.to_string().contains("at least one address"));
    }

    /// True no-follow behaviour: against an in-process axum server that returns a
    /// 301 with a `Location`, `request_no_follow` must surface the 3xx verbatim
    /// (status 301 + `Location` header intact) instead of following it. The
    /// default `request` (follow policy) would instead chase the redirect.
    ///
    /// Uses the same in-process axum harness pattern as the SSE tests, so no
    /// external server is needed.
    #[tokio::test]
    async fn request_no_follow_surfaces_3xx_without_following() {
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;
        use axum::Router;
        use lingxi_core::types::HttpMethod;
        use tokio::net::TcpListener;

        async fn redirector() -> Response {
            (
                axum::http::StatusCode::MOVED_PERMANENTLY,
                [("location", "https://other.example/landing")],
                "",
            )
                .into_response()
        }
        // A target the FOLLOW client would land on (200), to prove no-follow did
        // NOT chase the redirect.
        async fn target() -> &'static str {
            "FOLLOWED"
        }

        let app = Router::new()
            .route("/redir", get(redirector))
            .route("/landing", get(target));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: format!("http://{addr}/redir"),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };

        let resp = transport
            .request_no_follow(req)
            .await
            .expect("no-follow request must succeed (3xx is Ok, not Err)");
        assert_eq!(resp.status, 301, "3xx must be surfaced, not followed");
        let location = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map(|(_, v)| v.as_str());
        assert_eq!(
            location,
            Some("https://other.example/landing"),
            "Location header must be preserved for the caller's redirect policy"
        );
        assert_ne!(resp.body, "FOLLOWED", "must NOT have followed to /landing");
    }

    #[tokio::test]
    async fn request_with_resolved_addrs_pins_connection_and_preserves_host_header() {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::get;
        use axum::Router;
        use lingxi_core::host::ResolvedAddressOverride;
        use lingxi_core::types::HttpMethod;
        use std::net::SocketAddr;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        async fn handler(
            State(host_seen): State<Arc<Mutex<Option<String>>>>,
            headers: HeaderMap,
        ) -> &'static str {
            let host = headers
                .get("host")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            *host_seen.lock().unwrap() = host;
            "PINNED"
        }

        let host_seen = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/pinned", get(handler))
            .with_state(host_seen.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let logical_host = "hooks.example.invalid";
        let req = HttpRequest {
            method: HttpMethod::Get,
            url: format!("http://{logical_host}:{}/pinned", addr.port()),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: Some(std::time::Duration::from_secs(5)),
        };
        let resp = transport
            .request_with_resolved_addrs(
                req,
                Some(ResolvedAddressOverride {
                    domain: logical_host.to_string(),
                    addrs: vec![SocketAddr::new(addr.ip(), addr.port())],
                }),
            )
            .await
            .expect("resolved override should connect without DNS");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "PINNED");
        let expected_host = format!("{logical_host}:{}", addr.port());
        assert_eq!(
            host_seen.lock().unwrap().as_deref(),
            Some(expected_host.as_str()),
            "HTTP Host must remain the logical hostname even when the socket is pinned"
        );
    }

    /// `ReqwestHttp::stream_sse_with_meta` must capture the real status and headers
    /// (including `retry-after`) before the SSE event stream begins, and still
    /// deliver the SSE events normally.
    ///
    /// Uses the existing axum-based in-process test harness pattern.
    #[tokio::test]
    async fn stream_sse_with_meta_captures_status_and_headers() {
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use futures_util::StreamExt as _;
        use lingxi_core::types::HttpMethod;
        use tokio::net::TcpListener;

        async fn handler() -> Response {
            (
                axum::http::StatusCode::OK,
                [
                    ("content-type", "text/event-stream"),
                    ("retry-after", "7"),
                    ("x-custom", "meta"),
                ],
                "event: test\ndata: hello\n\n",
            )
                .into_response()
        }

        let app = Router::new().route("/stream", post(handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/stream"),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };

        let SseStreamWithMeta {
            status,
            headers,
            mut stream,
        } = transport.stream_sse_with_meta(req).await.unwrap();

        assert_eq!(status, 200, "status must be captured");
        let has_retry = headers.iter().any(|(k, v)| k == "retry-after" && v == "7");
        assert!(
            has_retry,
            "retry-after header must be captured; got: {headers:?}"
        );
        let has_custom = headers.iter().any(|(k, _)| k == "x-custom");
        assert!(has_custom, "x-custom header must be captured");

        // Events still arrive normally.
        let event = stream.next().await.expect("at least one event").unwrap();
        assert_eq!(event.event_type.as_deref(), Some("test"));
        assert_eq!(event.data, "hello");
    }

    /// `stream_sse_with_meta` on a 429 response must return `Ok` carrying the real
    /// status, real headers (including `retry-after`), and the JSON error body as
    /// a single frame.  This lets the bridge + codec see the full picture rather
    /// than an empty-header `Err(HttpError::Status)`.
    #[tokio::test]
    async fn stream_sse_with_meta_429_returns_ok_with_real_headers_and_body() {
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use futures_util::StreamExt as _;
        use lingxi_core::types::HttpMethod;
        use tokio::net::TcpListener;

        async fn handler_429() -> Response {
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [
                    ("content-type", "application/json"),
                    ("retry-after", "42"),
                    ("x-custom-error", "rate-limit-hit"),
                ],
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"Rate limited"}}"#,
            )
                .into_response()
        }

        let app = Router::new().route("/stream429", post(handler_429));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/stream429"),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };

        // Must be Ok — not Err — so the bridge receives real headers.
        let meta = transport
            .stream_sse_with_meta(req)
            .await
            .expect("429 must return Ok with real headers");

        assert_eq!(meta.status, 429);
        let has_retry = meta
            .headers
            .iter()
            .any(|(k, v)| k == "retry-after" && v == "42");
        assert!(
            has_retry,
            "retry-after header must be present; got: {:?}",
            meta.headers
        );
        let has_custom = meta.headers.iter().any(|(k, _)| k == "x-custom-error");
        assert!(has_custom, "x-custom-error header must be present");

        // The error body arrives as a single SSE data frame.
        let event = meta
            .stream
            .boxed()
            .next()
            .await
            .expect("one frame")
            .unwrap();
        assert!(
            event.data.contains("rate_limit_error"),
            "error body must be in frame data; got: {}",
            event.data
        );
    }

    /// `body_bytes` must be sent as the raw request body, verbatim (including
    /// non-UTF-8 bytes), and the string `body` must be ignored when bytes are
    /// set. Uses the existing axum-based in-process harness; the server
    /// captures the received body so binary fidelity is asserted end-to-end.
    #[tokio::test]
    async fn request_sends_body_bytes_verbatim_and_ignores_string_body() {
        use axum::extract::State;
        use axum::routing::post;
        use axum::Router;
        use lingxi_core::types::HttpMethod;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        type Captured = Arc<Mutex<Option<Vec<u8>>>>;

        async fn capture(State(seen): State<Captured>, body: axum::body::Bytes) -> &'static str {
            *seen.lock().unwrap() = Some(body.to_vec());
            "ok"
        }

        let seen: Captured = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/upload", post(capture))
            .with_state(Arc::clone(&seen));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let raw = vec![0x89u8, 0x50, 0x4E, 0x47, 0x00, 0xFF, 0x7F];
        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/upload"),
            headers: vec![("content-type".into(), "application/octet-stream".into())],
            // Deliberately set BOTH: the string body must be ignored.
            body: Some("IGNORED-JSON-BODY".to_string()),
            body_bytes: Some(raw.clone()),
            timeout: None,
        };

        let resp = transport.request(req).await.expect("request succeeds");
        assert_eq!(resp.status, 200);
        let captured = seen.lock().unwrap().take().expect("server saw a body");
        assert_eq!(captured, raw, "raw bytes must arrive verbatim");
    }

    /// Regression pin: without `body_bytes`, the string `body` is still sent
    /// unchanged.
    #[tokio::test]
    async fn request_without_body_bytes_sends_string_body() {
        use axum::extract::State;
        use axum::routing::post;
        use axum::Router;
        use lingxi_core::types::HttpMethod;
        use std::sync::Arc;
        use tokio::net::TcpListener;

        type Captured = Arc<Mutex<Option<Vec<u8>>>>;

        async fn capture(State(seen): State<Captured>, body: axum::body::Bytes) -> &'static str {
            *seen.lock().unwrap() = Some(body.to_vec());
            "ok"
        }

        let seen: Captured = Arc::new(Mutex::new(None));
        let app = Router::new()
            .route("/json", post(capture))
            .with_state(Arc::clone(&seen));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/json"),
            headers: vec![],
            body: Some(r#"{"model":"m"}"#.to_string()),
            body_bytes: None,
            timeout: None,
        };

        let resp = transport.request(req).await.expect("request succeeds");
        assert_eq!(resp.status, 200);
        let captured = seen.lock().unwrap().take().expect("server saw a body");
        assert_eq!(captured, br#"{"model":"m"}"#.to_vec());
    }

    /// Keepalive comment lines (`:`) are silently skipped; the event is still
    /// yielded. Ported from the deleted `api-client/src/sse.rs` test suite.
    #[test]
    fn ignore_keepalive_lines() {
        let raw = ": this is a comment\nevent: a\ndata: 1\n\n";
        let events = parse_sse_chunks(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type.as_deref(), Some("a"));
        assert_eq!(events[0].data, "1");
    }

    /// Multiple `data:` lines within one event are joined with `\n`.
    #[test]
    fn multi_line_data_concatenates() {
        let raw = "event: x\ndata: line1\ndata: line2\n\n";
        let events = parse_sse_chunks(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line1\nline2");
    }

    /// Two `\n\n`-separated events in a single chunk both decode.
    #[test]
    fn parse_multiple_events() {
        let raw = "event: a\ndata: 1\n\nevent: b\ndata: 2\n\n";
        let events = parse_sse_chunks(raw);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type.as_deref(), Some("a"));
        assert_eq!(events[1].event_type.as_deref(), Some("b"));
    }

    /// A keepalive-only block (no `data:` line) is silently dropped — it does
    /// not produce a spurious event. This is the "partial drop" behaviour from
    /// the original `api-client/src/sse.rs` test suite: blocks without data
    /// are ignored regardless of whether they contain comment or event-type
    /// lines.
    #[test]
    fn partial_event_dropped() {
        // A block that contains only a comment and an event-type line but no
        // data line must not yield an event.
        let raw = ": keepalive\nevent: noop\n\nevent: real\ndata: value\n\n";
        let events = parse_sse_chunks(raw);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type.as_deref(), Some("real"));
        assert_eq!(events[0].data, "value");
    }

    /// `stream_raw_bytes_with_meta` must capture real status + headers before
    /// the byte stream begins, and deliver body bytes incrementally.
    #[tokio::test]
    async fn stream_raw_bytes_with_meta_captures_status_and_headers() {
        use axum::body::Body;
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use futures_util::StreamExt as _;
        use lingxi_core::types::HttpMethod;
        use tokio::net::TcpListener;

        async fn binary_handler() -> Response {
            let body = Body::from(b"\x00\x01\x02\x03".as_ref());
            (
                axum::http::StatusCode::OK,
                [
                    ("content-type", "application/octet-stream"),
                    ("x-binary-meta", "yes"),
                ],
                body,
            )
                .into_response()
        }

        let app = Router::new().route("/binary", post(binary_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/binary"),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };

        let meta = transport
            .stream_raw_bytes_with_meta(req)
            .await
            .expect("binary stream must succeed");

        assert_eq!(meta.status, 200, "status must be captured");
        let has_meta = meta
            .headers
            .iter()
            .any(|(k, v)| k == "x-binary-meta" && v == "yes");
        assert!(
            has_meta,
            "x-binary-meta header must be captured; got: {:?}",
            meta.headers
        );

        // Collect all chunks.
        let mut all_bytes: Vec<u8> = Vec::new();
        let mut stream = meta.stream;
        while let Some(chunk) = stream.next().await {
            all_bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(
            all_bytes,
            &[0x00, 0x01, 0x02, 0x03],
            "binary body must arrive intact"
        );
    }

    /// `stream_raw_bytes_with_meta` on a ≥400 response must return `Ok` with
    /// real status, real headers, and body as a single chunk — mirroring the
    /// `stream_sse_with_meta` error-arm contract.
    #[tokio::test]
    async fn stream_raw_bytes_with_meta_error_returns_ok_with_headers() {
        use axum::response::{IntoResponse, Response};
        use axum::routing::post;
        use axum::Router;
        use futures_util::StreamExt as _;
        use lingxi_core::types::HttpMethod;
        use tokio::net::TcpListener;

        async fn error_handler() -> Response {
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [("content-type", "application/json"), ("retry-after", "30")],
                r#"{"error":"rate_limit"}"#,
            )
                .into_response()
        }

        let app = Router::new().route("/err", post(error_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let transport = ReqwestHttp::new();
        let req = HttpRequest {
            method: HttpMethod::Post,
            url: format!("http://{addr}/err"),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        };

        let meta = transport
            .stream_raw_bytes_with_meta(req)
            .await
            .expect("429 must return Ok — not Err — so bridge gets real headers");
        assert_eq!(meta.status, 429);
        let has_retry = meta
            .headers
            .iter()
            .any(|(k, v)| k == "retry-after" && v == "30");
        assert!(
            has_retry,
            "retry-after must be present; got: {:?}",
            meta.headers
        );

        // Body arrives as a single chunk.
        let chunk = meta
            .stream
            .boxed()
            .next()
            .await
            .expect("one chunk")
            .unwrap();
        assert!(
            chunk
                .windows(b"rate_limit".len())
                .any(|w| w == b"rate_limit"),
            "body chunk must contain error; got: {chunk:?}"
        );
    }
}
