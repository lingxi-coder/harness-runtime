use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sigv4;
use crate::{
    validate_capabilities, ApiKeyAuthenticator, AuthStrategy, Authenticator, BearerAuthenticator,
    ChatGptAuthenticator, ClientConfig, CopilotAuthenticator, Credential, CredentialConfig,
    CredentialProvider, CredentialScope, EnvCredentialProvider, LlmError, LlmEvent, LlmRequest,
    LlmResponse, MediaRoute, ModelListing, ModelRegistry, ProtocolFamily, ProviderId,
    ProviderRequest, ProviderStreamTransport, Route, Transport,
};

/// Anthropic Messages API version sent by codecs this client constructs.
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Clone)]
pub struct DefaultLlmClient {
    cache: Arc<crate::execution::ClientCache>,
    registry: Arc<ModelRegistry>,
    routes: Arc<BTreeMap<String, RouteEntry>>,
    credentials: Option<Arc<dyn CredentialProvider>>,
}

#[derive(Debug)]
pub(crate) struct RouteEntry {
    profile: lingxi_llm_client::protocol::ProviderProfile,
    protocol: ProtocolFamily,
    provider_id: ProviderId,
    auth: AuthStrategy,
    credential: CredentialConfig,
    base_url: String,
    signing: Option<crate::SigningConfig>,
    supports_websockets: bool,
    websocket_connect_timeout_ms: Option<u64>,
}

struct ServiceAuthenticator(DefaultLlmClient);

#[async_trait::async_trait]
impl lingxi_llm_client::Authenticator for ServiceAuthenticator {
    async fn apply(
        &self,
        request: &mut lingxi_llm_client::HttpRequest,
        profile: &lingxi_llm_client::protocol::ProviderProfile,
        credential: Option<&lingxi_llm_client::protocol::Secret<String>>,
    ) -> Result<(), lingxi_llm_client::protocol::LlmError> {
        use lingxi_llm_client::protocol::AuthStrategy as WireAuth;

        if let Some(credential) = credential {
            // Resource references inherit the operation's account scope. Never
            // authenticate them with a different account from the host store.
            let authenticator: &dyn lingxi_llm_client::Authenticator = match profile.auth {
                WireAuth::ApiKey => &lingxi_llm_client::ApiKeyAuthenticator,
                WireAuth::Bearer | WireAuth::OAuthBearer | WireAuth::GcpToken => {
                    &lingxi_llm_client::BearerAuthenticator
                }
                WireAuth::CopilotBearer
                | WireAuth::ChatGptOAuth
                | WireAuth::AwsSigV4
                | WireAuth::AzureToken
                | WireAuth::None => {
                    return Err(lingxi_llm_client::protocol::LlmError::UnsupportedCapability {
                        message: format!(
                            "explicit service credentials are unsupported for {:?} authentication on profile {:?}",
                            profile.auth, profile.profile_name
                        ),
                    });
                }
            };
            authenticator
                .apply(request, profile, Some(credential))
                .await?;

            return Ok(());
        }
        self.0
            .authenticate_wire(&profile.profile_name, request, std::time::SystemTime::now())
            .await
            .map_err(crate::execution::wire_error)
    }
}

/// Polling knobs for [`DefaultLlmClient::wait_for_file_active`]. The defaults
/// (2s interval, 300s budget) are this crate's own convenience choice — there
/// is no claude-code/codex counterpart to pin against; tune per call site.
#[derive(Debug, Clone, Copy)]
pub struct FileActivationPoll {
    /// Delay between consecutive status requests.
    pub interval: Duration,
    /// Total budget before giving up with [`LlmError::Transport`].
    pub max_wait: Duration,
}

impl Default for FileActivationPoll {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(2),
            max_wait: Duration::from_secs(300),
        }
    }
}

/// Turn-scoped OpenAI Responses WebSocket state.
///
/// A session owns one reusable transport connection plus enough Responses state
/// to send a compatible follow-up as `previous_response_id` + input delta.
pub struct ResponsesWebSocketSession {
    shared: lingxi_llm_client::ResponsesSession,
}
pub use lingxi_llm_client::websocket::ResponsesWebSocketRequestSnapshot;
impl Default for ResponsesWebSocketSession {
    fn default() -> Self {
        Self::new()
    }
}
impl ResponsesWebSocketSession {
    pub fn new() -> Self {
        Self {
            shared: lingxi_llm_client::ResponsesSession::new(),
        }
    }
    pub fn fallback_to_http(&self) -> bool {
        self.shared.fallback_to_http()
    }
    pub fn last_response_id(&self) -> Option<String> {
        self.shared.last_response_id()
    }
    pub fn last_added_response_items(&self) -> Vec<serde_json::Value> {
        self.shared.last_added_response_items()
    }
    pub fn last_response_from_prewarm(&self) -> bool {
        self.shared.last_response_from_prewarm()
    }
    pub fn last_request_snapshot(&self) -> ResponsesWebSocketRequestSnapshot {
        self.shared.last_request_snapshot()
    }
    pub async fn close(&mut self) -> Result<(), LlmError> {
        self.shared.close().await.map_err(crate::upstream::error)
    }
}
impl std::fmt::Debug for ResponsesWebSocketSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponsesWebSocketSession")
            .field("fallback", &self.fallback_to_http())
            .finish_non_exhaustive()
    }
}

