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
        LlmError::FileUploadOutcomeUnknown { message } => {
            wire::LlmError::FileUploadOutcomeUnknown { message }
        }
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

/// Resolve Native's Anthropic ID policy from trusted Host routing facts and
/// the same process environment inputs that Native reads. Host auto-mode
/// labels and arbitrary endpoint URLs never assign a provider kind.
fn native_traceparent_for_protocol(protocol: wire::ProtocolFamily) -> Option<String> {
    if matches!(
        protocol,
        wire::ProtocolFamily::AnthropicMessages
            | wire::ProtocolFamily::BedrockClaude
            | wire::ProtocolFamily::FoundryClaude
            | wire::ProtocolFamily::VertexClaude
    ) {
        telemetry::otel::capture_current_trace_context().map(|context| context.traceparent)
    } else {
        None
    }
}

fn native_request_header_facts(
    protocol: wire::ProtocolFamily,
    provider_id: &crate::ProviderId,
    selected_url: &str,
) -> lingxi_llm_client::providers::response_headers::NativeAnthropicRequestHeaderFacts {
    use lingxi_llm_client::providers::response_headers::{
        NativeAnthropicProvider, NativeAnthropicRequestHeaderFacts,
    };

    let provider = match provider_id {
        crate::ProviderId::AnthropicFirstParty => NativeAnthropicProvider::FirstParty,
        crate::ProviderId::BedrockClaude => NativeAnthropicProvider::Bedrock,
        _ => NativeAnthropicProvider::Other,
    };
    NativeAnthropicRequestHeaderFacts::from_process_environment(protocol, provider, selected_url)
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

/// SDK clients are shared by effective immutable provider configuration and
/// transport. Model selection is request-local, so changing models does not
/// create another SDK client or connection pool.
#[derive(Default)]
pub(crate) struct ClientCache(std::sync::Mutex<Vec<CachedClient>>);
struct CachedClient {
    profile: wire::ProviderProfile,
    transport: Arc<dyn sdk::Transport>,
    client: sdk::LlmClient,
}
impl ClientCache {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    fn get(
        &self,
        profile: &wire::ProviderProfile,
        transport: Arc<dyn sdk::Transport>,
    ) -> Result<sdk::LlmClient, LlmError> {
        let mut entries = self.0.lock().expect("SDK client cache");
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.profile == *profile && Arc::ptr_eq(&entry.transport, &transport))
        {
            return Ok(entry.client.clone());
        }
        let region = profile
            .regions
            .first()
            .copied()
            .unwrap_or(wire::Region::International);
        let client =
            sdk::LlmClientBuilder::with_transport(transport.clone(), std::slice::from_ref(profile))
                .with_region(region)
                .build()
                .map_err(|error| LlmError::InvalidRequest {
                    message: error.to_string(),
                })?;
        entries.push(CachedClient {
            profile: profile.clone(),
            transport,
            client: client.clone(),
        });
        Ok(client)
    }
}
impl std::fmt::Debug for ClientCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientCache")
    }
}

