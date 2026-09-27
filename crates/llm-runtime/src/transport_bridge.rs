//! Bridge from `platform_api::HttpTransport` to `llm_runtime::Transport`.
//!
//! One generic adapter serves every platform HTTP implementation
//! (`ReqwestHttp` on desktop, native transports on mobile).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::{
    BoxFuture, FrameStream, LlmError, ProviderRequest, ProviderResponse, ProviderStreamTransport,
    RawStreamFrame, StreamFraming, StreamingResponse,
};
use platform_api::http::{
    RawByteStream, RawByteStreamWithMeta, SseStream, SseStreamWithMeta, WebSocketConnection,
    WebSocketMessageStream, WebSocketMessageStreamWithMeta,
};
use platform_api::{HttpError, HttpTransport};
use protocol::{HttpMethod, HttpRequest, HttpResponse};

/// Adapter exposing a [`platform_api::HttpTransport`] as an [`llm_runtime::Transport`].
pub struct LlmTransportBridge<T> {
    inner: T,
}

impl<T> LlmTransportBridge<T> {
    /// Wrap a platform HTTP transport.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }

    /// Access the wrapped transport (used by hosts and tests).
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

fn to_http_request(request: &ProviderRequest) -> Result<HttpRequest, LlmError> {
    let method = match request.method.as_str() {
        "POST" => HttpMethod::Post,
        "GET" => HttpMethod::Get,
        other => {
            return Err(LlmError::InvalidRequest {
                message: format!("unsupported provider request method: {other}"),
            })
        }
    };
    // Raw bytes take precedence: when set, the JSON body is suppressed so a
    // transport can never double-send.
    let body = if request.body_bytes.is_some() {
        None
    } else {
        Some(
            String::from_utf8(request.wire_body_bytes()?).map_err(|err| {
                LlmError::InvalidRequest {
                    message: format!("provider request body was not valid UTF-8 JSON: {err}"),
                }
            })?,
        )
    };
    Ok(HttpRequest {
        method,
        url: request.url.clone(),
        headers: request
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
        body,
        body_bytes: request.body_bytes.clone(),
        // ProviderRequest carries no timeout field yet; deadline enforcement
        // lives above this seam in the retry layer.
        timeout: None,
    })
}

fn to_responses_websocket_request(request: &ProviderRequest) -> Result<HttpRequest, LlmError> {
    let mut http_request = to_http_request(request)?;
    if request.body_bytes.is_some() {
        return Err(LlmError::InvalidRequest {
            message: "OpenAI Responses WebSocket requests do not support body_bytes".to_string(),
        });
    }
    http_request.body = Some(responses_websocket_payload(&request.body_json)?);
    http_request.body_bytes = None;
    http_request.timeout = request
        .websocket_connect_timeout_ms
        .map(Duration::from_millis);
    Ok(http_request)
}

fn to_responses_websocket_handshake_request(
    request: &ProviderRequest,
) -> Result<HttpRequest, LlmError> {
    let mut http_request = to_http_request(request)?;
    if request.body_bytes.is_some() {
        return Err(LlmError::InvalidRequest {
            message: "OpenAI Responses WebSocket requests do not support body_bytes".to_string(),
        });
    }
    http_request.body = None;
    http_request.body_bytes = None;
    http_request.timeout = request
        .websocket_connect_timeout_ms
        .map(Duration::from_millis);
    Ok(http_request)
}

fn responses_websocket_payload(body: &serde_json::Value) -> Result<String, LlmError> {
    let serde_json::Value::Object(map) = body else {
        return Err(LlmError::InvalidRequest {
            message: "OpenAI Responses WebSocket request body must be a JSON object".to_string(),
        });
    };
    let mut payload = map.clone();
    payload.insert(
        "type".to_string(),
        serde_json::Value::String("response.create".to_string()),
    );
    serde_json::to_string(&serde_json::Value::Object(payload)).map_err(|err| {
        LlmError::InvalidRequest {
            message: format!("failed to encode OpenAI Responses WebSocket payload: {err}"),
        }
    })
}

fn lowercase_headers(headers: &[(String, String)]) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

/// Response header names (lowercased) that carry a provider's server-side
/// request id, in priority order. The transport lowercases all header names
/// before lookup (see [`lowercase_headers`]), so every candidate here is
/// lowercase. A provider that doesn't emit any of these simply yields `None`.
///
/// claude-code is single-provider and only ever reads Anthropic's `request-id`
/// (`x-request-id` fallback). LingXi is multi-provider, so the canonical id
/// header differs per provider — without this list Gemini / Bedrock / Azure
/// responses would report no request id at all (so the assistant transcript's
/// `requestId` would be blank for them).
pub const REQUEST_ID_HEADER_CANDIDATES: &[&str] = &[
    "request-id",        // Anthropic (req_…) — also the SDK's response._request_id
    "x-request-id",      // OpenAI / OpenAI-compatible (DeepSeek, GLM, Copilot), generic
    "apim-request-id",   // Azure OpenAI (APIM gateway)
    "x-ms-request-id",   // Azure
    "x-amzn-requestid",  // AWS Bedrock (`x-amzn-RequestId`)
    "x-amzn-request-id", // AWS Bedrock (alternate spelling)
    "x-goog-request-id", // Google / Vertex (best-effort)
];

/// Extract a provider's server-side request id from a (lowercased) response
/// header map, trying [`REQUEST_ID_HEADER_CANDIDATES`] in priority order.
///
/// Keys are expected to be lowercase (the transport normalizes them before this
/// runs); pass a lowercased map.
#[must_use]
pub fn extract_response_request_id(headers: &BTreeMap<String, String>) -> Option<String> {
    REQUEST_ID_HEADER_CANDIDATES
        .iter()
        .find_map(|name| headers.get(*name))
        .cloned()
}

fn request_id(headers: &BTreeMap<String, String>) -> Option<String> {
    extract_response_request_id(headers)
}

fn to_provider_response(response: &HttpResponse) -> ProviderResponse {
    let headers = lowercase_headers(&response.headers);
    let body_json = serde_json::from_str(&response.body).unwrap_or(serde_json::Value::Null);
    ProviderResponse {
        status: response.status,
        request_id: request_id(&headers),
        headers,
        body_json,
    }
}

fn status_error_response(status: u16, body: &str) -> ProviderResponse {
    ProviderResponse {
        status,
        headers: BTreeMap::new(),
        body_json: serde_json::from_str(body).unwrap_or(serde_json::Value::Null),
        request_id: None,
    }
}

fn map_http_error(error: &HttpError) -> LlmError {
    let message = error.to_string();
    // SSL/cert transport failures must fast-fail (never retried). Parity:
    // claude-code 2.1.201 `JF` walks the cause chain and, when it finds a code
    // in the `bBp` SSL set, marks the error terminal + attaches the `YLe` hint.
    // This port's HttpError is string-typed, so we scan the rendered error text
    // for a known SSL code token (see [`crate::ssl`]).
    if let Some(code) = crate::ssl::detect_ssl_code(&message) {
        return LlmError::tls_cert(code);
    }
    // `HttpError::Timeout` is already TYPED here — keep the distinction rather
    // than collapsing it and recovering it from text later. The oracle's `x2()`
    // yields a separate `ETIMEDOUT` code and `sir()` renders its own line.
    if matches!(error, HttpError::Timeout(_)) {
        return LlmError::TransportTimeout { message };
    }
    LlmError::Transport { message }
}

async fn open_http_stream<T: HttpTransport>(
    inner: &T,
    request: &ProviderRequest,
) -> Result<StreamingResponse, LlmError> {
    match request.stream_framing {
        StreamFraming::AwsEventStream => {
            // Raw binary path: pass byte chunks directly to the codec's
            // StreamDecoder (no SSE splitting). Used by Bedrock.
            let http_request = to_http_request(request)?;
            match inner.stream_raw_bytes_with_meta(http_request).await {
                Ok(RawByteStreamWithMeta {
                    status,
                    headers,
                    stream,
                }) => Ok(StreamingResponse {
                    status,
                    headers: lowercase_headers(&headers),
                    frames: Box::new(RawFrames { stream }),
                }),
                // Default transport impl (no override) may surface Err for ≥400.
                Err(HttpError::Status { status, body }) => Ok(StreamingResponse {
                    status,
                    headers: BTreeMap::new(),
                    frames: Box::new(BodyFrame {
                        body: Some(body.into_bytes()),
                    }),
                }),
                Err(error) => Err(map_http_error(&error)),
            }
        }
        StreamFraming::Sse => {
            let http_request = to_http_request(request)?;
            match inner.stream_sse_with_meta(http_request).await {
                Ok(SseStreamWithMeta {
                    status,
                    headers,
                    stream,
                }) => Ok(StreamingResponse {
                    status,
                    // Vec<(String,String)> → BTreeMap<String,String>; names are
                    // already lowercased by the SseStreamWithMeta contract.
                    headers: lowercase_headers(&headers),
                    frames: Box::new(SseFrames { stream }),
                }),
                // Error path: `reqwest`'s error arm has no headers at this
                // point (the response was consumed into the Status variant
                // before headers could be captured), so headers remain empty.
                Err(HttpError::Status { status, body }) => Ok(StreamingResponse {
                    status,
                    headers: BTreeMap::new(),
                    frames: Box::new(BodyFrame {
                        body: Some(body.into_bytes()),
                    }),
                }),
                Err(error) => Err(map_http_error(&error)),
            }
        }
    }
}

async fn open_responses_websocket_stream<T: HttpTransport>(
    inner: &T,
    request: &ProviderRequest,
) -> Result<StreamingResponse, LlmError> {
    let ws_request = to_responses_websocket_request(request)?;
    match inner.stream_websocket_messages_with_meta(ws_request).await {
        Ok(WebSocketMessageStreamWithMeta {
            status,
            headers,
            stream,
        }) => Ok(StreamingResponse {
            status,
            headers: lowercase_headers(&headers),
            frames: Box::new(WebSocketFrames { stream }),
        }),
        Err(HttpError::Status { status: 426, .. }) => open_http_stream(inner, request).await,
        Err(error) => Err(map_http_error(&error)),
    }
}

impl<T: HttpTransport + 'static> crate::Transport for LlmTransportBridge<T> {
    fn connect_raw(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> BoxFuture<
        '_,
        Result<
            Box<dyn lingxi_llm_client::transport::WebSocketConnection>,
            lingxi_llm_client::protocol::LlmError,
        >,
    > {
        Box::pin(lingxi_llm_client::Transport::connect_websocket(
            self, request,
        ))
    }

    fn send_raw(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> BoxFuture<
        '_,
        Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>,
    > {
        Box::pin(lingxi_llm_client::Transport::send(self, request))
    }

    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>> {
        Box::pin(async move {
            let http_request = to_http_request(request)?;
            match self.inner.request(http_request).await {
                Ok(response) => Ok(to_provider_response(&response)),
                // Some transports surface non-2xx as an error variant; keep
                // it data so llm-runtime's taxonomy does the classification.
                Err(HttpError::Status { status, body }) => Ok(status_error_response(status, &body)),
                Err(error) => Err(map_http_error(&error)),
            }
        })
    }

    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async move {
            match request.stream_transport {
                ProviderStreamTransport::Http => open_http_stream(&self.inner, request).await,
                ProviderStreamTransport::ResponsesWebSocket => {
                    open_responses_websocket_stream(&self.inner, request).await
                }
            }
        })
    }

