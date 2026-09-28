//! HTTP transport abstraction. Provider-neutral; the api-client crate uses
//! this trait so it never imports reqwest directly.

use async_trait::async_trait;
use futures_core::stream::Stream;
use protocol::{HttpRequest, HttpResponse, SseEvent};
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use thiserror::Error;

/// A pinned, boxed stream of SSE events (each item is `Result<SseEvent, HttpError>`).
///
/// Returned by [`HttpTransport::stream_sse`]; the caller drives it to completion.
pub type SseStream = Pin<Box<dyn Stream<Item = Result<SseEvent, HttpError>> + Send>>;

/// A pinned, boxed stream of raw response-body byte chunks (each item is
/// `Result<Vec<u8>, HttpError>`).
///
/// Returned by [`HttpTransport::stream_raw_bytes`] for binary response protocols
/// (e.g. the AWS event-stream used by Bedrock streaming); the caller frames and
/// interprets the bytes.
pub type RawByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>> + Send>>;

/// One-shot streaming upload. Transports must consume the body incrementally,
/// set its declared content length, and never replay it or follow redirects.
/// A single matching Content-Length is normalized; conflicting/duplicate lengths
/// and Transfer-Encoding are rejected before the body is consumed.
pub struct HttpStreamRequest {
    /// HTTP method.
    pub method: protocol::HttpMethod,
    /// Destination URL.
    pub url: String,
    /// Request headers; an optional Content-Length must match `content_length`.
    pub headers: Vec<(String, String)>,
    /// Raw upload chunks. Dropping the request cancels the source stream.
    pub body: RawByteStream,
    /// Exact byte count of the body.
    pub content_length: u64,
    /// Optional deadline covering the upload and response body.
    pub timeout: Option<std::time::Duration>,
}

impl std::fmt::Debug for HttpStreamRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpStreamRequest")
            .field("method", &self.method)
            .field("url", &"<redacted>")
            .field("content_length", &self.content_length)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// A pinned, boxed stream of WebSocket text-message payloads as raw bytes.
///
/// Returned by [`HttpTransport::stream_websocket_messages_with_meta`] for
/// provider protocols that deliver one JSON event per WebSocket message.
pub type WebSocketMessageStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>> + Send>>;

/// Whether a socket address is eligible for arbitrary-URL Monitor egress.
pub fn is_public_monitor_address(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            let o = ip.octets();
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_multicast()
                || o[0] == 0
                || o[0] >= 240
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19)))
        }
        std::net::IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return is_public_monitor_address(std::net::IpAddr::V4(v4));
            }
            let first = ip.segments()[0];
            !ip.is_loopback()
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && first & 0xfe00 != 0xfc00
                && first & 0xffc0 != 0xfe80
                && first & 0xe000 == 0x2000
                && !(first == 0x2001 && ip.segments()[1] == 0xdb8)
        }
    }
}

/// A tunneled native socket, before the destination's TLS/WebSocket handshake.
pub trait MonitorSocketIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> MonitorSocketIo for T {}

/// Optional host adapter for an existing enterprise/environment proxy connector.
#[async_trait]
pub trait MonitorWebSocketProxy: Send + Sync {
    /// Return a tunnel, or None when the host's proxy/NO_PROXY policy says direct.
    async fn connect_proxy(
        &self,
        host: &str,
        port: u16,
        secure: bool,
    ) -> std::io::Result<Option<Box<dyn MonitorSocketIo>>>;
}

/// Frames delivered by a passive Monitor WebSocket (no request frame is sent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorWebSocketFrame {
    /// UTF-8 text event.
    Text(String),
    /// Binary frame length; bytes never enter a model prompt.
    Binary(usize),
    /// Frame exceeded the one-megabyte monitor bound; the socket is closed.
    Oversized(usize),
    /// Peer close code and reason.
    Closed(u16, String),
}

/// Bounded passive socket event channel. Dropping it closes the socket.
pub type MonitorWebSocketReceiver =
    tokio::sync::mpsc::Receiver<Result<MonitorWebSocketFrame, HttpError>>;