impl DefaultLlmClient {
    pub(crate) fn search_profile(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<&lingxi_llm_client::protocol::ProviderProfile> {
        let route = self.registry.resolve_in(model, profile).ok()?;
        self.routes
            .get(&route.profile_name)
            .map(|entry| &entry.profile)
    }

    pub(crate) fn attempt_price_bounds(
        &self,
        route: &crate::ResolvedRoute,
    ) -> Result<Option<crate::AttemptPriceBounds>, LlmError> {
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        crate::attempt_pricing::bounds(&entry.profile, route)
    }

    pub(crate) fn profile_pricing_config(&self, profile: &str) -> Option<crate::PricingConfig> {
        self.registry.profile_pricing_config(profile)
    }

    pub fn from_config(config: ClientConfig) -> Result<Self, LlmError> {
        let registry = ModelRegistry::from_config(config.clone())?;
        let mut routes = BTreeMap::new();

        for provider in config.providers {
            if routes.contains_key(&provider.profile_name) {
                return Err(LlmError::InvalidRequest {
                    message: format!("duplicate provider profile_name: {}", provider.profile_name),
                });
            }
            validate_provider_profile(&provider)?;
            let base_url = provider.base_url.clone();
            let signing = provider.signing.clone();
            routes.insert(
                provider.profile_name.clone(),
                RouteEntry {
                    profile: crate::upstream::profile(&provider)?,
                    protocol: provider.protocol,
                    provider_id: provider.provider_id,
                    auth: provider.auth,
                    credential: provider.credential,
                    base_url,
                    signing,
                    supports_websockets: provider.supports_websockets,
                    websocket_connect_timeout_ms: provider.websocket_connect_timeout_ms,
                },
            );
        }

        Ok(Self {
            cache: Arc::new(Default::default()),
            registry: Arc::new(registry),
            routes: Arc::new(routes),
            credentials: None,
        })
    }

    /// Attach a host-managed credential store consulted for
    /// `CredentialConfig::Static` and `CredentialConfig::HostManaged` ids.
    #[must_use]
    pub fn with_credential_provider(mut self, provider: Arc<dyn CredentialProvider>) -> Self {
        self.credentials = Some(provider);
        self
    }

    /// Reuse configured provider profiles and the host transport for independent
    /// SDK services. Create this once per configuration/transport lifetime.
    ///
    /// SDK operations using the registered authenticator honor an explicit
    /// `RequestOptions::credential` for API-key, bearer, OAuth-bearer and GCP-token
    /// profiles. Other authenticated strategies reject explicit overrides rather
    /// than silently using a different account. Without an explicit credential,
    /// these operations reuse this client's host-managed authentication.
    /// Audio and remote Skills may use their dedicated credential handling.
    /// Supply resource account scopes on every operation; independent services
    /// do not participate in [`crate::ApiService`] turn accounting.
    pub fn provider_services(
        &self,
        region: lingxi_llm_client::protocol::Region,
        transport: Arc<dyn Transport>,
    ) -> Result<crate::services::ProviderServices, lingxi_llm_client::BuildError> {
        let profiles = self
            .routes
            .values()
            .map(|entry| {
                let mut profile = entry.profile.clone();
                // Conversation preparation disables SDK auth until host sealing.
                // Independent SDK calls must select the host authenticator, and
                // file services still need the original explicit-key strategy.
                use lingxi_llm_client::protocol::AuthStrategy as WireAuth;
                profile.auth = match entry.auth {
                    AuthStrategy::ApiKey => WireAuth::ApiKey,
                    AuthStrategy::Bearer => WireAuth::Bearer,
                    AuthStrategy::OAuthBearer => WireAuth::OAuthBearer,
                    AuthStrategy::CopilotBearer => WireAuth::CopilotBearer,
                    AuthStrategy::ChatGptOAuth => WireAuth::ChatGptOAuth,
                    AuthStrategy::AwsSigV4 => WireAuth::AwsSigV4,
                    AuthStrategy::GcpToken => WireAuth::GcpToken,
                    AuthStrategy::AzureToken => WireAuth::AzureToken,
                    AuthStrategy::None => WireAuth::None,
                };
                profile
            })
            .collect::<Vec<_>>();
        let authenticator: Arc<dyn lingxi_llm_client::Authenticator> =
            Arc::new(ServiceAuthenticator(self.clone()));
        crate::services::ProviderServices::with_configured_transport(
            &profiles,
            region,
            transport,
            |builder| {
                for profile in &profiles {
                    builder.register_authenticator(profile.auth, authenticator.clone());
                }
            },
        )
    }

    #[must_use]
    pub fn available_models(&self) -> Vec<ModelListing> {
        self.registry.available_models()
    }

    /// Resolve the selected main route plus an optional same-profile vision delegate.
    pub fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        self.registry.resolve_media_route_in(model, profile)
    }

    /// Resolve, validate, encode, and authenticate a request.
    ///
    /// The returned provider request may carry credentials in its headers;
    /// redact with [`crate::Redactor`] before logging.
    pub async fn prepare(&self, request: &LlmRequest) -> Result<PreparedLlmCall, LlmError> {
        use std::time::SystemTime;
        self.prepare_at(request, SystemTime::now()).await
    }

    /// Clock-injectable variant of [`prepare`] used by tests to pin the `SigV4`
    /// timestamp and produce a deterministic `Authorization` header.
    ///
    /// Production code uses [`prepare`] which passes `SystemTime::now()`.
    pub async fn prepare_at(
        &self,
        request: &LlmRequest,
        now: std::time::SystemTime,
    ) -> Result<PreparedLlmCall, LlmError> {
        self.prepare_internal(request, now, None, true).await
    }

    pub(crate) async fn prepare_on(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
    ) -> Result<PreparedLlmCall, LlmError> {
        self.prepare_internal(
            request,
            std::time::SystemTime::now(),
            Some(transport),
            false,
        )
        .await
    }

