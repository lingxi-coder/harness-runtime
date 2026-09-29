//! Scripted fixtures for SDK Transport. Never enabled in production builds.
use crate::{BoxFuture, LlmError, ProviderRequest, ProviderResponse};
pub use async_trait::async_trait;
/// Raw streaming frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawStreamFrame {
    /// Raw frame bytes.
    pub bytes: Vec<u8>,
}

impl RawStreamFrame {
    /// Create a raw stream frame.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }
}

use lingxi_llm_client::{self as sdk, protocol as wire};
pub trait FixtureTransport: Send + Sync {
    /// Send a one-shot upload without buffering, replaying, or following
    /// redirects. Files, audio and remote skill services use this byte seam.
    /// Existing hosts fail explicitly until they provide streaming uploads.
    fn send_stream_raw(
        &self,
        _request: lingxi_llm_client::HttpStreamRequest,
    ) -> BoxFuture<
        '_,
        Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>,
    > {
        Box::pin(async {
            Err(
                lingxi_llm_client::protocol::LlmError::UnsupportedCapability {
                    message: "transport does not support streaming request bodies".into(),
                },
            )
        })
    }

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
            Ok(Box::new(ScriptedWebSocket {
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

struct ScriptedWebSocket {
    pub connection: Box<dyn ResponsesWebSocketTransportSession>,
    pub request: ProviderRequest,
}
#[async_trait::async_trait]
impl sdk::transport::WebSocketConnection for ScriptedWebSocket {
    async fn send(&mut self, payload: bytes::Bytes) -> Result<sdk::StreamResponse, wire::LlmError> {
        use futures::StreamExt;
        self.request.body_json =
            serde_json::from_slice(&payload).map_err(|e| wire::LlmError::InvalidRequest {
                message: e.to_string(),
            })?;
        self.request
            .body_json
            .as_object_mut()
            .map(|body| body.remove("type"));
        let response = self
            .connection
            .send(&self.request)
            .await
            .map_err(crate::execution::wire_error)?;
        let body = futures::stream::unfold(Some(response.frames), |frames| async move {
            let mut frames = frames?;
            match frames.next_frame().await {
                Ok(Some(frame)) => Some((Ok(frame.bytes.into()), Some(frames))),
                Ok(None) => None,
                Err(e) => Some((Err(crate::execution::wire_error(e)), None)),
            }
        })
        .boxed();
        Ok(sdk::StreamResponse {
            status: response.status,
            headers: response.headers.into_iter().collect(),
            body,
        })
    }
    async fn close(&mut self) -> Result<(), wire::LlmError> {
        self.connection
            .close()
            .await
            .map_err(crate::execution::wire_error)
    }
}

/// Implement the SDK contract for a local scripted fixture.
#[macro_export]
macro_rules! impl_fixture_transport {
    ($ty:ty) => {
        #[llm_runtime::test_support::async_trait]
        impl llm_runtime::Transport for $ty {
            async fn send(
                &self,
                request: llm_runtime::services::sdk::HttpRequest,
            ) -> Result<
                llm_runtime::services::sdk::StreamResponse,
                llm_runtime::services::sdk::protocol::LlmError,
            > {
                llm_runtime::test_support::FixtureTransport::send_raw(self, request).await
            }
            async fn send_stream(
                &self,
                request: llm_runtime::services::sdk::HttpStreamRequest,
            ) -> Result<
                llm_runtime::services::sdk::StreamResponse,
                llm_runtime::services::sdk::protocol::LlmError,
            > {
                llm_runtime::test_support::FixtureTransport::send_stream_raw(self, request).await
            }
            async fn connect_websocket(
                &self,
                request: llm_runtime::services::sdk::HttpRequest,
            ) -> Result<
                Box<dyn llm_runtime::services::sdk::transport::WebSocketConnection>,
                llm_runtime::services::sdk::protocol::LlmError,
            > {
                llm_runtime::test_support::FixtureTransport::connect_raw(self, request).await
            }
        }
    };
}

/// Adapt a scripted general HTTP fixture to SDK byte streams; never used by hosts.
pub async fn send_http_fixture(
    transport: &dyn platform_api::HttpTransport,
    request: sdk::HttpRequest,
) -> Result<sdk::StreamResponse, wire::LlmError> {
    use futures::StreamExt;
    let streaming = serde_json::from_slice::<serde_json::Value>(&request.body)
        .ok()
        .and_then(|v| v.get("stream").and_then(|v| v.as_bool()))
        .unwrap_or(false);
    let method = match request.method.as_str() {
        "GET" => protocol::HttpMethod::Get,
        "POST" => protocol::HttpMethod::Post,
        _ => panic!("unsupported fixture method"),
    };
    let request = protocol::HttpRequest {
        method,
        url: request.url,
        headers: request.headers,
        body: Some(String::from_utf8(request.body.to_vec()).expect("fixture JSON")),
        body_bytes: None,
        timeout: request.timeout,
    };
    let error = |e: platform_api::HttpError| wire::LlmError::Transport {
        message: e.to_string(),
    };
    if streaming {
        let stream = transport.stream_sse(request).await.map_err(error)?;
        Ok(sdk::StreamResponse {
            status: 200,
            headers: vec![],
            body: stream
                .map(move |e| {
                    e.map(|e| format!("data: {}\n\n", e.data).into())
                        .map_err(error)
                })
                .boxed(),
        })
    } else {
        let response = transport.request(request).await.map_err(error)?;
        Ok(sdk::StreamResponse {
            status: response.status,
            headers: response.headers,
            body: futures::stream::once(async move {
                Ok(if response.body_bytes.is_empty() {
                    response.body.into()
                } else {
                    response.body_bytes.into()
                })
            })
            .boxed(),
        })
    }
}
#[macro_export]
macro_rules! impl_http_fixture_transport {
    ($ty:ty) => {
        #[llm_runtime::test_support::async_trait]
        impl llm_runtime::Transport for $ty {
            async fn send(
                &self,
                request: llm_runtime::services::sdk::HttpRequest,
            ) -> Result<
                llm_runtime::services::sdk::StreamResponse,
                llm_runtime::services::sdk::protocol::LlmError,
            > {
                llm_runtime::test_support::send_http_fixture(self, request).await
            }
        }
    };
}

use std::collections::BTreeMap;
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