/// SSE stream together with the HTTP response metadata that preceded it.
///
/// Returned by [`HttpTransport::stream_sse_with_meta`]. The status and headers
/// are captured from the response line/headers before any SSE data arrives, so
/// callers can inspect them immediately — for example, to honour a
/// `retry-after` header on a connect-phase 429.
///
/// # Note on the default implementation
///
/// The default [`HttpTransport::stream_sse_with_meta`] loses metadata: it
/// delegates to [`HttpTransport::stream_sse`] which surfaces neither status nor
/// headers.  Real transports (e.g. `ReqwestHttp` in `platform-common`)
/// override this method to capture the metadata before handing the
/// byte-stream to the SSE decoder.  Use the override wherever accurate
/// retry-after / rate-limit tracking matters.
pub struct SseStreamWithMeta {
    /// HTTP status of the streaming response (e.g. 200, 429).
    pub status: u16,
    /// Response headers, lowercased names (e.g. `"retry-after"`).
    pub headers: Vec<(String, String)>,
    /// The SSE event stream; drive to completion as usual.
    pub stream: SseStream,
}

/// Raw byte stream together with the HTTP response metadata that preceded it.
///
/// Returned by [`HttpTransport::stream_raw_bytes_with_meta`]. The status and
/// headers are captured from the response line/headers before any body bytes
/// arrive — exactly as [`SseStreamWithMeta`] does for SSE — so callers can
/// inspect rate-limit and other headers immediately.
///
/// Used by the bridge's AWS event-stream path: the bridge calls this method
/// instead of [`HttpTransport::stream_raw_bytes`] so it can forward real
/// response headers (e.g. `retry-after`) upstream even for binary-framed
/// responses.
///
/// # Note on the default implementation
///
/// The default [`HttpTransport::stream_raw_bytes_with_meta`] delegates to
/// [`HttpTransport::stream_raw_bytes`] with a synthetic `status: 200` and
/// empty headers — it loses metadata that a real transport would surface.
/// Override in production transports (e.g. `ReqwestHttp`) to capture the
/// metadata before streaming body bytes.
pub struct RawByteStreamWithMeta {
    /// HTTP status of the streaming response (e.g. 200, 429).
    pub status: u16,
    /// Response headers, lowercased names (e.g. `"retry-after"`).
    pub headers: Vec<(String, String)>,
    /// The raw byte chunk stream; drive to completion and interpret framing.
    pub stream: RawByteStream,
}

/// WebSocket message stream together with the HTTP upgrade response metadata.
///
/// The status and headers are captured from the successful WebSocket upgrade
/// response before the first provider event message arrives.
pub struct WebSocketMessageStreamWithMeta {
    /// HTTP status of the WebSocket upgrade response (normally 101).
    pub status: u16,
    /// Response headers, lowercased names (e.g. `"openai-model"`).
    pub headers: Vec<(String, String)>,
    /// Provider event messages as raw bytes.
    pub stream: WebSocketMessageStream,
}

/// Reusable WebSocket connection plus the HTTP upgrade metadata that opened it.
pub struct WebSocketConnectionWithMeta {
    /// HTTP status of the WebSocket upgrade response (normally 101).
    pub status: u16,
    /// Response headers, lowercased names (e.g. `"openai-model"`).
    pub headers: Vec<(String, String)>,
    /// Open connection. Callers may send sequential provider request messages
    /// and drain the returned stream to a terminal event before sending again.
    pub connection: Box<dyn WebSocketConnection>,
}

/// Vetted DNS override for a request that must connect to pre-resolved
/// addresses instead of performing a fresh lookup inside the HTTP transport.
///
/// The original request URL stays intact so HTTP Host and TLS SNI still use the
/// logical domain while the socket connection is pinned to these addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAddressOverride {
    /// Lowercase logical hostname from the request URL.
    pub domain: String,
    /// Pre-vetted socket addresses the transport must use for the connection.
    pub addrs: Vec<SocketAddr>,
}