    // Preparation owns SDK request, attachment and authentication state. Box it
    // once here so all execution entry points retain a bounded stack footprint.
    fn prepare_internal<'a>(
        &'a self,
        request: &'a LlmRequest,
        now: std::time::SystemTime,
        transport: Option<Arc<dyn Transport>>,
        authenticate: bool,
    ) -> crate::BoxFuture<'a, Result<PreparedLlmCall, LlmError>> {
        Box::pin(self.prepare_internal_impl(request, now, transport, authenticate))
    }

    async fn prepare_internal_impl(
        &self,
        request: &LlmRequest,
        now: std::time::SystemTime,
        transport: Option<Arc<dyn Transport>>,
        authenticate: bool,
    ) -> Result<PreparedLlmCall, LlmError> {
        let resolved_route = self
            .registry
            .resolve_in(&request.model, request.profile.as_deref())?;

        // Soft-degrade `reasoning` rather than hard-failing: it is a best-effort
        // enhancement driven by the SESSION thinking config, so switching to a
        // model that doesn't advertise reasoning (e.g. an OpenRouter free model
        // like `qwen/qwen3-coder:free`) must silently drop it — not break the turn
        // with "unsupported capability: reasoning". This covers BOTH the top-level
        // `request.reasoning` field AND any `Reasoning`/`RedactedThinking` blocks
        // left in message HISTORY from an earlier thinking-capable model —
        // `validate_capabilities` rejects those blocks independently, so a
        // mid-conversation downgrade (not just a first turn) must strip them from
        // the request that gets validated AND encoded. (streaming / tools /
        // structured_output stay hard errors — they cannot be dropped safely.)
        let is_reasoning_block = |b: &crate::ContentBlock| {
            matches!(
                b,
                crate::ContentBlock::Reasoning { .. }
                    | crate::ContentBlock::RedactedThinking { .. }
            )
        };
        let needs_reasoning_degrade = !resolved_route.capabilities.reasoning
            && (request.reasoning.is_some()
                || request
                    .messages
                    .iter()
                    .any(|m| m.content.iter().any(is_reasoning_block)));

        let entry = self
            .routes
            .get(&resolved_route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        // Fast mode is a first-party Anthropic request property, not a generic
        // Anthropic-wire feature. Resolve the route before encoding and strip
        // it for custom compatible endpoints, cloud transports, and models
        // without the canonical capability. Doing this before authentication
        // is essential for signed Bedrock/Vertex requests.
        let needs_speed_degrade =
            request.speed.is_some() && !route_allows_first_party_fast_mode(&resolved_route, entry);

        let mut owned: Option<LlmRequest> = None;
        if needs_reasoning_degrade || needs_speed_degrade {
            let mut r = request.clone();
            if needs_reasoning_degrade {
                r.reasoning = None;
                for m in &mut r.messages {
                    m.content.retain(|b| !is_reasoning_block(b));
                }
            }
            if needs_speed_degrade {
                r.speed = None;
            }
            owned = Some(r);
        }
        let request = owned.as_ref().unwrap_or(request);

        validate_capabilities(request, resolved_route.capabilities)?;

        // Per-model wire override: GitHub Copilot serves its GPT-5.x / codex
        // models ONLY via the Responses endpoint, though the provider declares a
        // single OpenAiChat protocol. When the override fires we swap in a
        // Responses codec bound to the SAME host (`…/responses`), which the
        // Copilot bearer already authorizes; `effective_protocol` + `codec` then
        // flow through encode, the websocket gate, and the returned Route.
        let effective_protocol = copilot_responses_override(
            &resolved_route.profile_name,
            &entry.protocol,
            &resolved_route.request_model,
        )
        .unwrap_or_else(|| entry.protocol.clone());
        let mut routed_request;
        let encoding_request = if request.model == resolved_route.request_model {
            request
        } else {
            routed_request = request.clone();
            routed_request
                .model
                .clone_from(&resolved_route.request_model);
            &routed_request
        };
        let mut profile = entry.profile.clone();
        profile.protocol = crate::upstream::family(&effective_protocol);
        let host_failure = Arc::new(Mutex::new(None));
        let (wire_draft, mut provider_request) = crate::execution::prepare(
            &self.cache,
            profile,
            &resolved_route,
            encoding_request,
            transport,
            Arc::new(crate::execution::HostAuthenticator {
                client: self.clone(),
                now: authenticate.then_some(now),
                failure: host_failure.clone(),
            }),
            if request.stream {
                lingxi_llm_client::RequestMode::Stream
            } else {
                lingxi_llm_client::RequestMode::Complete
            },
        )
        .await
        .map_err(|error| {
            host_failure
                .lock()
                .expect("host failure")
                .take()
                .unwrap_or(error)
        })?;
        if authenticate {
            provider_request = self
                .authenticate_at(entry, &resolved_route.profile_name, provider_request, now)
                .await?;
        }
        if request.stream
            && entry.supports_websockets
            && matches!(effective_protocol, ProtocolFamily::OpenAiResponses)
        {
            provider_request.stream_transport = ProviderStreamTransport::ResponsesWebSocket;
            provider_request.websocket_connect_timeout_ms = entry.websocket_connect_timeout_ms;
        }

        Ok(PreparedLlmCall {
            host_failure,
            wire_draft: Some(wire_draft),
            wire_call: None,
            registered_attempt: request.model_attempt.is_some(),
            route: Route {
                resolved_route,
                protocol: effective_protocol,
            },
            provider_request,
        })
    }

    /// Execute one unregistered request using the shared execution lifecycle.
    pub async fn execute(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
    ) -> Result<LlmResponse, LlmError> {
        if request.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        let mut prepared = self
            .prepare_internal(
                request,
                std::time::SystemTime::now(),
                Some(transport.clone()),
                false,
            )
            .await?;
        self.seal_prepared(&mut prepared).await?;
        let call = prepared.wire_call.take().expect("sealed");
        let raw = transport.clone();
        let collected = call
            .dispatch_once_using(raw.as_ref(), || Ok(()))
            .await
            .map_err(crate::upstream::error)?
            .collect()
            .await
            .map_err(crate::upstream::error)?;
        let decoded = crate::execution::decode(&collected).and_then(|response| {
            crate::upstream::project_response(
                response,
                crate::execution::response(collected.response()),
                crate::upstream::family(&prepared.route.protocol),
            )
        });
        collected.finish().await;
        decoded
    }
    pub async fn execute_stream(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
    ) -> Result<LlmEventStream, LlmError> {
        if request.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        if !request.stream {
            return Err(LlmError::InvalidRequest {
                message: "execute_stream requires LlmRequest.stream = true".into(),
            });
        }
        let mut prepared = self
            .prepare_internal(
                request,
                std::time::SystemTime::now(),
                Some(transport.clone()),
                false,
            )
            .await?;
        self.seal_prepared(&mut prepared).await?;
        let call = prepared.wire_call.take().expect("sealed");
        let received = call
            .dispatch_once_using(transport.as_ref(), || Ok(()))
            .await
            .map_err(crate::upstream::error)?;
        LlmEventStream::from_received(received, crate::upstream::family(&prepared.route.protocol))
            .await
    }
    pub async fn preconnect_websocket(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesWebSocketSession,
    ) -> Result<(), LlmError> {
        if request.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        if session.fallback_to_http() {
            return Ok(());
        }
        let mut req = request.clone();
        req.stream = true;
        let mut prepared = self
            .prepare_internal(
                &req,
                std::time::SystemTime::now(),
                Some(transport.clone()),
                false,
            )
            .await?;
        if !matches!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        ) {
            return Err(LlmError::InvalidRequest {message:"preconnect_websocket requires an OpenAI Responses profile with supports_websockets=true".into()});
        }
        let mut draft = prepared.wire_draft.take().expect("draft");
        session
            .shared
            .prepare_using(&mut draft, false, true, Some(transport.clone()))
            .await
            .map_err(|error| crate::execution::restore_error(error, &prepared.host_failure))
    }
    pub async fn prewarm_websocket(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesWebSocketSession,
    ) -> Result<(), LlmError> {
        if request.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        let mut req = request.clone();
        req.stream = true;
        let prepared = self
            .prepare_internal(
                &req,
                std::time::SystemTime::now(),
                Some(transport.clone()),
                false,
            )
            .await?;
        self.prewarm_prepared_websocket(prepared, transport, session)
            .await
    }
    pub async fn prewarm_prepared_websocket(
        &self,
        prepared: PreparedLlmCall,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesWebSocketSession,
    ) -> Result<(), LlmError> {
        if prepared.registered_attempt {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        if session.fallback_to_http() {
            return Ok(());
        }
        if !matches!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        ) {
            return Err(LlmError::InvalidRequest {
                message:
                    "prewarm_prepared_websocket requires an OpenAI Responses WebSocket request"
                        .into(),
            });
        }
        let (prepared, received) = self
            .dispatch_unregistered_stream(prepared, transport, session, true)
            .await?;
        let mut stream = LlmEventStream::from_received(
            received,
            crate::upstream::family(&prepared.route.protocol),
        )
        .await?;
        while stream.next_event().await?.is_some() {}
        Ok(())
    }
    pub async fn execute_stream_with_session(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesWebSocketSession,
    ) -> Result<LlmEventStream, LlmError> {
        if request.model_attempt.is_some() {
            return Err(crate::model_attempt::missing_hooks_error());
        }
        if !request.stream {
            return Err(LlmError::InvalidRequest {
                message: "execute_stream_with_session requires LlmRequest.stream = true".into(),
            });
        }
        let prepared = self
            .prepare_internal(
                request,
                std::time::SystemTime::now(),
                Some(transport.clone()),
                false,
            )
            .await?;
        let (prepared, received) = self
            .dispatch_unregistered_stream(prepared, transport, session, false)
            .await?;
        LlmEventStream::from_received(received, crate::upstream::family(&prepared.route.protocol))
            .await
    }
    async fn dispatch_unregistered_stream(
        &self,
        mut prepared: PreparedLlmCall,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesWebSocketSession,
        prewarm: bool,
    ) -> Result<(PreparedLlmCall, lingxi_llm_client::ReceivedCall), LlmError> {
        let websocket = matches!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        );
        let raw = transport.clone();
        let mut draft = prepared.wire_draft.take().expect("draft");
        draft.request_mut().headers = prepared
            .provider_request
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        draft
            .set_json_body(
                prepared.provider_request.body_json.clone(),
                &prepared.provider_request.json_string_overrides,
            )
            .map_err(crate::upstream::error)?;
        if websocket {
            session
                .shared
                .prepare_using(&mut draft, prewarm, !prewarm, Some(raw.clone()))
                .await
                .map_err(|error| crate::execution::restore_error(error, &prepared.host_failure))?;
        }
        let call = draft
            .seal()
            .await
            .map_err(|error| crate::execution::restore_error(error, &prepared.host_failure))?;
        let received = if websocket {
            session
                .shared
                .dispatch_using(call, || Ok(()), Some(raw.as_ref()))
                .await
        } else {
            call.dispatch_once_using(raw.as_ref(), || Ok(())).await
        }
        .map_err(crate::upstream::error)?;
        Ok((prepared, received))
    }

    /// Resolve, validate, encode, and authenticate an Anthropic
    /// `count_tokens` call. Errors on non-Anthropic routes.
    pub async fn prepare_count_tokens(
        &self,
        request: &LlmRequest,
    ) -> Result<ProviderRequest, LlmError> {
        let (draft, mut host, failure) = self.count_draft(request, None).await?;
        let call = crate::execution::seal(draft, &host)
            .await
            .map_err(|error| {
                failure
                    .lock()
                    .expect("host failure")
                    .take()
                    .unwrap_or(error)
            })?;
        host.headers = call.request().headers.iter().cloned().collect();
        host.body_bytes = Some(call.request().body.to_vec());
        Ok(host)
    }
    async fn count_draft(
        &self,
        request: &LlmRequest,
        transport: Option<Arc<dyn Transport>>,
    ) -> Result<
        (
            lingxi_llm_client::RequestDraft,
            ProviderRequest,
            crate::execution::HostFailure,
        ),
        LlmError,
    > {
        let route = self
            .registry
            .resolve_in(&request.model, request.profile.as_deref())?;
        validate_capabilities(request, route.capabilities)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        if !matches!(entry.protocol, ProtocolFamily::AnthropicMessages) {
            return Err(LlmError::InvalidRequest {
                message: "count_tokens is only available on AnthropicMessages routes".into(),
            });
        }
        let failure = Arc::new(Mutex::new(None));
        let auth = Arc::new(crate::execution::HostAuthenticator {
            client: self.clone(),
            now: None,
            failure: failure.clone(),
        });
        let (draft, mut host) = crate::execution::prepare(
            &self.cache,
            entry.profile.clone(),
            &route,
            request,
            transport,
            auth,
            lingxi_llm_client::RequestMode::CountTokens,
        )
        .await?;
        crate::model::betas::apply_beta_header(
            &mut host,
            crate::model::betas::Provider::Anthropic,
            crate::model::betas::Endpoint::CountTokens,
            &crate::model::betas::BetaContext::for_model(route.request_model),
        );
        Ok((draft, host, failure))
    }
    pub(crate) async fn count_tokens_exact(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
    ) -> Result<Option<u64>, LlmError> {
        let route = self
            .registry
            .resolve_in(&request.model, request.profile.as_deref())?;
        if !matches!(
            self.routes
                .get(&route.profile_name)
                .ok_or(LlmError::ModelUnavailable)?
                .protocol,
            ProtocolFamily::AnthropicMessages
        ) {
            return Ok(None);
        }
        // Counting is an auxiliary operation outside the generation watchdog.
        // Bound preparation/authentication, dispatch and response collection together.
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            let (draft, host, failure) = self.count_draft(request, Some(transport.clone())).await?;
            let call = crate::execution::seal(draft, &host)
                .await
                .map_err(|error| {
                    failure
                        .lock()
                        .expect("host failure")
                        .take()
                        .unwrap_or(error)
                })?;
            let collected = call
                .dispatch_once_using(transport.as_ref(), || Ok(()))
                .await
                .map_err(crate::upstream::error)?
                .collect()
                .await
                .map_err(crate::upstream::error)?;
            let result = collected
                .decode_token_count()
                .map(Some)
                .map_err(crate::upstream::error);
            collected.finish().await;
            result
        })
        .await
        .map_err(|_| LlmError::TransportTimeout {
            message: "Exact token counting exceeded its 120 second deadline".into(),
        })?
    }

    /// Upload raw media bytes via the Gemini File API resumable protocol.
    ///
    /// Resolves `model_or_alias` exactly like [`prepare`](Self::prepare) and
    /// requires the resolved profile's protocol family to be EXACTLY
    /// [`ProtocolFamily::GeminiGenerateContent`] — Vertex Gemini does not use
    /// the File API (media goes through GCS URIs there), so `VertexGemini`
    /// routes are rejected with [`LlmError::InvalidRequest`].
    ///
    /// Two-leg flow, both legs authenticated through the same path as
    /// `prepare` (`x-goog-api-key` for `ApiKey`-auth Gemini profiles):
    ///
    /// 1. START — `POST {upload_base}/upload/v1beta/files` with metadata; the
    ///    response's `x-goog-upload-url` header is the session URL.
    /// 2. UPLOAD+FINALIZE — `POST` the raw `bytes` (via
    ///    `ProviderRequest::body_bytes`) to that session URL.
    ///
    /// Non-2xx responses on either leg map through the shared Gemini error
    /// taxonomy. The returned [`crate::GeminiFile`] is NOT polled here:
    /// callers must poll
    /// [`crate::providers::gemini_files::file_status_request`] until
    /// `state == "ACTIVE"` for video/PDF uploads (or use the
    /// [`Self::wait_for_file_active`] convenience); images are typically
    /// `ACTIVE` immediately. A `FAILED` state passes through as data, not an
    /// error. The resulting `uri` plugs into
    /// [`crate::ContentBlock::ImageUrl`], which the Gemini codec encodes as a
    /// `file_data.file_uri` part.
    pub async fn upload_file(
        &self,
        model_or_alias: &str,
        bytes: Vec<u8>,
        mime_type: &str,
        display_name: &str,
        transport: Arc<dyn Transport>,
    ) -> Result<crate::GeminiFile, LlmError> {
        let route = self.registry.resolve(model_or_alias)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let failure = Arc::new(Mutex::new(None));
        let auth = crate::execution::HostAuthenticator {
            client: self.clone(),
            now: None,
            failure: failure.clone(),
        };
        let http = transport.clone();
        let mut profile = entry.profile.clone();
        profile.auth = lingxi_llm_client::protocol::AuthStrategy::Bearer;
        let service =
            lingxi_llm_client::FileService::new(http.as_ref(), &profile, Some(&auth), None, None);
        service
            .upload_gemini_unpolled(&lingxi_llm_client::UploadFile {
                filename: display_name.into(),
                media_type: mime_type.into(),
                bytes: bytes.into(),
            })
            .await
            .map_err(|e| crate::execution::restore_error(e, &failure))
    }

    /// Wait for readiness through the shared file lifecycle service.
    pub async fn wait_for_file_active(
        &self,
        model_or_alias: &str,
        file_name: &str,
        transport: Arc<dyn Transport>,
        poll: FileActivationPoll,
    ) -> Result<crate::GeminiFile, LlmError> {
        let route = self.registry.resolve(model_or_alias)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let failure = Arc::new(Mutex::new(None));
        let auth = crate::execution::HostAuthenticator {
            client: self.clone(),
            now: None,
            failure: failure.clone(),
        };
        let http = transport.clone();
        let mut profile = entry.profile.clone();
        profile.auth = lingxi_llm_client::protocol::AuthStrategy::Bearer;
        let service =
            lingxi_llm_client::FileService::new(http.as_ref(), &profile, Some(&auth), None, None);
        service
            .poll_gemini_active(file_name, poll.interval, poll.max_wait)
            .await
            .map_err(|e| crate::execution::restore_error(e, &failure))
    }

    // Each auth strategy is a self-contained arm; the length is necessary.
    #[allow(clippy::too_many_lines)]
    async fn authenticate(
        &self,
        entry: &RouteEntry,
        profile_name: &str,
        request: ProviderRequest,
    ) -> Result<ProviderRequest, LlmError> {
        use std::time::SystemTime;
        self.authenticate_at(entry, profile_name, request, SystemTime::now())
            .await
    }

    /// Injectable-clock variant used by tests to pin the `SigV4` timestamp.
    ///
    /// Production code calls `authenticate` which passes `SystemTime::now()`.
    /// Called from `prepare_at` (public) so that integration tests can use it
    /// without needing access to the private `RouteEntry` type.
    #[allow(clippy::too_many_lines)]
    async fn authenticate_at(
        &self,
        entry: &RouteEntry,
        profile_name: &str,
        mut request: ProviderRequest,
        now: std::time::SystemTime,
    ) -> Result<ProviderRequest, LlmError> {
        match entry.auth {
            AuthStrategy::None => return Ok(request),

            // ── AWS SigV4 ────────────────────────────────────────────────────
            // Credential::AwsSigV4 must be loaded via StaticCredentialProvider
            // or a host-managed store; the single-env-var Env path cannot
            // express three fields (access key, secret key, session token).
            AuthStrategy::AwsSigV4 => {
                // Require signing config (region + service).
                let signing = entry.signing.as_ref().ok_or_else(|| LlmError::InvalidRequest {
                    message: format!(
                        "provider profile '{profile_name}' uses AwsSigV4 but has no signing config; \
                         set ProviderProfile.signing = Some(SigningConfig {{ region, service }})"
                    ),
                })?;

                // Load Credential::AwsSigV4 from the credential store.
                let credential = self.load_credential(entry, profile_name).await?;
                let (access_key_id, secret_access_key, session_token) = match credential {
                    Some(Credential::AwsSigV4 {
                        access_key_id,
                        secret_access_key,
                        session_token,
                    }) => (access_key_id, secret_access_key, session_token),
                    Some(other) => {
                        return Err(LlmError::InvalidRequest {
                            message: format!(
                                "provider profile '{profile_name}': AwsSigV4 auth requires \
                                 Credential::AwsSigV4 but got {other:?}",
                            ),
                        });
                    }
                    None => {
                        // CredentialConfig::None — host opted out of signing.
                        return Ok(request);
                    }
                };

                // Body bytes: sign exactly what LlmTransportBridge sends on the wire.
                let body_bytes = request.wire_body_bytes()?;

                // Use the injected clock (now) for the timestamp.
                // Production code passes SystemTime::now(); tests pass a fixed instant.
                let datetime = {
                    use std::time::UNIX_EPOCH;
                    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
                    // Format as YYYYMMDDTHHMMSSZ from Unix seconds.
                    let (year, month, day, hour, min, sec) = secs_to_ymdhms(secs);
                    format!("{year:04}{month:02}{day:02}T{hour:02}{min:02}{sec:02}Z")
                };

                let signed = sigv4::sign_request(
                    &request.method,
                    &request.url,
                    &request.headers,
                    &body_bytes,
                    &access_key_id,
                    &secret_access_key,
                    session_token.as_deref(),
                    &signing.region,
                    &signing.service,
                    &datetime,
                )
                .map_err(|e| LlmError::InvalidRequest { message: e })?;

                request
                    .headers
                    .insert("x-amz-date".to_string(), signed.x_amz_date);
                request.headers.insert(
                    "x-amz-content-sha256".to_string(),
                    signed.x_amz_content_sha256,
                );
                if let Some(token) = signed.x_amz_security_token {
                    request
                        .headers
                        .insert("x-amz-security-token".to_string(), token);
                }
                request
                    .headers
                    .insert("Authorization".to_string(), signed.authorization);
                return Ok(request);
            }

            // ── GCP Token (Bearer token) ─────────────────────────────────────
            // Vertex AI / GCP services use OAuth2 bearer tokens.
            // Reuse BearerAuthenticator — same wire shape as OpenAI bearer.
            AuthStrategy::GcpToken => {
                let Some(secret) = self.load_secret(entry, profile_name).await? else {
                    return Ok(request);
                };
                let authenticator = BearerAuthenticator::new(secret);
                return authenticator.apply(request);
            }

            // ── Azure Token (api-key header) ─────────────────────────────────
            // Azure OpenAI uses `api-key: <key>` rather than `Authorization:
            // Bearer ...`. The key may come from Credential::ApiKey or
            // Credential::BearerToken (host-managed stores may use either).
            //
            // Reference:
            // https://learn.microsoft.com/en-us/azure/ai-services/openai/reference
            AuthStrategy::AzureToken => {
                let Some(secret) = self.load_secret(entry, profile_name).await? else {
                    return Ok(request);
                };
                let authenticator = ApiKeyAuthenticator::with_header_name("api-key", secret);
                return authenticator.apply(request);
            }

            // ── ChatGPT OAuth ────────────────────────────────────────────────
            // Credential::ChatGptOAuth carries the bearer access token plus the
            // ChatGPT-Account-ID header (and FedRAMP flag). Must be loaded via
            // load_credential() — load_secret() rejects it.
            AuthStrategy::ChatGptOAuth => {
                let Some(secret) = self.load_credential(entry, profile_name).await? else {
                    return Ok(request);
                };
                let Credential::ChatGptOAuth {
                    access_token,
                    account_id,
                    fedramp,
                } = secret
                else {
                    return Err(LlmError::Authentication {
                        message: String::new(),
                    });
                };
                // Codex subscription requests use the Responses backend's
                // restricted shape (OpenCode's Codex plugin also omits its
                // maxOutputTokens parameter). Keep API-key profiles unchanged.
                lingxi_llm_client::auth::header_policy::chatgpt_body(&mut request.body_json);
                let authenticator = ChatGptAuthenticator::new(access_token, account_id, fedramp);
                request = authenticator.apply(request)?;
            }

            // ── Standard key / bearer auth ───────────────────────────────────
            AuthStrategy::ApiKey
            | AuthStrategy::Bearer
            | AuthStrategy::OAuthBearer
            | AuthStrategy::CopilotBearer => {
                let Some(secret) = self.load_secret(entry, profile_name).await? else {
                    // CredentialConfig::None: the host opted out of
                    // client-applied authentication for this profile.
                    return Ok(request);
                };
                let authenticator: Box<dyn Authenticator> = match (&entry.auth, &entry.protocol) {
                    // GitHub Copilot: GitHub OAuth token used directly as the
                    // bearer plus the Copilot header set (also strips x-api-key).
                    (AuthStrategy::CopilotBearer, _) => Box::new(CopilotAuthenticator::new(secret)),
                    (AuthStrategy::ApiKey, ProtocolFamily::AnthropicMessages) => {
                        Box::new(ApiKeyAuthenticator::new(secret))
                    }
                    // Azure AI Foundry: a plain API key is sent as `x-api-key`
                    // (CC 2.1.207 `AnthropicFoundry.authHeaders()`:
                    // string apiKey ⇒ `{"x-api-key": apiKey}`). AAD-token Foundry
                    // profiles use `AuthStrategy::Bearer` (the `_` arm) which
                    // emits `Authorization: Bearer`, matching the SDK's
                    // `azureADTokenProvider` (function) path.
                    (AuthStrategy::ApiKey, ProtocolFamily::FoundryClaude) => {
                        Box::new(ApiKeyAuthenticator::new(secret))
                    }
                    (AuthStrategy::ApiKey, ProtocolFamily::GeminiGenerateContent) => Box::new(
                        ApiKeyAuthenticator::with_header_name("x-goog-api-key", secret),
                    ),
                    // OpenAI-style APIs send api keys as bearer tokens.
                    _ => Box::new(BearerAuthenticator::new(secret)),
                };
                request = authenticator.apply(request)?;
            }
        }

        // Anthropic accepts OAuth bearer tokens only with the oauth beta flag.
        //
        // Parity: `constants/oauth.ts:36` OAUTH_BETA_HEADER = 'oauth-2025-04-20';
        // `utils/betas.ts:251-252` appends it via push() (list element, not clobber).
        // Use append_beta so that any betas already present (e.g., set by the
        // orchestrator's betas assembler before authenticate runs) are preserved.
        if matches!(entry.auth, AuthStrategy::OAuthBearer)
            && matches!(entry.protocol, ProtocolFamily::AnthropicMessages)
        {
            lingxi_llm_client::auth::header_policy::anthropic_oauth(&mut request.headers);
        }
        Ok(request)
    }

    /// Load credential material for `entry`, returning `None` when the
    /// credential config is `None` (host opted out of client auth).
    async fn load_credential(
        &self,
        entry: &RouteEntry,
        profile_name: &str,
    ) -> Result<Option<Credential>, LlmError> {
        let credential = match &entry.credential {
            CredentialConfig::None => return Ok(None),
            CredentialConfig::Env { var } => {
                EnvCredentialProvider::new(var.clone())
                    .load(&CredentialScope::new(
                        entry.provider_id.clone(),
                        profile_name,
                    ))
                    .await?
            }
            CredentialConfig::Static { id } | CredentialConfig::HostManaged { id } => {
                self.credentials
                    .as_ref()
                    .ok_or(LlmError::Authentication {
                        message: String::new(),
                    })?
                    .load(
                        &CredentialScope::new(entry.provider_id.clone(), profile_name)
                            .with_credential_id(id.clone()),
                    )
                    .await?
            }
        };
        Ok(Some(credential))
    }

    /// Load a plain string secret for key/bearer auth strategies.
    ///
    /// Returns `None` when `CredentialConfig::None` was set (host opted out).
    /// Returns `Err` when the credential is `AwsSigV4` (use `load_credential`
    /// instead for that strategy).
    async fn load_secret(
        &self,
        entry: &RouteEntry,
        profile_name: &str,
    ) -> Result<Option<String>, LlmError> {
        let Some(credential) = self.load_credential(entry, profile_name).await? else {
            return Ok(None);
        };

        match credential {
            Credential::ApiKey(secret) | Credential::BearerToken(secret) => Ok(Some(secret)),
            // AwsSigV4 creds are three-field structs; callers that need them
            // must use load_credential() directly.
            Credential::AwsSigV4 { .. } => Err(LlmError::InvalidRequest {
                message: format!(
                    "provider profile '{profile_name}': AwsSigV4 credentials must be loaded \
                     via load_credential(), not load_secret()"
                ),
            }),
            // ChatGptOAuth creds are multi-field structs; callers that need them
            // must use load_credential() directly.
            Credential::ChatGptOAuth { .. } => Err(LlmError::InvalidRequest {
                message: format!(
                    "provider profile '{profile_name}': ChatGptOAuth credentials must be loaded \
                     via load_credential(), not load_secret()"
                ),
            }),
        }
    }
}

