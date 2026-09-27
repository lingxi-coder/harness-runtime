//! Host policy adapters around the shared client's execution lifecycle.
use crate::{LlmError, ProviderRequest, ProviderResponse, Transport};
use lingxi_llm_client::{self as sdk, protocol as wire};
use std::sync::Arc;

pub(crate) fn wire_error(error: LlmError) -> wire::LlmError {
    match error {
        LlmError::TransportTimeout { message } => wire::LlmError::TransportTimeout { message },
        LlmError::StreamInterrupted { message } => wire::LlmError::StreamInterrupted { message },
        LlmError::InvalidRequest { message } => wire::LlmError::InvalidRequest { message },
        LlmError::Authentication { message } => wire::LlmError::Authentication { message },
        LlmError::PermissionDenied { message } => wire::LlmError::PermissionDenied { message },
        LlmError::CostUnavailable { message } => wire::LlmError::CostUnavailable { message },
        LlmError::UnsupportedCapability { capability } => wire::LlmError::UnsupportedCapability {
            message: capability,
        },
        LlmError::Transport { message } => wire::LlmError::Transport { message },
        LlmError::TlsCert { message, .. } => wire::LlmError::TlsCert { message },
        other => wire::LlmError::Transport {
            message: other.to_string(),
        },
    }
}