/// Reusable WebSocket connection abstraction for provider protocols that send
/// one request text frame followed by one JSON-event stream.
#[async_trait]
pub trait WebSocketConnection: Send {
    /// Send one text request message and return the provider event stream for
    /// that request.
    async fn send_text_with_meta(
        &mut self,
        text: String,
    ) -> Result<WebSocketMessageStreamWithMeta, HttpError>;

    /// Close the underlying WebSocket connection.
    ///
    /// Default transports may no-op because the connection is owned by the
    /// concrete implementation and will close on drop.
    async fn close(&mut self) -> Result<(), HttpError> {
        Ok(())
    }
}

/// A `Stream` that yields a single chunk then ends. Backs the default
/// [`HttpTransport::stream_raw_bytes`] (buffer-the-body) impl without pulling a
/// stream-combinator dependency into this leaf crate.
struct OnceBytes(Option<Result<Vec<u8>, HttpError>>);

impl Stream for OnceBytes {
    type Item = Result<Vec<u8>, HttpError>;
    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.0.take())
    }
}

/// Asynchronous HTTP transport used by `lingxi-api-client`.
///
/// Implementations live in platform crates and wrap a concrete HTTP client
/// (e.g. `reqwest` on posix, native APIs on iOS). Engine code never imports a
/// concrete HTTP library directly — see D17.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Send a request and await the full response.
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError>;

    /// Upload a one-shot body and expose raw response bytes with metadata.
    /// The default fails without polling the body; it never silently buffers
    /// uploads. Production transports override this for files/audio/skills.
    async fn send_stream(
        &self,
        _req: HttpStreamRequest,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        Err(HttpError::InvalidRequest(
            "streaming request bodies are not supported by this transport".into(),
        ))
    }

    /// Send a request using pre-vetted DNS answers for the logical hostname.
    ///
    /// Transports that can pin the connection must override this method. The
    /// default fails closed whenever an override is supplied so a wrapper or
    /// platform backend cannot silently discard the SSRF guard's DNS result.
    async fn request_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<HttpResponse, HttpError> {
        if resolved.is_some() {
            return Err(HttpError::InvalidRequest(
                "HTTP transport does not support pre-resolved address pinning".to_string(),
            ));
        }
        self.request(req).await
    }

    /// Send a request WITHOUT following redirects: a 3xx is surfaced to the caller as
    /// `Ok(status=3xx)` with its `Location` header intact (so callers like WebFetch can
    /// apply their own permitted-redirect policy — mirrors claude-code's maxRedirects:0).
    ///
    /// The default delegates to [`Self::request`] (which MAY auto-follow, transport-dependent),
    /// preserving existing behavior for mocks/tests. Production reqwest transports OVERRIDE
    /// this with a no-redirect client.
    async fn request_no_follow(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.request(req).await
    }

    /// Send a request WITHOUT following redirects while also pinning the
    /// connection to pre-vetted DNS answers for the logical hostname.
    ///
    /// The default implementation preserves the fail-closed resolved-address
    /// contract and otherwise delegates to [`Self::request_no_follow`].
    async fn request_no_follow_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<HttpResponse, HttpError> {
        if resolved.is_some() {
            return Err(HttpError::InvalidRequest(
                "HTTP transport does not support pre-resolved address pinning".to_string(),
            ));
        }
        self.request_no_follow(req).await
    }

    /// Open an SSE stream. Caller drives the stream to completion.
    async fn stream_sse(&self, req: HttpRequest) -> Result<SseStream, HttpError>;

    /// Open an SSE stream, also capturing the HTTP status and response headers.
    ///
    /// The default implementation wraps [`Self::stream_sse`] with a synthetic
    /// `status: 200` and empty headers — it loses metadata that the platform
    /// transport would otherwise surface.  Override in production transports so
    /// connect-phase rate-limit headers (`retry-after`, etc.) are preserved.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] on connection failure or a non-2xx status that the
    /// transport surfaces as an error (behaviour depends on the implementation).
    async fn stream_sse_with_meta(&self, req: HttpRequest) -> Result<SseStreamWithMeta, HttpError> {
        Ok(SseStreamWithMeta {
            status: 200,
            headers: Vec::new(),
            stream: self.stream_sse(req).await?,
        })
    }

    /// Open a raw byte stream for a binary response protocol (e.g. the AWS
    /// event-stream used by Bedrock streaming). The caller drives it to
    /// completion and interprets the framing.
    ///
    /// The default impl buffers the full response via [`Self::request`] and
    /// yields it as a single chunk — correct for decoders that frame a complete
    /// buffer, and for test transports. Production transports override this for
    /// true incremental byte streaming. Additive (default-provided) so existing
    /// implementations compile unchanged.
    async fn stream_raw_bytes(&self, req: HttpRequest) -> Result<RawByteStream, HttpError> {
        let resp = self.request(req).await?;
        if resp.status >= 400 {
            return Err(HttpError::Status {
                status: resp.status,
                body: resp.body,
            });
        }
        // Prefer the raw wire bytes when the transport captured them; fall back
        // to re-encoding the (lossy) String body for producers that set only
        // `body` (test mocks, non-transport producers).
        let bytes = if resp.body_bytes.is_empty() {
            resp.body.into_bytes()
        } else {
            resp.body_bytes
        };
        Ok(Box::pin(OnceBytes(Some(Ok(bytes)))))
    }

    /// Open a raw byte stream, also capturing the HTTP status and response
    /// headers before any body bytes arrive.
    ///
    /// The default implementation wraps [`Self::stream_raw_bytes`] with a
    /// synthetic `status: 200` and empty headers — it loses metadata that the
    /// platform transport would otherwise surface.  Override in production
    /// transports so connect-phase rate-limit headers (`retry-after`, etc.)
    /// are preserved for binary-framed streaming responses.
    ///
    /// # ≥400 error-arm behaviour
    ///
    /// Production overrides (e.g. `ReqwestHttp`) return `Ok` with the real
    /// status and headers even for error responses, mirroring the
    /// [`Self::stream_sse_with_meta`] contract.  The default falls back to
    /// [`Self::stream_raw_bytes`], which surfaces `Err(HttpError::Status)` for
    /// non-2xx — callers of the default lose headers on error paths.
    ///
    /// # Errors
    ///
    /// Returns [`HttpError`] on connection failure. Non-2xx handling depends
    /// on the implementation (see above).
    async fn stream_raw_bytes_with_meta(
        &self,
        req: HttpRequest,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        Ok(RawByteStreamWithMeta {
            status: 200,
            headers: Vec::new(),
            stream: self.stream_raw_bytes(req).await?,
        })
    }

    /// Open a raw byte stream without following redirects while optionally
    /// pinning the connection to pre-resolved addresses.
    ///
    /// The default implementation preserves the fail-closed resolved-address
    /// contract, delegates to [`Self::request_no_follow_with_resolved_addrs`],
    /// and exposes the full buffered body as a single chunk.
    async fn stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
        &self,
        req: HttpRequest,
        resolved: Option<ResolvedAddressOverride>,
    ) -> Result<RawByteStreamWithMeta, HttpError> {
        let resp = self
            .request_no_follow_with_resolved_addrs(req, resolved)
            .await?;
        let status = resp.status;
        let headers = resp.headers;
        let bytes = if resp.body_bytes.is_empty() {
            resp.body.into_bytes()
        } else {
            resp.body_bytes
        };
        let stream: RawByteStream = Box::pin(OnceBytes(Some(Ok::<Vec<u8>, HttpError>(bytes))));
        Ok(RawByteStreamWithMeta {
            status,
            headers,
            stream,
        })
    }

    /// Validate Monitor endpoint DNS before any task is registered.
    /// The connection path must validate again and pin its vetted addresses.
    async fn preflight_monitor_websocket(&self, _url: &str) -> Result<(), HttpError> {
        Err(HttpError::InvalidRequest(
            "passive websocket monitoring is unsupported".into(),
        ))
    }

    /// Open a passive, public-network-only WebSocket with no initial frame.
    /// Native implementations validate DNS and pin the vetted address; they
    /// must not follow redirects or connect to private/metadata addresses.
    async fn monitor_websocket(
        &self,
        _url: String,
        _protocols: Vec<String>,
    ) -> Result<MonitorWebSocketReceiver, HttpError> {
        Err(HttpError::InvalidRequest(
            "passive websocket monitoring is unsupported".into(),
        ))
    }

    /// Open a provider WebSocket stream, send the request body as the first
    /// text message, and return provider event text messages as raw bytes.
    ///
    /// Default implementations do not support WebSocket streaming. Production
    /// transports that can perform WebSocket handshakes override this method.
    async fn stream_websocket_messages_with_meta(
        &self,
        _req: HttpRequest,
    ) -> Result<WebSocketMessageStreamWithMeta, HttpError> {
        Err(HttpError::InvalidRequest(
            "websocket streaming is not supported by this transport".to_string(),
        ))
    }

    /// Open a reusable provider WebSocket connection without sending a prompt
    /// payload. Default transports do not support WebSocket reuse.
    async fn open_websocket_connection_with_meta(
        &self,
        _req: HttpRequest,
    ) -> Result<WebSocketConnectionWithMeta, HttpError> {
        Err(HttpError::InvalidRequest(
            "websocket connection reuse is not supported by this transport".to_string(),
        ))
    }
}