fn route_allows_first_party_fast_mode(route: &crate::ResolvedRoute, entry: &RouteEntry) -> bool {
    route.provider_id == ProviderId::AnthropicFirstParty
        && entry.protocol == ProtocolFamily::AnthropicMessages
        && entry.base_url.trim_end_matches('/') == "https://api.anthropic.com"
        && platform_api::model_capabilities::has_capability(
            &route.request_model,
            platform_api::model_capabilities::ModelCapability::FastMode,
        )
}

/// Convert Unix epoch seconds to `(year, month, day, hour, minute, second)`.
///
/// Used to build the `YYYYMMDDTHHMMSSZ` timestamp for `SigV4` signing without
/// depending on `chrono` or `time` crates.
pub(crate) use lingxi_llm_client::auth::sigv4::secs_to_ymdhms;

/// GitHub Copilot serves its GPT-5.x and `codex` models ONLY through the OpenAI
/// Responses endpoint (`api.githubcopilot.com/responses`); older models use
/// `/chat/completions`. Our `github-copilot` preset declares a single
/// `OpenAiChat` protocol for the whole provider, so those newer models 400 with
/// "model … is not accessible via the /chat/completions endpoint". When we
/// detect one we route it through a Responses codec bound to the SAME host — the
/// Copilot bearer authorizes both paths. Mirrors the fix other Copilot gateways
/// adopted (cherry-studio #13637, opencode #5866): non-`codex` GPT-5+ models
/// must use `/responses`.
///
/// Returns `Some(OpenAiResponses)` only for the `github-copilot` profile on an
/// `OpenAiChat` route whose model needs Responses; `None` leaves routing intact
/// (so `openai`/`openrouter`/`deepseek`/etc. are never affected).
fn copilot_responses_override(
    profile_name: &str,
    protocol: &ProtocolFamily,
    request_model: &str,
) -> Option<ProtocolFamily> {
    if profile_name != "github-copilot" || !matches!(protocol, ProtocolFamily::OpenAiChat) {
        return None;
    }
    let model = request_model.to_ascii_lowercase();
    (model.contains("codex") || is_gpt5_or_newer(&model)).then_some(ProtocolFamily::OpenAiResponses)
}

