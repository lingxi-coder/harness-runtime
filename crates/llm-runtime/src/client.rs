use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    validate_capabilities, AuthStrategy, ClientConfig, Credential, CredentialConfig,
    CredentialProvider, CredentialScope, EnvCredentialProvider, LlmError, LlmRequest, MediaRoute,
    ModelListing, ModelRegistry, ProtocolFamily, ProviderId, ProviderRequest,
    ProviderStreamTransport, Route, Transport,
};

/// Host execution composition: immutable routing, request-local credentials,
/// and shared SDK clients. Protocol execution and stream assembly belong to SDK.
#[derive(Debug, Clone)]
pub struct ModelRuntime {
    cache: Arc<crate::execution::ClientCache>,
    registry: Arc<ModelRegistry>,
    routes: Arc<BTreeMap<String, RouteEntry>>,
    credentials: Option<Arc<dyn CredentialProvider>>,
}

#[derive(Debug, Clone)]
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

struct ServiceAuthenticator(ModelRuntime);

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
                    return Err(
                        lingxi_llm_client::protocol::LlmError::UnsupportedCapability {
                            message: format!(
                                "explicit service credentials are unsupported for {:?} authentication on profile {:?}",
                                profile.auth, profile.profile_name
                            ),
                        },
                    );
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

/// Polling knobs for [`ModelRuntime::wait_for_file_active`]. The defaults
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

pub use lingxi_llm_client::websocket::ResponsesWebSocketRequestSnapshot;
pub use lingxi_llm_client::ResponsesSession;