/// Failure modes for [`HttpTransport`] calls.
#[derive(Debug, Clone, Error)]
pub enum HttpError {
    /// Request did not complete within the timeout.
    #[error("request timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Transport-level connection failure (DNS, TCP, TLS).
    #[error("connection failed: {0}")]
    Connection(String),

    /// Non-2xx HTTP response.
    #[error("non-success HTTP status {status}: {body}")]
    Status {
        /// Numeric HTTP status code returned by the server.
        status: u16,
        /// Response body verbatim (truncated at the implementation's discretion).
        body: String,
    },

    /// Request could not be assembled or violated client preconditions.
    #[error("invalid request: {0}")]
    InvalidRequest(String),

    /// Response could not be decoded or violated expected shape.
    #[error("invalid response: {0}")]
    InvalidResponse(String),

    /// Operation was cancelled before completion.
    #[error("cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use protocol::HttpMethod;

    #[test]
    fn http_error_display_includes_status() {
        let e = HttpError::Status {
            status: 429,
            body: "rate limited".into(),
        };
        assert!(format!("{e}").contains("429"));
    }

    /// Minimal transport whose `request` returns a fixed body, to exercise the
    /// default `stream_raw_bytes` (buffer-the-body) impl.
    struct OneShot;

    #[async_trait]
    impl HttpTransport for OneShot {
        async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
            Ok(HttpResponse {
                status: 200,
                headers: vec![],
                body: "hello".to_string(),
                body_bytes: Vec::new(),
            })
        }
        async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
            Err(HttpError::InvalidRequest(
                "sse not supported in this mock".to_string(),
            ))
        }
    }

    fn get_req() -> HttpRequest {
        HttpRequest {
            method: HttpMethod::Get,
            url: "http://x.local".to_string(),
            headers: vec![],
            body: None,
            body_bytes: None,
            timeout: None,
        }
    }

    #[tokio::test]
    async fn pre_resolved_address_default_fails_closed() {
        let err = OneShot
            .request_with_resolved_addrs(
                get_req(),
                Some(ResolvedAddressOverride {
                    domain: "x.local".to_string(),
                    addrs: vec!["93.184.216.34:80".parse().unwrap()],
                }),
            )
            .await
            .expect_err("transport must not silently discard a vetted DNS override");
        assert!(
            matches!(err, HttpError::InvalidRequest(ref message) if message.contains("pre-resolved"))
        );
    }

    /// The default `stream_sse_with_meta` wraps `stream_sse` with status 200
    /// and empty headers — it must compile and forward events correctly.
    #[tokio::test]
    async fn default_stream_sse_with_meta_uses_status_200_empty_headers() {
        struct SseOnce;

        #[async_trait]
        impl HttpTransport for SseOnce {
            async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
                Err(HttpError::InvalidRequest("not used".to_string()))
            }
            async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
                // Return an empty stream.
                use futures_core::stream::Stream;
                use std::pin::Pin;
                use std::task::{Context, Poll};
                struct Empty;
                impl Stream for Empty {
                    type Item = Result<protocol::SseEvent, HttpError>;
                    fn poll_next(
                        self: Pin<&mut Self>,
                        _cx: &mut Context<'_>,
                    ) -> Poll<Option<Self::Item>> {
                        Poll::Ready(None)
                    }
                }
                Ok(Box::pin(Empty))
            }
        }

        let t = SseOnce;
        let meta = t
            .stream_sse_with_meta(get_req())
            .await
            .expect("default must succeed");
        assert_eq!(meta.status, 200, "default status must be 200");
        assert!(meta.headers.is_empty(), "default headers must be empty");
    }

    /// The default `request_no_follow` delegates to `request`: a transport that
    /// returns a 3xx from `request` (and does NOT override `request_no_follow`)
    /// must surface the SAME 3xx response — status and `Location` header intact —
    /// from `request_no_follow`. This pins the "mocks/tests keep existing
    /// behavior" half of the contract (only production reqwest transports
    /// override with a no-redirect client).
    #[tokio::test]
    async fn default_request_no_follow_delegates_to_request() {
        struct Redirector;

        #[async_trait]
        impl HttpTransport for Redirector {
            async fn request(&self, _req: HttpRequest) -> Result<HttpResponse, HttpError> {
                Ok(HttpResponse {
                    status: 301,
                    headers: vec![("location".to_string(), "https://other.example/".to_string())],
                    body: String::new(),
                    body_bytes: Vec::new(),
                })
            }
            async fn stream_sse(&self, _req: HttpRequest) -> Result<SseStream, HttpError> {
                Err(HttpError::InvalidRequest("not used".to_string()))
            }
        }

        let t = Redirector;
        let resp = t
            .request_no_follow(get_req())
            .await
            .expect("default request_no_follow must succeed");
        assert_eq!(
            resp.status, 301,
            "default must surface the 3xx from request"
        );
        let location = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("location"))
            .map(|(_, v)| v.as_str());
        assert_eq!(
            location,
            Some("https://other.example/"),
            "Location header must be preserved"
        );
    }

    #[tokio::test]
    async fn default_stream_raw_bytes_yields_full_body_once() {
        let t = OneShot;
        let mut s = t.stream_raw_bytes(get_req()).await.unwrap();
        let first = std::future::poll_fn(|cx| s.as_mut().poll_next(cx)).await;
        assert_eq!(first.unwrap().unwrap(), b"hello".to_vec());
        // stream ends after the single chunk
        let second = std::future::poll_fn(|cx| s.as_mut().poll_next(cx)).await;
        assert!(second.is_none());
    }

    /// The default `stream_raw_bytes_with_meta` wraps `stream_raw_bytes` with a
    /// synthetic status 200 and empty headers — it must compile, forward the body
    /// chunk, and report the synthetic metadata.
    #[tokio::test]
    async fn default_stream_raw_bytes_with_meta_uses_status_200_empty_headers() {
        let t = OneShot;
        let meta = t
            .stream_raw_bytes_with_meta(get_req())
            .await
            .expect("default must succeed");
        assert_eq!(meta.status, 200, "default status must be 200");
        assert!(meta.headers.is_empty(), "default headers must be empty");

        let mut s = meta.stream;
        let first = std::future::poll_fn(|cx| s.as_mut().poll_next(cx)).await;
        assert_eq!(
            first.unwrap().unwrap(),
            b"hello".to_vec(),
            "body chunk must be forwarded"
        );
        let second = std::future::poll_fn(|cx| s.as_mut().poll_next(cx)).await;
        assert!(second.is_none(), "stream must end after single chunk");
    }
}