/// `true` for `gpt-<major>[…]` with `major >= 5` (`gpt-5`, `gpt-5.5`,
/// `gpt-5-mini`, `gpt-6`, …); `false` for `gpt-4o`, `gpt-4.1`, non-`gpt-` ids.
fn is_gpt5_or_newer(model_lower: &str) -> bool {
    let Some(rest) = model_lower.strip_prefix("gpt-") else {
        return false;
    };
    let major: String = rest.chars().take_while(char::is_ascii_digit).collect();
    major.parse::<u32>().is_ok_and(|n| n >= 5)
}

fn validate_provider_profile(provider: &crate::ProviderProfile) -> Result<(), LlmError> {
    if provider.supports_websocket_compression {
        return Err(LlmError::InvalidRequest {
            message: format!(
                "provider profile '{}' enables supports_websocket_compression, \
                 but this build does not expose a stable WebSocket compression configuration",
                provider.profile_name
            ),
        });
    }

    if !provider.supports_websockets {
        return Ok(());
    }

    if !matches!(provider.protocol, ProtocolFamily::OpenAiResponses) {
        return Err(LlmError::InvalidRequest {
            message: format!(
                "provider profile '{}' enables supports_websockets but uses protocol {:?}; \
                 Responses WebSocket transport is only valid for OpenAiResponses",
                provider.profile_name, provider.protocol
            ),
        });
    }

    if matches!(provider.auth, AuthStrategy::AwsSigV4) {
        return Err(LlmError::InvalidRequest {
            message: format!(
                "provider profile '{}' enables supports_websockets with AwsSigV4; \
                 Responses WebSocket transport does not support SigV4 signing",
                provider.profile_name
            ),
        });
    }

    Ok(())
}