pub(crate) async fn prepare(
    cache: &ClientCache,
    mut profile: wire::ProviderProfile,
    selected_auth: wire::AuthStrategy,
    route: &crate::ResolvedRoute,
    request: &crate::LlmRequest,
    transport: Option<Arc<dyn Transport>>,
    authenticator: Arc<dyn sdk::Authenticator>,
    preparation: (sdk::RequestMode, Option<wire::CapabilitySupport>),
) -> Result<(sdk::RequestDraft, ProviderRequest), LlmError> {
    let (mode, fast_capability) = preparation;
    // This exact connection was already selected by the application's policy.
    // Keep the route auth strategy for provider body policy, while credentials
    // are still applied by the host after its final body/header policies.
    let body_auth_strategy = selected_auth;
    profile.auth = if profile.protocol == wire::ProtocolFamily::GeminiInteractions {
        selected_auth
    } else {
        wire::AuthStrategy::Bearer
    };
    let anthropic_request_kind = if matches!(
        mode,
        sdk::RequestMode::Complete | sdk::RequestMode::Stream
    ) && profile.protocol == wire::ProtocolFamily::AnthropicMessages
        && request.execution.query_source.as_deref() == Some("hook_prompt")
    {
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::HookPrompt
    } else {
        request.execution.anthropic_request_kind
    };
    let transport: Arc<dyn sdk::Transport> = transport.unwrap_or_else(|| {
        static PREPARATION: std::sync::OnceLock<Arc<dyn sdk::Transport>> =
            std::sync::OnceLock::new();
        PREPARATION
            .get_or_init(|| Arc::new(PreparationOnly))
            .clone()
    });
    let client = cache.get(&profile, transport)?;
    let mut input = crate::upstream::request(request, profile.protocol)?;
    input.model.clone_from(&route.display_model);
    let mut draft = Box::pin(client.prepare_draft_on(
        &profile.profile_name,
        &input,
        &sdk::RequestOptions {
            anthropic_request_kind,
            message_text_utf16_overrides: request.execution.message_json_string_overrides.clone(),
            fast_capability,
            authenticator: Some(sdk::client::options::RequestAuthenticator(authenticator)),
            account_scope: request.execution.account_scope.clone(),
            file_account_scope: request.execution.file_account_scope.clone(),
            ..Default::default()
        },
        mode,
    ))
    .await
    .map_err(crate::upstream::error)?;
    draft
        .apply_request_body_auth_policy(body_auth_strategy)
        .map_err(crate::upstream::error)?;
    let http = draft.request();
    let mut host = ProviderRequest::post_json(
        http.url.clone(),
        draft.semantic_body_json().map_err(crate::upstream::error)?,
    );
    host.method.clone_from(&http.method);
    host.headers = http.headers.iter().cloned().collect();
    host.json_encoding =
        lingxi_llm_client::exact_json::JsonEncoding::for_protocol(profile.protocol);
    host.body_protocol = Some(profile.protocol);
    host.anthropic_request_kind = anthropic_request_kind;
    host.json_string_overrides = draft.message_json_string_overrides().clone();
    Ok((draft, host))
}

pub(crate) async fn seal(
    mut draft: sdk::RequestDraft,
    request: &ProviderRequest,
    native_provider_id: Option<&crate::ProviderId>,
) -> Result<sdk::PreparedCall, LlmError> {
    // Preparation exposes an authenticated byte snapshot. When host policy
    // edits the paired semantic body or exact strings, that old snapshot is
    // stale even if it remains in ProviderRequest::body_bytes. Re-encode and
    // reauthenticate those edits rather than restoring the old signed body.
    // A distinct caller-supplied raw image remains authoritative.
    let preserve_raw = request.body_bytes.as_ref().is_some_and(|bytes| {
        bytes.as_slice() != draft.request().body.as_ref()
            || (draft
                .semantic_body_json()
                .is_ok_and(|body| body == request.body_json)
                && draft.request_json_string_overrides() == &request.json_string_overrides)
    });
    {
        let wire = draft.request_mut();
        wire.url.clone_from(&request.url);
        wire.method.clone_from(&request.method);
        wire.headers = request
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
    }
    if let Some(bytes) = request.body_bytes.as_ref().filter(|_| preserve_raw) {
        draft
            .set_json_body(request.body_json.clone(), &request.json_string_overrides)
            .map_err(crate::upstream::error)?;
        draft.set_body_bytes(bytes.clone().into(), request.body_json.clone());
    } else {
        draft
            .set_json_body(request.body_json.clone(), &request.json_string_overrides)
            .map_err(crate::upstream::error)?;
    }
    if let Some(provider_id) = native_provider_id {
        let protocol = draft.profile().protocol;
        let facts = native_request_header_facts(protocol, provider_id, &request.url);
        draft
            .set_native_anthropic_request_header_facts(facts)
            .map_err(crate::upstream::error)?;
        if let Some(traceparent) = native_traceparent_for_protocol(protocol) {
            draft
                .set_native_traceparent(traceparent)
                .map_err(crate::upstream::error)?;
        }
    }
    Box::pin(draft.seal()).await.map_err(crate::upstream::error)
}

pub(crate) fn response(
    raw: &sdk::HttpResponse,
    protocol: wire::ProtocolFamily,
    provider_id: &str,
) -> ProviderResponse {
    let headers = raw
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .collect();
    let metadata = lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
        protocol,
        provider_id,
        &raw.headers,
        std::time::SystemTime::now(),
    );
    ProviderResponse {
        status: raw.status,
        request_id: metadata.request_id,
        headers,
        body_json: raw.payload_value(),
    }
}