    fn open_responses_websocket_session<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<Box<dyn crate::ResponsesWebSocketTransportSession>, LlmError>> {
        Box::pin(async move {
            let ws_request = to_responses_websocket_handshake_request(request)?;
            match self
                .inner
                .open_websocket_connection_with_meta(ws_request)
                .await
            {
                Ok(connection) => Ok(Box::new(BridgeResponsesWebSocketSession {
                    connection: connection.connection,
                })
                    as Box<dyn crate::ResponsesWebSocketTransportSession>),
                Err(error) => Err(map_http_error(&error)),
            }
        })
    }
}

struct BridgeResponsesWebSocketSession {
    connection: Box<dyn WebSocketConnection>,
}

impl crate::ResponsesWebSocketTransportSession for BridgeResponsesWebSocketSession {
    fn send<'a>(
        &'a mut self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>> {
        Box::pin(async move {
            let text = responses_websocket_payload(&request.body_json)?;
            match self.connection.send_text_with_meta(text).await {
                Ok(WebSocketMessageStreamWithMeta {
                    status,
                    headers,
                    stream,
                }) => Ok(StreamingResponse {
                    status,
                    headers: lowercase_headers(&headers),
                    frames: Box::new(WebSocketFrames { stream }),
                }),
                Err(error) => Err(map_http_error(&error)),
            }
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async move {
            self.connection
                .close()
                .await
                .map_err(|error| map_http_error(&error))
        })
    }
}