pub struct PreparedLlmCall {
    pub(crate) host_failure: crate::execution::HostFailure,
    pub(crate) wire_draft: Option<lingxi_llm_client::RequestDraft>,
    pub(crate) wire_call: Option<lingxi_llm_client::PreparedCall>,
    registered_attempt: bool,
    pub route: Route,
    pub provider_request: ProviderRequest,
}

/// Pull-based application events projected from the shared client's stream.
pub struct LlmEventStream {
    stream: lingxi_llm_client::ModelStream,
    projection: crate::upstream::Decoder,
    queue: VecDeque<LlmEvent>,
    yielded_any: bool,
    finished: bool,
}
impl LlmEventStream {
    /// Scoped continuation state from a successfully completed Responses stream.
    /// Interrupted or still-running streams do not yield a reusable reference.
    pub fn continuation(&self) -> Option<&lingxi_llm_client::protocol::ContinuationRef> {
        self.stream.continuation()
    }

    async fn from_received(
        received: lingxi_llm_client::ReceivedCall,
        protocol: lingxi_llm_client::protocol::ProtocolFamily,
    ) -> Result<Self, LlmError> {
        let stream = match received.into_stream() {
            Ok(stream) => stream,
            Err(received) => {
                let collected = received.collect().await.map_err(crate::upstream::error)?;
                let error = crate::execution::decode(&collected)
                    .err()
                    .unwrap_or(LlmError::ProviderInternal);
                collected.finish().await;
                return Err(error);
            }
        };
        let headers = stream
            .headers()
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        let projection = crate::upstream::Decoder::projection(
            protocol,
            crate::stream_provider_metadata_from_headers(&headers),
        );
        Ok(Self {
            stream,
            projection,
            queue: VecDeque::new(),
            yielded_any: false,
            finished: false,
        })
    }
    pub async fn next_event(&mut self) -> Result<Option<LlmEvent>, LlmError> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                self.yielded_any = true;
                return Ok(Some(event));
            }
            if self.finished {
                return Ok(None);
            }
            let Some(batch) = self.stream.next_batch().await else {
                self.finished = true;
                return Ok(None);
            };
            self.finished = batch.finished;
            match self.projection.project_batch(batch) {
                Ok(events) => self.queue.extend(events),
                Err(error) => {
                    self.finished = true;
                    return Err(match error {
                        LlmError::Transport { message } if self.yielded_any => {
                            LlmError::StreamInterrupted { message }
                        }
                        other => other,
                    });
                }
            }
        }
    }
}