/// Wire-facing provider identity derived from the resolved route. These are
/// protocol identities, independent of similarly named Host UI auto-mode labels.
pub(crate) fn response_provider_id(provider: &crate::ProviderId) -> &str {
    match provider {
        crate::ProviderId::AnthropicFirstParty => "anthropic",
        crate::ProviderId::OpenAI => "openai",
        crate::ProviderId::OpenAICompatible { name } | crate::ProviderId::Custom { name } => name,
        crate::ProviderId::Gemini => "gemini",
        crate::ProviderId::VertexGemini => "vertex-gemini",
        crate::ProviderId::VertexClaude => "vertex-claude",
        crate::ProviderId::BedrockClaude => "bedrock",
        crate::ProviderId::FoundryClaude => "foundry",
        crate::ProviderId::AzureOpenAI => "azure-openai",
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
pub(crate) fn decode(collected: &sdk::CollectedResponse) -> Result<wire::ChatResponse, LlmError> {
    let decoded = collected.decode().map_err(|error| match error {
        wire::LlmError::ProviderInternal { message }
            if (200..300).contains(&collected.response().status) =>
        {
            LlmError::InvalidRequest { message }
        }
        other => crate::upstream::error(other),
    })?;
    Ok(decoded)
}

/// Credential material belongs to one draft. The SDK authenticates a Responses
/// handshake before it seals the final body; both must use the same account.
/// This snapshot contains no account-change generation; cache overage fencing
/// is carried separately by the Host request context.
pub(crate) struct RequestCredentialSnapshot {
    pub profile: String,
    pub credential: tokio::sync::OnceCell<Option<crate::Credential>>,
}

pub(crate) struct HostAuthenticator {
    client: crate::ModelRuntime,
    now: Option<std::time::SystemTime>,
    failure: HostFailure,
    snapshot: Option<Arc<RequestCredentialSnapshot>>,
}

impl HostAuthenticator {
    pub(crate) async fn captured_credential(&self) -> Result<Option<crate::Credential>, LlmError> {
        let snapshot = self
            .snapshot
            .as_ref()
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "credential capture requires a request draft".into(),
            })?;
        self.client.capture_request_credential(snapshot).await
    }
    pub(crate) fn for_request(
        client: crate::ModelRuntime,
        profile: String,
        now: Option<std::time::SystemTime>,
        failure: HostFailure,
    ) -> Self {
        Self {
            client,
            now,
            failure,
            snapshot: Some(Arc::new(RequestCredentialSnapshot {
                profile,
                credential: tokio::sync::OnceCell::new(),
            })),
        }
    }

    pub(crate) fn with_request_credentials(
        mut self,
        shared: Option<&crate::RequestCredentials>,
    ) -> Self {
        if let (Some(shared), Some(snapshot)) = (shared, self.snapshot.as_ref()) {
            let mut snapshots = shared
                .snapshots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.snapshot = Some(
                snapshots
                    .entry(snapshot.profile.clone())
                    .or_insert_with(|| snapshot.clone())
                    .clone(),
            );
        }
        self
    }

    /// A file service may perform multiple operations and refresh credentials
    /// between them. It does not share a model draft's authentication snapshot.
    pub(crate) fn live(
        client: crate::ModelRuntime,
        now: Option<std::time::SystemTime>,
        failure: HostFailure,
    ) -> Self {
        Self {
            client,
            now,
            failure,
            snapshot: None,
        }
    }

    /// Authenticate one wire image at the requested time while retaining the
    /// credential material already captured by this draft's other operations.
    pub(crate) async fn authenticate_at(
        &self,
        profile_name: &str,
        request: &mut sdk::HttpRequest,
        now: std::time::SystemTime,
    ) -> Result<(), LlmError> {
        self.client
            .authenticate_wire(profile_name, request, now, self.snapshot.as_deref())
            .await?;
        if request.http1_header_layout == Some(sdk::Http1HeaderLayout::NativeFetch) {
            for (name, _) in &mut request.headers {
                let canonical =
                    sdk::providers::anthropic::request_policy::native_fetch_header_name(name);
                if canonical != name {
                    *name = canonical.to_owned();
                }
            }
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl sdk::Authenticator for HostAuthenticator {
    async fn apply(
        &self,
        request: &mut sdk::HttpRequest,
        profile: &wire::ProviderProfile,
        _: Option<&wire::Secret<String>>,
    ) -> Result<(), wire::LlmError> {
        self.authenticate_at(
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

#[cfg(test)]
#[path = "execution_auth_snapshot_tests.rs"]
mod auth_snapshot_tests;

pub(crate) fn non_stream_bound<'a, T: Send + 'a>(
    timeout: std::time::Duration,
    future: impl std::future::Future<Output = Result<T, LlmError>> + Send + 'a,
) -> crate::BoxFuture<'a, Result<T, LlmError>> {
    let future = Box::pin(future);
    Box::pin(async move {
        let expired = || LlmError::TransportTimeout {
            message: "Non-stream model request deadline exceeded".into(),
        };
        if timeout.is_zero() {
            return Err(expired());
        }
        tokio::time::timeout(timeout, future)
            .await
            .map_err(|_| expired())?
    })
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

    #[tokio::test]
    async fn native_traceparent_capture_uses_current_context_only_for_anthropic_routes() {
        let parent = telemetry::otel::SerializedTraceContext {
            traceparent: "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".into(),
            tracestate: None,
        };
        telemetry::otel::with_trace_context_future(Some(&parent), async {
            assert_eq!(
                native_traceparent_for_protocol(wire::ProtocolFamily::AnthropicMessages),
                Some(parent.traceparent.clone())
            );
            assert_eq!(
                native_traceparent_for_protocol(wire::ProtocolFamily::BedrockClaude),
                Some(parent.traceparent.clone())
            );
            assert_eq!(
                native_traceparent_for_protocol(wire::ProtocolFamily::OpenAiChat),
                None
            );
        })
        .await;
    }
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
    #[test]
    fn sdk_configuration_and_transport_snapshots_are_shared_and_isolated() {
        let mut profile: wire::ProviderProfile = serde_json::from_value(serde_json::json!({
            "provider_id":"test", "profile_name":"test", "protocol":"open_ai_chat", "auth":"none", "base_url":"https://old.example", "models":[
                {"display_model":"one", "request_model":"model-one", "billing_model":"one"},
                {"display_model":"two", "request_model":"model-two", "billing_model":"two"}
            ]
        })).unwrap();
        let cache = ClientCache::default();
        let transport: Arc<dyn sdk::Transport> = Arc::new(PreparationOnly);
        let old = cache.get(&profile, transport.clone()).unwrap();
        assert!(old.resolve("one").is_ok());
        assert!(old.resolve("two").is_ok());
        let _same = cache.get(&profile, transport.clone()).unwrap();
        assert_eq!(
            cache.len(),
            1,
            "model selection does not rebuild the SDK client"
        );
        profile.base_url = "https://new.example".into();
        let new = cache.get(&profile, transport).unwrap();
        assert_eq!(cache.len(), 2);
        assert_eq!(
            old.snapshot().profile("test").unwrap().base_url,
            "https://old.example"
        );
        assert_eq!(
            new.snapshot().profile("test").unwrap().base_url,
            "https://new.example"
        );
        let _other_transport = cache.get(&profile, Arc::new(PreparationOnly)).unwrap();
        assert_eq!(
            cache.len(),
            3,
            "network snapshots must not share execution identity"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn transport_heartbeats_do_not_reset_the_provider_progress_watchdog() {
        let profile:crate::ProviderProfile=serde_json::from_value(serde_json::json!({
            "profile_name":"test","provider_id":{"custom":{"name":"test"}},"protocol":"open_ai_chat","auth":"none","credential":{"type":"none"},"base_url":"https://example.test",
            "models":[{"display_model":"model","request_model":"model","billing_model":"model","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        })).unwrap();
        let client = crate::ModelRuntime::from_config(crate::ClientConfig {
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
        request.input.max_tokens = Some(100);
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

pub(crate) fn extract_response_request_id(
    protocol: wire::ProtocolFamily,
    provider_id: &str,
    headers: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    lingxi_llm_client::providers::response_headers::ProviderResponseHeaders::decode(
        protocol,
        provider_id,
        &headers,
        std::time::SystemTime::now(),
    )
    .request_id
}