/// Raw byte stream wrapped as a [`FrameStream`] for the AWS event-stream path.
///
/// Each byte chunk from the transport is surfaced as one [`RawStreamFrame`].
/// The codec's [`StreamDecoder`] is responsible for reassembling binary frames
/// from the raw chunks (typically by feeding them to an [`EventStreamSplitter`]).
struct RawFrames {
    stream: RawByteStream,
}

impl FrameStream for RawFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        Box::pin(async move {
            use futures_util::StreamExt;
            match self.stream.next().await {
                Some(Ok(bytes)) => Ok(Some(RawStreamFrame::new(bytes))),
                Some(Err(error)) => Err(map_http_error(&error)),
                None => Ok(None),
            }
        })
    }
}

struct SseFrames {
    stream: SseStream,
}

impl FrameStream for SseFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        Box::pin(async move {
            use futures_util::StreamExt;
            match self.stream.next().await {
                Some(Ok(event)) => Ok(Some(RawStreamFrame::new(event.data.into_bytes()))),
                Some(Err(error)) => Err(map_http_error(&error)),
                None => Ok(None),
            }
        })
    }
}

struct WebSocketFrames {
    stream: WebSocketMessageStream,
}

impl FrameStream for WebSocketFrames {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        Box::pin(async move {
            use futures_util::StreamExt;
            match self.stream.next().await {
                Some(Ok(bytes)) => Ok(Some(RawStreamFrame::new(bytes))),
                Some(Err(error)) => Err(map_http_error(&error)),
                None => Ok(None),
            }
        })
    }
}