impl std::fmt::Debug for LlmEventStream {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LlmEventStream")
            .field("queued_events", &self.queue.len())
            .field("yielded_any", &self.yielded_any)
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

/// Append `beta` to the comma-joined `anthropic-beta` header value if it is not
/// already present.
///
/// - If `existing` is `None`, returns `beta.to_string()` (first entry).
/// - If `existing` already contains `beta` as a comma-separated segment (trimmed
///   match — handles `"a, b"` spacing that arises when headers are joined with
///   `", "`), the original value is returned unchanged.
/// - Otherwise `", beta"` is appended to `existing`.
///
/// **Passing `Some("")` is not expected and would yield a leading comma.**
///
/// **Parity:** mirrors `claude-code/src/utils/betas.ts:251-252` semantics where
/// `OAUTH_BETA_HEADER` is pushed into the beta list only when
/// `isClaudeAISubscriber()` is true, and the list is later joined — it is never
/// a standalone clobbering insert.
///
/// **`constants/oauth.ts:36`:** `OAUTH_BETA_HEADER = 'oauth-2025-04-20'` is the
/// beta value passed to this function by `authenticate()` for OAuth sessions.
#[must_use]
pub(crate) fn append_beta(existing: Option<&str>, beta: &str) -> String {
    match existing {
        None => beta.to_string(),
        Some(current) => {
            // Check whether `beta` is already a segment (trim to handle ", "-joined lists).
            if current.split(',').any(|seg| seg.trim() == beta) {
                current.to_string()
            } else {
                format!("{current},{beta}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{append_beta, copilot_responses_override, is_gpt5_or_newer, ProtocolFamily};

    #[test]
    fn gpt5_plus_detection_covers_majors_and_variants() {
        for yes in [
            "gpt-5",
            "gpt-5.5",
            "gpt-5-mini",
            "gpt-5.2-codex",
            "gpt-6",
            "gpt-10",
        ] {
            assert!(is_gpt5_or_newer(yes), "{yes} should be gpt-5+");
        }
        for no in [
            "gpt-4o",
            "gpt-4.1",
            "gpt-4-turbo",
            "o3",
            "claude-opus-4-8",
            "",
        ] {
            assert!(!is_gpt5_or_newer(no), "{no} should NOT be gpt-5+");
        }
    }

    #[test]
    fn copilot_override_fires_only_for_copilot_chat_gpt5_and_codex() {
        let chat = ProtocolFamily::OpenAiChat;
        // Fires: Copilot + OpenAiChat + gpt-5.x/codex.
        for m in ["gpt-5.5", "gpt-5-codex", "gpt-5.4-mini"] {
            assert_eq!(
                copilot_responses_override("github-copilot", &chat, m),
                Some(ProtocolFamily::OpenAiResponses),
                "{m} on copilot should override to Responses"
            );
        }
        // No override: older Copilot model.
        assert_eq!(
            copilot_responses_override("github-copilot", &chat, "gpt-4o"),
            None
        );
        // No override: different profile, even for a gpt-5 id.
        assert_eq!(
            copilot_responses_override("openrouter", &chat, "gpt-5.5"),
            None
        );
        // No override: already Responses (openai first-party) — nothing to fix.
        assert_eq!(
            copilot_responses_override(
                "github-copilot",
                &ProtocolFamily::OpenAiResponses,
                "gpt-5.5"
            ),
            None
        );
    }

    // ---- Task 2: `append_beta` pure-fn unit tests ----
    //
    // Reference: `claude-code/src/utils/betas.ts:251-252`:
    //   if (isClaudeAISubscriber()) { betaHeaders.push(OAUTH_BETA_HEADER) }
    // Reference: `claude-code/src/constants/oauth.ts:36`:
    //   export const OAUTH_BETA_HEADER = 'oauth-2025-04-20' as const

    #[test]
    fn append_beta_to_none_returns_beta_alone() {
        assert_eq!(append_beta(None, "oauth-2025-04-20"), "oauth-2025-04-20");
    }

    #[test]
    fn append_beta_to_existing_single_entry_comma_joins() {
        assert_eq!(
            append_beta(Some("claude-code-20250219"), "oauth-2025-04-20"),
            "claude-code-20250219,oauth-2025-04-20",
        );
    }

    #[test]
    fn append_beta_to_existing_multi_entry_appends_at_end() {
        assert_eq!(
            append_beta(
                Some("claude-code-20250219,interleaved-thinking-2025-05-14"),
                "oauth-2025-04-20"
            ),
            "claude-code-20250219,interleaved-thinking-2025-05-14,oauth-2025-04-20",
        );
    }

    #[test]
    fn append_beta_does_not_duplicate_when_already_present() {
        // oauth-2025-04-20 is already in the list — must not be added again.
        assert_eq!(
            append_beta(
                Some("claude-code-20250219,oauth-2025-04-20"),
                "oauth-2025-04-20"
            ),
            "claude-code-20250219,oauth-2025-04-20",
        );
    }

    #[test]
    fn append_beta_does_not_duplicate_when_only_entry() {
        assert_eq!(
            append_beta(Some("oauth-2025-04-20"), "oauth-2025-04-20"),
            "oauth-2025-04-20",
        );
    }

    /// Verify the `authenticate()` clobbering bug is exercised via `append_beta`:
    /// a pre-existing multi-beta header must not be overwritten when oauth is added.
    #[test]
    fn oauth_beta_does_not_clobber_existing_betas() {
        let existing = "claude-code-20250219,interleaved-thinking-2025-05-14";
        let result = append_beta(Some(existing), "oauth-2025-04-20");
        // Both pre-existing betas survive.
        assert!(
            result.split(',').any(|p| p == "claude-code-20250219"),
            "claude-code beta must survive; got: {result}"
        );
        assert!(
            result
                .split(',')
                .any(|p| p == "interleaved-thinking-2025-05-14"),
            "interleaved-thinking beta must survive; got: {result}"
        );
        // And oauth is now present.
        assert!(
            result.split(',').any(|p| p == "oauth-2025-04-20"),
            "oauth beta must be present; got: {result}"
        );
    }

    /// Dedup must work even when segments carry surrounding whitespace from a
    /// `", "`-joined list (e.g., `"a, oauth-2025-04-20"` — the space after
    /// the comma is present).  Passing the same beta again must be a no-op.
    #[test]
    fn append_beta_dedups_with_whitespace() {
        // "a, oauth-2025-04-20" — note the space after the comma.
        let result = append_beta(Some("a, oauth-2025-04-20"), "oauth-2025-04-20");
        assert_eq!(
            result, "a, oauth-2025-04-20",
            "dedup must fire even when the segment has leading whitespace; got: {result}",
        );
    }
}

impl std::fmt::Debug for PreparedLlmCall {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedLlmCall")
            .field("route", &self.route)
            .finish_non_exhaustive()
    }
}
impl DefaultLlmClient {
    pub(crate) async fn seal_prepared(
        &self,
        prepared: &mut PreparedLlmCall,
    ) -> Result<(), LlmError> {
        let draft = prepared
            .wire_draft
            .take()
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "request already sealed".into(),
            })?;
        let call = crate::execution::seal(draft, &prepared.provider_request)
            .await
            .map_err(|error| {
                prepared
                    .host_failure
                    .lock()
                    .expect("host failure")
                    .take()
                    .unwrap_or(error)
            })?;
        prepared.provider_request.headers = call.request().headers.iter().cloned().collect();
        if let Ok(body) = serde_json::from_slice(&call.request().body) {
            prepared.provider_request.body_json = body;
        }
        prepared.wire_call = Some(call);
        Ok(())
    }
}

impl DefaultLlmClient {
    pub(crate) async fn prepare_shared_stream(
        &self,
        mut prepared: PreparedLlmCall,
        session: &mut ResponsesWebSocketSession,
    ) -> Result<PreparedLlmCall, LlmError> {
        let websocket = matches!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        );
        let mut draft = prepared
            .wire_draft
            .take()
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "request already sealed".into(),
            })?;
        let http = draft.request_mut();
        http.headers = prepared
            .provider_request
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        draft
            .set_json_body(
                prepared.provider_request.body_json.clone(),
                &prepared.provider_request.json_string_overrides,
            )
            .map_err(crate::upstream::error)?;
        if websocket {
            session
                .shared
                .prepare(&mut draft, false, true)
                .await
                .map_err(crate::upstream::error)?;
        }
        let call = draft
            .seal()
            .await
            .map_err(|error| crate::execution::restore_error(error, &prepared.host_failure))?;
        prepared.provider_request.headers = call.request().headers.iter().cloned().collect();
        prepared.wire_call = Some(call);
        Ok(prepared)
    }
    pub(crate) async fn open_shared_stream(
        &self,
        mut prepared: PreparedLlmCall,
        session: &mut ResponsesWebSocketSession,
        on_dispatch: &mut (dyn FnMut() -> Result<(), LlmError> + Send),
    ) -> Result<(PreparedLlmCall, lingxi_llm_client::ReceivedCall), LlmError> {
        let websocket = matches!(
            prepared.provider_request.stream_transport,
            ProviderStreamTransport::ResponsesWebSocket
        );
        let call = prepared
            .wire_call
            .take()
            .ok_or_else(|| LlmError::InvalidRequest {
                message: "request must be sealed before dispatch".into(),
            })?;
        let mark = || on_dispatch().map_err(crate::execution::wire_error);
        let received = if websocket {
            session.shared.dispatch(call, mark).await
        } else {
            call.dispatch_once_with(mark).await
        }
        .map_err(crate::upstream::error)?;
        Ok((prepared, received))
    }
}