impl ModelRuntime {
    /// Resolve the SDK wire family before adapting durable history. Per-model
    /// protocol selection is provider-owned, while the host selects the route.
    pub fn protocol_for_model(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<ProtocolFamily, LlmError> {
        let route = self.registry.resolve_in(model, profile)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        Ok(
            lingxi_llm_client::providers::github_copilot::responses_protocol_override(
                &route.profile_name,
                &entry.protocol,
                &route.request_model,
            )
            .unwrap_or(entry.protocol),
        )
    }

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
                profile.auth = entry.auth;
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
            .resolve_in(&request.input.model, request.profile.as_deref())?;

        let effective_protocol =
            self.protocol_for_model(&request.input.model, request.profile.as_deref())?;
        let adapted;
        let request = if let Some(source) = request.execution.input_protocol.filter(|source| {
            lingxi_llm_client::replay::native_family(*source)
                != lingxi_llm_client::replay::native_family(effective_protocol)
        }) {
            adapted = crate::upstream::adapt_request(request, source, effective_protocol)?;
            &adapted
        } else {
            request
        };

        // Soft-degrade `reasoning` rather than hard-failing: it is a best-effort
        // enhancement driven by the SESSION thinking config, so switching to a
        // model that doesn't advertise reasoning (e.g. an OpenRouter free model
        // like `qwen/qwen3-coder:free`) must silently drop it — not break the turn
        // with "unsupported capability: reasoning". This covers BOTH the top-level
        // `request.input.thinking` field AND any `Reasoning`/`RedactedThinking` blocks
        // left in message HISTORY from an earlier thinking-capable model —
        // `validate_capabilities` rejects those blocks independently, so a
        // mid-conversation downgrade (not just a first turn) must strip them from
        // the request that gets validated AND encoded. (streaming / tools /
        // structured_output stay hard errors — they cannot be dropped safely.)
        let is_reasoning_block = |b: &lingxi_llm_client::protocol::ContentBlock| {
            matches!(
                b,
                lingxi_llm_client::protocol::ContentBlock::Thinking { .. }
                    | lingxi_llm_client::protocol::ContentBlock::RedactedThinking { .. }
            )
        };
        let needs_reasoning_degrade = !resolved_route.capabilities.reasoning
            && (request.input.thinking.is_some()
                || request
                    .input
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
        let needs_speed_degrade = matches!(
            entry.protocol,
            ProtocolFamily::AnthropicMessages
                | ProtocolFamily::BedrockClaude
                | ProtocolFamily::VertexClaude
                | ProtocolFamily::FoundryClaude
        ) && request.input.service_tier
            == Some(lingxi_llm_client::protocol::ServiceTier::Fast)
            && !route_allows_first_party_fast_mode(&resolved_route, entry);

        let mut owned: Option<LlmRequest> = None;
        if needs_reasoning_degrade || needs_speed_degrade {
            let mut r = request.clone();
            if needs_reasoning_degrade {
                r.input.thinking = None;
                crate::model::thinking_signature::retain_input_blocks(
                    &mut r,
                    |_, block| !is_reasoning_block(block),
                    None,
                );
            }
            if needs_speed_degrade {
                r.input.service_tier = None;
            }
            owned = Some(r);
        }
        let request = owned.as_ref().unwrap_or(request);

        validate_capabilities(request, resolved_route.capabilities)?;

        let mut routed_request;
        let encoding_request = if request.input.model == resolved_route.request_model {
            request
        } else {
            routed_request = request.clone();
            routed_request
                .input
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
            registered_attempt: request.execution.model_attempt.is_some(),
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
    ) -> Result<lingxi_llm_client::protocol::ChatResponse, LlmError> {
        if request.execution.model_attempt.is_some() {
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
        let decoded = crate::execution::decode(&collected);
        collected.finish().await;
        decoded
    }
    pub async fn execute_stream(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
    ) -> Result<lingxi_llm_client::ModelStream, LlmError> {
        if request.execution.model_attempt.is_some() {
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
        model_stream(received).await
    }
    pub async fn preconnect_websocket(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesSession,
    ) -> Result<(), LlmError> {
        if request.execution.model_attempt.is_some() {
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
            .prepare_using(&mut draft, false, true, Some(transport.clone()))
            .await
            .map_err(|error| crate::execution::restore_error(error, &prepared.host_failure))
    }
    pub async fn prewarm_websocket(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesSession,
    ) -> Result<(), LlmError> {
        if request.execution.model_attempt.is_some() {
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
        session: &mut ResponsesSession,
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
        let (_prepared, received) = self
            .dispatch_unregistered_stream(prepared, transport, session, true)
            .await?;
        let mut stream = model_stream(received).await?;
        while let Some(batch) = stream.next_batch().await {
            for event in batch.events {
                event.map_err(crate::upstream::error)?;
            }
        }
        Ok(())
    }
    pub async fn execute_stream_with_session(
        &self,
        request: &LlmRequest,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesSession,
    ) -> Result<lingxi_llm_client::ModelStream, LlmError> {
        if request.execution.model_attempt.is_some() {
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
        let (_prepared, received) = self
            .dispatch_unregistered_stream(prepared, transport, session, false)
            .await?;
        model_stream(received).await
    }
    async fn dispatch_unregistered_stream(
        &self,
        mut prepared: PreparedLlmCall,
        transport: Arc<dyn Transport>,
        session: &mut ResponsesSession,
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
            .resolve_in(&request.input.model, request.profile.as_deref())?;
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
            .resolve_in(&request.input.model, request.profile.as_deref())?;
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
    /// [`Self::wait_for_file_active`] until
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

    /// Preparation inspection uses the same SDK authenticator as live sends.
    async fn authenticate_at(
        &self,
        _entry: &RouteEntry,
        profile_name: &str,
        mut request: ProviderRequest,
        now: std::time::SystemTime,
    ) -> Result<ProviderRequest, LlmError> {
        let mut wire = lingxi_llm_client::HttpRequest {
            method: request.method.clone(),
            url: request.url.clone(),
            headers: request
                .headers
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            body: request.wire_body_bytes()?.into(),
            timeout: None,
        };
        self.authenticate_wire(profile_name, &mut wire, now).await?;
        request.headers = wire.headers.into_iter().collect();
        if let Ok(body) = serde_json::from_slice(&wire.body) {
            request.body_json = body;
        }
        // Preserve the signed byte image, including exact UTF-16 overrides.
        request.body_bytes = Some(wire.body.to_vec());
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

/// Keep provider execution canonical. History projection is the service edge.
async fn model_stream(
    received: lingxi_llm_client::ReceivedCall,
) -> Result<lingxi_llm_client::ModelStream, LlmError> {
    match received.into_stream() {
        Ok(stream) => Ok(stream),
        Err(received) => {
            let collected = received.collect().await.map_err(crate::upstream::error)?;
            let error = crate::execution::decode(&collected)
                .err()
                .unwrap_or(LlmError::ProviderInternal);
            collected.finish().await;
            Err(error)
        }
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
#[cfg(test)]
#[must_use]
fn append_beta(existing: Option<&str>, beta: &str) -> String {
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
    use super::append_beta;

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
impl ModelRuntime {
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

impl ModelRuntime {
    pub(crate) async fn prepare_shared_stream(
        &self,
        mut prepared: PreparedLlmCall,
        session: &mut ResponsesSession,
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
        session: &mut ResponsesSession,
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
            session.dispatch(call, mark).await
        } else {
            call.dispatch_once_with(mark).await
        }
        .map_err(crate::upstream::error)?;
        Ok((prepared, received))
    }
}

impl ModelRuntime {
    pub(crate) async fn authenticate_wire(
        &self,
        profile: &str,
        request: &mut lingxi_llm_client::HttpRequest,
        now: std::time::SystemTime,
    ) -> Result<(), LlmError> {
        use lingxi_llm_client::auth::{apply_credential, ClientIdentity, CredentialRef};
        let entry = self.routes.get(profile).ok_or(LlmError::ModelUnavailable)?;
        if entry.auth == AuthStrategy::None {
            return Ok(());
        }
        let Some(credential) = self.load_credential(entry, profile).await? else {
            return Ok(());
        };
        let material = match &credential {
            Credential::ApiKey(secret) | Credential::BearerToken(secret) => {
                CredentialRef::Token(secret)
            }
            Credential::AwsSigV4 {
                access_key_id,
                secret_access_key,
                session_token,
            } => CredentialRef::Aws {
                access_key_id,
                secret_access_key,
                session_token: session_token.as_deref(),
            },
            Credential::ChatGptOAuth {
                access_token,
                account_id,
                fedramp,
            } => CredentialRef::ChatGpt {
                access_token,
                account_id: account_id.as_deref(),
                fedramp: *fedramp,
            },
        };
        let mut sdk_profile = entry.profile.clone();
        sdk_profile.auth = entry.auth;
        apply_credential(
            request,
            &sdk_profile,
            material,
            ClientIdentity {
                user_agent: crate::auth::copilot::COPILOT_USER_AGENT,
                editor_version: crate::auth::copilot::COPILOT_EDITOR_VERSION,
                plugin_version: crate::auth::copilot::COPILOT_EDITOR_PLUGIN_VERSION,
            },
            now,
        )
        .map_err(crate::upstream::error)
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
    async fn canonical_openai_service_tier_survives_host_anthropic_fast_policy() {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({"providers":[{
            "provider_id":"open_ai", "profile_name":"openai", "base_url":"https://api.openai.com/v1", "protocol":"open_ai_responses", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"gpt-5","request_model":"gpt-5","billing_model":"gpt-5","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":true}}]
        }]})).unwrap();
        let client = ModelRuntime::from_config(config).unwrap();
        let mut request = LlmRequest::new("gpt-5").with_user_text("test");
        request.input.service_tier = Some(lingxi_llm_client::protocol::ServiceTier::Fast);
        let prepared = client.prepare(&request).await.unwrap();
        // This configured SDK catalog row uses FastWire::Fast. The host must
        // preserve the SDK choice instead of removing the selected tier.
        assert_eq!(prepared.provider_request.body_json["service_tier"], "fast");
    }

    #[tokio::test]
    async fn configuration_cache_is_reused_without_caching_request_credentials() {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({"providers":[{
            "provider_id":"anthropic_first_party", "profile_name":"test", "base_url":"https://api.anthropic.com", "protocol":"anthropic_messages", "auth":"api_key", "credential":{"type":"host_managed","id":"key"},
            "models":[{"display_model":"claude-test","request_model":"claude-test","billing_model":"claude-test","capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        let client = ModelRuntime::from_config(config).unwrap();
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