/// Error-status body delivered as a single frame for llm-runtime to drain.
struct BodyFrame {
    body: Option<Vec<u8>>,
}

impl FrameStream for BodyFrame {
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>> {
        let body = self.body.take();
        Box::pin(async move { Ok(body.map(RawStreamFrame::new)) })
    }
}

/// Wrap any host [`platform_api::HttpTransport`] as an [`crate::Transport`].
#[must_use]
pub fn from_http<T: platform_api::HttpTransport + 'static>(http: T) -> Arc<dyn crate::Transport> {
    Arc::new(LlmTransportBridge::new(http))
}

/// Expose the same platform TLS/proxy stack to the independent client. Raw
/// bytes cross this boundary; SSE and AWS framing remain owned by the client.
pub fn upstream_from_http<T: platform_api::HttpTransport + 'static>(
    http: T,
) -> Arc<dyn lingxi_llm_client::Transport> {
    Arc::new(LlmTransportBridge::new(http))
}

fn upstream_http_request(
    request: lingxi_llm_client::HttpRequest,
) -> Result<HttpRequest, lingxi_llm_client::protocol::LlmError> {
    let method =
        serde_json::from_value(serde_json::Value::String(request.method)).map_err(|_| {
            lingxi_llm_client::protocol::LlmError::InvalidRequest {
                message: "unsupported HTTP method".into(),
            }
        })?;
    Ok(HttpRequest {
        method,
        url: request.url,
        headers: request.headers,
        body: None,
        body_bytes: Some(request.body.to_vec()),
        timeout: request.timeout,
    })
}

fn upstream_http_error(error: HttpError) -> lingxi_llm_client::protocol::LlmError {
    use lingxi_llm_client::protocol::LlmError as E;
    match map_http_error(&error) {
        LlmError::TransportTimeout { message } => E::TransportTimeout { message },
        LlmError::TlsCert { message, .. } => E::TlsCert { message },
        _ => E::Transport {
            message: error.to_string(),
        },
    }
}