impl Clone for RouteEntry {
    fn clone(&self) -> Self {
        Self {
            profile: self.profile.clone(),
            protocol: self.protocol.clone(),
            provider_id: self.provider_id.clone(),
            auth: self.auth.clone(),
            credential: self.credential.clone(),
            base_url: self.base_url.clone(),
            signing: self.signing.clone(),
            supports_websockets: self.supports_websockets,
            websocket_connect_timeout_ms: self.websocket_connect_timeout_ms,
        }
    }
}
impl DefaultLlmClient {
    pub(crate) async fn authenticate_wire(
        &self,
        profile: &str,
        request: &mut lingxi_llm_client::HttpRequest,
        now: std::time::SystemTime,
    ) -> Result<(), LlmError> {
        let entry = self.routes.get(profile).ok_or(LlmError::ModelUnavailable)?;
        let mut host = ProviderRequest::post_json(
            request.url.clone(),
            serde_json::from_slice(&request.body).unwrap_or_default(),
        );
        host.method.clone_from(&request.method);
        host.headers = request.headers.iter().cloned().collect();
        // Non-JSON bodies and exact UTF-16 strings must retain their byte image.
        if serde_json::from_slice::<serde_json::Value>(&request.body).is_err() {
            host.body_bytes = Some(request.body.to_vec());
        }
        let host = self.authenticate_at(entry, profile, host, now).await?;
        request.headers = host
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        request.body = host.wire_body_bytes()?.into();
        Ok(())
    }
}

impl PreparedLlmCall {
    /// Exact model prices retained before the physical generation dispatch.
    pub fn pricing_snapshot(&self) -> Option<lingxi_llm_client::FrozenPricing> {
        self.wire_call
            .as_ref()
            .map(lingxi_llm_client::PreparedCall::pricing_snapshot)
            .or_else(|| {
                let draft = self.wire_draft.as_ref()?;
                lingxi_llm_client::FrozenPricing::capture(
                    draft.profile(),
                    &draft.model().display_model,
                    &draft.model().request_model,
                )
                .ok()
            })
    }
}

#[cfg(test)]
mod shared_client_regression {
    use super::*;
    #[tokio::test]
    async fn configuration_cache_is_reused_without_caching_request_credentials() {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"api_key", "credential":{"type":"host_managed","id":"key"},
            "models":[{"display_model":"claude-test","request_model":"claude-test","billing_model":"claude-test","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = DefaultLlmClient::from_config(config).unwrap();
        for secret in ["account-a", "account-b", "account-a"] {
            let client = client.clone().with_credential_provider(Arc::new(
                crate::StaticCredentialProvider::new(crate::Credential::ApiKey(secret.into())),
            ));
            let prepared = client
                .prepare(&LlmRequest::new("claude-test"))
                .await
                .unwrap();
            assert_eq!(
                prepared
                    .provider_request
                    .headers
                    .get("x-api-key")
                    .map(String::as_str),
                Some(secret)
            );
        }
        assert_eq!(client.cache.len(), 1);
    }
}
