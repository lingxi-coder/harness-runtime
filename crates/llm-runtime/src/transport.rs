//! Transport contract between prepared provider requests and host HTTP stacks.
//!
//! `llm-runtime` never talks HTTP itself. Hosts implement [`Transport`] over
//! their HTTP client and the client orchestrates encode → send → decode
//! around it.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use crate::{LlmError, ProviderRequest, ProviderResponse, RawStreamFrame};

/// Boxed future alias keeping [`Transport`] object-safe without an
/// `async_trait` dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Asynchronous HTTP seam implemented by hosts.
///
/// Implementations own HTTP semantics only: connection, TLS, and timeout
/// failures map to [`LlmError::Transport`]. Provider payloads are never
/// interpreted here — non-2xx responses come back as data and the
/// orchestration layer routes them through the codec error taxonomy.
pub trait Transport: Send + Sync {
    /// Raw byte seam consumed by the shared executor. Platform transports
    /// override this directly; the default supports in-memory host fixtures.
    fn send_raw(
        &self,
        request: lingxi_llm_client::HttpRequest,
    ) -> BoxFuture<
        '_,
        Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>,
    > {
        Box::pin(async move {
            use futures::StreamExt;
            let body_json: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or_default();
            let is_stream = body_json
                .get("stream")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || request.url.contains("streamGenerateContent")
                || request.url.contains("invoke-with-response-stream");
            let mut host = ProviderRequest::post_json(request.url, body_json);
            host.method = request.method;
            host.headers = request.headers.into_iter().collect();
            if !request.body.is_empty()
                && serde_json::from_slice::<serde_json::Value>(&request.body).is_err()
            {
                host.body_bytes = Some(request.body.to_vec());
            }
            if !is_stream {
                let response = self
                    .execute(&host)
                    .await
                    .map_err(crate::execution::wire_error)?;
                return Ok(lingxi_llm_client::StreamResponse {
                    status: response.status,
                    headers: response.headers.into_iter().collect(),
                    body: futures::stream::once(async move {
                        Ok(serde_json::to_vec(&response.body_json)
                            .expect("JSON response")
                            .into())
                    })
                    .boxed(),
                });
            }
            let response = self
                .open_stream(&host)
                .await
                .map_err(crate::execution::wire_error)?;
            let sse = (200..300).contains(&response.status)
                && !host.url.contains("invoke-with-response-stream");
            let body = futures::stream::unfold(response.frames, move |mut frames| async move {
                match frames.next_frame().await {
                    Ok(Some(frame)) => {
                        let bytes = if sse {
                            let mut bytes = b"data: ".to_vec();
                            bytes.extend(frame.bytes);
                            bytes.extend(b"\n\n");
                            bytes
                        } else {
                            frame.bytes
                        };
                        Some((Ok(bytes.into()), frames))
                    }
                    Ok(None) => None,
                    Err(e) => Some((Err(crate::execution::wire_error(e)), frames)),
                }
            })
            .boxed();
            Ok(lingxi_llm_client::StreamResponse {
                status: response.status,
                headers: response.headers.into_iter().collect(),
                body,
            })
        })
    }

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
        Box::pin(async move {
            let mut host = ProviderRequest::post_json(request.url, serde_json::Value::Null);
            host.headers = request.headers.into_iter().collect();
            let connection = self
                .open_responses_websocket_session(&host)
                .await
                .map_err(crate::execution::wire_error)?;
            Ok(Box::new(crate::execution::HostWebSocket {
                connection,
                request: host,
            })
                as Box<
                    dyn lingxi_llm_client::transport::WebSocketConnection,
                >)
        })
    }

    /// Send a request and await the complete response.
    fn execute<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<ProviderResponse, LlmError>>;

    /// Open a streaming response.
    ///
    /// On success each frame is one SSE `data:` payload without the field
    /// prefix (see [`crate::SseFrameSplitter`] for byte-stream hosts). For
    /// non-2xx statuses the frames carry raw body bytes instead.
    fn open_stream<'a>(
        &'a self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>>;

    /// Open a reusable OpenAI Responses WebSocket session.
    ///
    /// The default keeps existing transports source-compatible and reports the
    /// capability as unsupported. Hosts that support WebSocket reuse override
    /// this and return a session that can send sequential `response.create`
    /// messages over one upgraded connection.
    fn open_responses_websocket_session<'a>(
        &'a self,
        _request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<Box<dyn ResponsesWebSocketTransportSession>, LlmError>> {
        Box::pin(async {
            Err(LlmError::InvalidRequest {
                message: "Responses WebSocket session transport is not supported".to_string(),
            })
        })
    }
}

/// Reusable transport session for OpenAI Responses WebSocket requests.
pub trait ResponsesWebSocketTransportSession: Send {
    /// Send one prepared provider request over the already-open connection.
    fn send<'a>(
        &'a mut self,
        request: &'a ProviderRequest,
    ) -> BoxFuture<'a, Result<StreamingResponse, LlmError>>;

    /// Close the reusable transport connection.
    fn close(&mut self) -> BoxFuture<'_, Result<(), LlmError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Status line and headers of a streaming response, plus its frame stream.
pub struct StreamingResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers; lowercase names expected.
    pub headers: BTreeMap<String, String>,
    /// Frame stream, drained by the orchestration layer.
    pub frames: Box<dyn FrameStream>,
}

/// Pull-based stream of raw frames.
pub trait FrameStream: Send {
    /// Next frame; `Ok(None)` is the normal end of stream.
    fn next_frame(&mut self) -> BoxFuture<'_, Result<Option<RawStreamFrame>, LlmError>>;
}