#[async_trait::async_trait]
impl<T: HttpTransport + 'static> lingxi_llm_client::Transport for LlmTransportBridge<T> {
    async fn send(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures_util::StreamExt;
        let response = match self
            .inner
            .stream_raw_bytes_with_meta_no_follow_with_resolved_addrs(
                upstream_http_request(request)?,
                None,
            )
            .await
        {
            Ok(response) => response,
            // Some injected/native transports expose HTTP failures as values
            // on their error channel. Keep the status and body observable by
            // the shared decoder rather than reclassifying them as transport.
            Err(HttpError::Status { status, body }) => {
                return Ok(lingxi_llm_client::StreamResponse {
                    status,
                    headers: Vec::new(),
                    body: futures_util::stream::once(async move { Ok(body.into_bytes().into()) })
                        .boxed(),
                })
            }
            Err(error) => return Err(upstream_http_error(error)),
        };
        Ok(lingxi_llm_client::StreamResponse {
            status: response.status,
            headers: response.headers,
            body: response
                .stream
                .map(|item| item.map(Into::into).map_err(upstream_http_error))
                .boxed(),
        })
    }
    async fn connect_websocket(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> Result<
        Box<dyn lingxi_llm_client::transport::WebSocketConnection>,
        lingxi_llm_client::protocol::LlmError,
    > {
        let connection = self
            .inner
            .open_websocket_connection_with_meta(upstream_http_request(request)?)
            .await
            .map_err(upstream_http_error)?;
        Ok(Box::new(UpstreamWebSocket {
            connection: connection.connection,
        }))
    }
}
struct UpstreamWebSocket {
    connection: Box<dyn WebSocketConnection>,
}
#[async_trait::async_trait]
impl lingxi_llm_client::transport::WebSocketConnection for UpstreamWebSocket {
    async fn send(
        &mut self,
        payload: bytes::Bytes,
    ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError> {
        use futures_util::StreamExt;
        let text = String::from_utf8(payload.to_vec()).map_err(|_| {
            lingxi_llm_client::protocol::LlmError::InvalidRequest {
                message: "WebSocket payload must be UTF-8".into(),
            }
        })?;
        let response = self
            .connection
            .send_text_with_meta(text)
            .await
            .map_err(upstream_http_error)?;
        Ok(lingxi_llm_client::StreamResponse {
            status: if response.status == 101 {
                200
            } else {
                response.status
            },
            headers: response.headers,
            body: response
                .stream
                .map(|item| item.map(Into::into).map_err(upstream_http_error))
                .boxed(),
        })
    }
    async fn close(&mut self) -> Result<(), lingxi_llm_client::protocol::LlmError> {
        self.connection.close().await.map_err(upstream_http_error)
    }
}

#[cfg(test)]
mod request_id_tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn extracts_each_providers_id_header() {
        // Anthropic
        assert_eq!(
            extract_response_request_id(&map(&[("request-id", "req_abc")])),
            Some("req_abc".to_string())
        );
        // OpenAI / OpenAI-compatible
        assert_eq!(
            extract_response_request_id(&map(&[("x-request-id", "oai-1")])),
            Some("oai-1".to_string())
        );
        // Azure OpenAI
        assert_eq!(
            extract_response_request_id(&map(&[("apim-request-id", "az-1")])),
            Some("az-1".to_string())
        );
        // Bedrock
        assert_eq!(
            extract_response_request_id(&map(&[("x-amzn-requestid", "aws-1")])),
            Some("aws-1".to_string())
        );
        // Google / Vertex
        assert_eq!(
            extract_response_request_id(&map(&[("x-goog-request-id", "g-1")])),
            Some("g-1".to_string())
        );
    }

    #[test]
    fn prefers_canonical_anthropic_over_generic() {
        let h = map(&[("x-request-id", "generic"), ("request-id", "canonical")]);
        assert_eq!(
            extract_response_request_id(&h),
            Some("canonical".to_string())
        );
    }

    #[test]
    fn none_when_no_known_header() {
        assert_eq!(
            extract_response_request_id(&map(&[("content-type", "application/json")])),
            None
        );
    }
}

#[cfg(test)]
mod map_http_error_tests {
    use super::*;

    #[test]
    fn ssl_cert_connection_error_maps_to_tls_cert() {
        // A connection failure whose text carries a `bBp` SSL code → terminal
        // TlsCert (fast-fail), not a retryable Transport error.
        let err = map_http_error(&HttpError::Connection(
            "certificate has expired: CERT_HAS_EXPIRED".to_string(),
        ));
        match err {
            LlmError::TlsCert { code, message } => {
                assert_eq!(code, "CERT_HAS_EXPIRED");
                assert!(message.contains("NODE_EXTRA_CA_CERTS"));
            }
            other => panic!("expected TlsCert, got {other:?}"),
        }
    }

    #[test]
    fn non_ssl_connection_error_stays_transport() {
        // A plain connection error (no SSL code) remains a retryable Transport.
        let err = map_http_error(&HttpError::Connection(
            "connection refused (ECONNREFUSED)".to_string(),
        ));
        assert!(
            matches!(err, LlmError::Transport { .. }),
            "non-SSL transport failure must stay Transport, got {err:?}"
        );
    }
}