pub(crate) struct HostTransport(pub Arc<dyn Transport>);
#[async_trait::async_trait]
impl sdk::Transport for HostTransport {
    async fn send(&self, request: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        self.0.send_raw(request).await
    }
    async fn connect_websocket(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<Box<dyn sdk::transport::WebSocketConnection>, wire::LlmError> {
        self.0.connect_raw(request).await
    }
}
struct PreparationOnly;
#[async_trait::async_trait]
impl sdk::Transport for PreparationOnly {
    async fn send(&self, _: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        Err(wire::LlmError::InvalidRequest {
            message: "attachment preparation requires a host transport".into(),
        })
    }
}

pub(crate) async fn prepare(
    mut profile: wire::ProviderProfile,
    route: &crate::ResolvedRoute,
    request: &crate::LlmRequest,
    transport: Option<Arc<dyn Transport>>,
    authenticator: Arc<dyn sdk::Authenticator>,
    mode: sdk::RequestMode,
) -> Result<(sdk::RequestDraft, ProviderRequest), LlmError> {
    profile.models.retain(|row| {
        row.display_model == route.display_model && row.request_model == route.request_model
    });
    // This exact connection was already selected by the application's policy.
    // Credentials are applied by the host after its final body/header policies.
    profile.auth = wire::AuthStrategy::Bearer;
    let transport: Arc<dyn sdk::Transport> = transport
        .map(|t| Arc::new(HostTransport(t)) as Arc<dyn sdk::Transport>)
        .unwrap_or_else(|| Arc::new(PreparationOnly));
    let region = profile
        .regions
        .first()
        .copied()
        .unwrap_or(wire::Region::International);
    let mut builder =
        sdk::LlmClientBuilder::with_transport(transport, &[profile.clone()]).with_region(region);
    builder.register_authenticator(wire::AuthStrategy::Bearer, authenticator);
    let client = builder.build().map_err(|e| LlmError::InvalidRequest {
        message: e.to_string(),
    })?;
    let mut input = crate::upstream::request(request, profile.protocol)?;
    input.model.clone_from(&route.display_model);
    let draft = client
        .prepare_draft_on(
            &profile.profile_name,
            &input,
            &sdk::RequestOptions::default(),
            mode,
        )
        .await
        .map_err(crate::upstream::error)?;
    let http = draft.request();
    let mut host = ProviderRequest::post_json(
        http.url.clone(),
        serde_json::from_slice(&http.body).map_err(|e| LlmError::InvalidRequest {
            message: e.to_string(),
        })?,
    );
    host.method.clone_from(&http.method);
    host.headers = http.headers.iter().cloned().collect();
    if profile.protocol == wire::ProtocolFamily::BedrockClaude {
        host.stream_framing = crate::StreamFraming::AwsEventStream;
    }
    host.json_string_overrides =
        crate::upstream::message_string_overrides(request, profile.protocol)?;
    Ok((draft, host))
}

pub(crate) async fn seal(
    mut draft: sdk::RequestDraft,
    request: &ProviderRequest,
) -> Result<sdk::PreparedCall, LlmError> {
    let wire = draft.request_mut();
    wire.url.clone_from(&request.url);
    wire.method.clone_from(&request.method);
    wire.headers = request
        .headers
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if let Some(bytes) = &request.body_bytes {
        wire.body = bytes.clone().into();
    } else {
        draft
            .set_json_body(request.body_json.clone(), &request.json_string_overrides)
            .map_err(crate::upstream::error)?;
    }
    draft.seal().await.map_err(crate::upstream::error)
}

pub(crate) fn response(raw: &sdk::HttpResponse) -> ProviderResponse {
    let headers = raw
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    ProviderResponse {
        status: raw.status,
        request_id: crate::transport_bridge::extract_response_request_id(&headers),
        headers,
        body_json: serde_json::from_slice(&raw.body).unwrap_or_default(),
    }
}

pub(crate) struct HostWebSocket {
    pub connection: Box<dyn crate::ResponsesWebSocketTransportSession>,
    pub request: ProviderRequest,
}
#[async_trait::async_trait]
impl sdk::transport::WebSocketConnection for HostWebSocket {
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
            .map_err(wire_error)?;
        let body = futures::stream::unfold(Some(response.frames), |frames| async move {
            let mut frames = frames?;
            match frames.next_frame().await {
                Ok(Some(frame)) => Some((Ok(frame.bytes.into()), Some(frames))),
                Ok(None) => None,
                Err(e) => Some((Err(wire_error(e)), None)),
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
        self.connection.close().await.map_err(wire_error)
    }
}

pub(crate) type HostFailure = Arc<std::sync::Mutex<Option<LlmError>>>;
pub(crate) fn restore_error(error: wire::LlmError, failure: &HostFailure) -> LlmError {
    failure
        .lock()
        .expect("host failure")
        .take()
        .unwrap_or_else(|| crate::upstream::error(error))
}
pub(crate) fn decode(
    collected: &sdk::CollectedResponse,
) -> Result<wire::CompletionResponse, LlmError> {
    collected.decode().map_err(|error| match error {
        wire::LlmError::ProviderInternal { message }
            if (200..300).contains(&collected.response().status) =>
        {
            LlmError::InvalidRequest { message }
        }
        other => crate::upstream::error(other),
    })
}
pub(crate) struct HostAuthenticator {
    pub client: crate::DefaultLlmClient,
    pub now: Option<std::time::SystemTime>,
    pub failure: HostFailure,
}
#[async_trait::async_trait]
impl sdk::Authenticator for HostAuthenticator {
    async fn apply(
        &self,
        request: &mut sdk::HttpRequest,
        profile: &wire::ProviderProfile,
        _: Option<&wire::Secret<String>>,
    ) -> Result<(), wire::LlmError> {
        self.client
            .authenticate_wire(
                &profile.profile_name,
                request,
                self.now.unwrap_or_else(std::time::SystemTime::now),
            )
            .await
            .map_err(|error| {
                *self.failure.lock().expect("host failure") = Some(error.clone());
                wire_error(error)
            })
    }
}

pub(crate) struct BorrowedHostTransport<'a>(pub &'a dyn Transport);
#[async_trait::async_trait]
impl sdk::Transport for BorrowedHostTransport<'_> {
    async fn send(&self, request: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
        self.0.send_raw(request).await
    }
    async fn connect_websocket(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<Box<dyn sdk::transport::WebSocketConnection>, wire::LlmError> {
        self.0.connect_raw(request).await
    }
}

pub(crate) fn first_byte_bound<'a, T: Send + 'a>(
    timeout: Option<std::time::Duration>,
    future: impl std::future::Future<Output = Result<T, LlmError>> + Send + 'a,
) -> crate::BoxFuture<'a, Result<T, LlmError>> {
    // Box before constructing the watchdog future: otherwise both timeout
    // branches embed the large provider-preparation state on the caller stack.
    let future = Box::pin(future);
    Box::pin(async move {
        match timeout {
            None => future.await,
            Some(timeout) => {
                let wall = std::time::SystemTime::now();
                tokio::time::timeout(timeout, future)
                    .await
                    .unwrap_or_else(|_| {
                        Err(crate::model::stream_watchdog::first_byte_abort_error(
                            timeout,
                            wall.elapsed().unwrap_or(timeout),
                        ))
                    })
            }
        }
    })
}

/// Wait for provider progress rather than arbitrary partial network chunks.
/// Never await again after new usage/inference has been decoded: the caller
/// must retain that observation synchronously before reading further.
pub(crate) async fn next_batch(
    stream: &mut sdk::ModelStream,
) -> Result<Option<sdk::StreamBatch>, LlmError> {
    let prior_usage = stream.usage_report();
    let prior_inference = stream.inference_report();
    while let Some(batch) = stream.next_batch().await {
        if batch.finished
            || batch.usage != prior_usage
            || batch.inference != prior_inference
            || batch
                .events
                .iter()
                .any(|event| !matches!(event, Ok(wire::StreamEvent::Inference { .. })))
        {
            return Ok(Some(batch));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    struct Heartbeats;
    #[async_trait::async_trait]
    impl sdk::Transport for Heartbeats {
        async fn send(&self, _: sdk::HttpRequest) -> Result<sdk::StreamResponse, wire::LlmError> {
            Ok(sdk::StreamResponse {
                status: 200,
                headers: vec![],
                body: futures::stream::unfold((), |()| async {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    Some((Ok(bytes::Bytes::from_static(b": keepalive\n\n")), ()))
                })
                .boxed(),
            })
        }
    }
    impl crate::Transport for Heartbeats {
        fn execute<'a>(
            &'a self,
            _: &'a crate::ProviderRequest,
        ) -> crate::BoxFuture<'a, Result<crate::ProviderResponse, LlmError>> {
            panic!("legacy transport")
        }
        fn open_stream<'a>(
            &'a self,
            _: &'a crate::ProviderRequest,
        ) -> crate::BoxFuture<'a, Result<crate::StreamingResponse, LlmError>> {
            panic!("legacy transport")
        }
        fn send_raw(
            &self,
            request: sdk::HttpRequest,
        ) -> crate::BoxFuture<'_, Result<sdk::StreamResponse, wire::LlmError>> {
            Box::pin(sdk::Transport::send(self, request))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn transport_heartbeats_do_not_reset_the_provider_progress_watchdog() {
        let profile:crate::ProviderProfile=serde_json::from_value(serde_json::json!({
            "profile_name":"test","provider_id":{"custom":{"name":"test"}},"protocol":"open_ai_chat","auth":"none","credential":{"type":"none"},"base_url":"https://example.test",
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
        let client = crate::DefaultLlmClient::from_config(crate::ClientConfig {
            providers: vec![profile],
        })
        .unwrap();
        let service = crate::ApiService::new(
            Arc::new(client),
            Arc::new(Heartbeats),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        )
        .with_stream_idle_timeout_override(Some(std::time::Duration::from_secs(5)));
        let mut request = crate::LlmRequest::new("model");
        request.max_tokens = Some(100);
        let mut stream = service.stream_request(request).await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(6), stream.next())
            .await
            .expect("host watchdog must fire despite heartbeats");
        assert!(matches!(
            result,
            Some(Err(LlmError::StreamInterrupted { .. }))
        ));
    }
}
