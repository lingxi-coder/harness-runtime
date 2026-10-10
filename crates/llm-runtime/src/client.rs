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

fn credential_scope(entry: &RouteEntry, profile_name: &str) -> CredentialScope {
    let scope = CredentialScope::new(entry.provider_id.clone(), profile_name);
    match &entry.credential {
        CredentialConfig::HostManaged { id } | CredentialConfig::Static { id } => {
            scope.with_credential_id(id.clone())
        }
        CredentialConfig::None | CredentialConfig::Env { .. } => scope,
    }
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
                | WireAuth::ChatGptPlan
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
            .authenticate_wire(
                &profile.profile_name,
                request,
                std::time::SystemTime::now(),
                None,
            )
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

    pub(crate) fn fast_account_identity(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::model::fast_admission::Account, LlmError> {
        let route = self.registry.resolve_in(model, profile)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let first_party = route.provider_id == ProviderId::AnthropicFirstParty
            && entry.protocol == ProtocolFamily::AnthropicMessages
            && entry.base_url.trim_end_matches('/') == "https://api.anthropic.com";
        let default_model = self
            .registry
            .resolve_in("opus", Some(&route.profile_name))
            .map(|value| value.request_model)
            .unwrap_or_else(|_| "opus".into());
        Ok(crate::model::fast_admission::Account {
            profile: route.profile_name,
            base_url: entry.base_url.clone(),
            model: route.request_model,
            default_model,
            first_party,
            credential: None,
            alternate_api_key: None,
            oauth_scope: None,
        })
    }

    /// Whether the resolved route uses the native first-party provider, wire
    /// and endpoint. Profile names and model-family guesses are not authority.
    pub fn is_first_party_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<bool, LlmError> {
        self.fast_account_identity(model, profile)
            .map(|account| account.first_party)
    }

    pub(crate) async fn load_fast_account(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::model::fast_admission::Account, LlmError> {
        let mut account = self.fast_account_identity(model, profile)?;
        if account.first_party {
            let entry = self
                .routes
                .get(&account.profile)
                .ok_or(LlmError::ModelUnavailable)?;
            let mut scope = CredentialScope::new(entry.provider_id.clone(), &account.profile);
            if let CredentialConfig::HostManaged { id } | CredentialConfig::Static { id } =
                &entry.credential
            {
                scope = scope.with_credential_id(id);
            }
            let mut snapshot = match &self.credentials {
                Some(provider) => provider.anthropic_auth_snapshot(&scope).await,
                None => Ok(crate::AnthropicAuthSnapshot::default()),
            }
            .unwrap_or_default();
            if let CredentialConfig::Env { var } = &entry.credential {
                // An environment key selects model auth without suppressing
                // the host's independent status OAuth snapshot.
                snapshot.api_key = EnvCredentialProvider::new(var)
                    .anthropic_auth_snapshot(&scope)
                    .await
                    .unwrap_or_default()
                    .api_key;
            }
            account.alternate_api_key = snapshot.api_key.and_then(|credential| match credential {
                Credential::ApiKey(key) => Some(key),
                _ => None,
            });
            if let Some((oauth_scope, credential @ Credential::AnthropicOAuth { .. })) =
                snapshot.oauth
            {
                account.credential = Some(credential);
                account.oauth_scope = Some(oauth_scope);
            } else {
                account.credential = account.alternate_api_key.clone().map(Credential::ApiKey);
            }
        }
        Ok(account)
    }

    pub(crate) async fn refresh_fast_account(
        &self,
        account: &crate::model::fast_admission::Account,
    ) -> Result<Option<Credential>, LlmError> {
        let (Some(provider), Some(rejected), Some(scope)) =
            (&self.credentials, &account.credential, &account.oauth_scope)
        else {
            return Ok(None);
        };
        provider.refresh(scope, rejected).await
    }

    /// Resolve current Fast admission without authentication or transport.
    pub(crate) fn fast_model_allowed(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<bool, LlmError> {
        let route = self.registry.resolve_in(model, profile)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        Ok(route_allows_first_party_fast_mode(&route, entry))
    }

    /// Resolve native effort inputs without encoding, credentials or transport.
    pub(crate) fn native_per_turn_effort_policy(
        &self,
        request: &LlmRequest,
    ) -> Result<
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicEffortPolicy>,
        LlmError,
    > {
        let route = self
            .registry
            .resolve_in(&request.input.model, request.profile.as_deref())?;
        if route.provider_id != ProviderId::AnthropicFirstParty
            || !lingxi_llm_client::providers::anthropic::supports_per_message_effort(
                &route.request_model,
            )
        {
            return Ok(None);
        }
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        Ok(crate::model::effort::prepare(
            request,
            entry.protocol,
            &route.provider_id,
            &entry.profile,
            &route.request_model,
        ))
    }

    pub(crate) fn native_api_system_route(&self, model: &str, profile: Option<&str>) -> bool {
        self.registry.resolve_in(model, profile).is_ok_and(|route| {
            route.provider_id == ProviderId::AnthropicFirstParty
                && self
                    .routes
                    .get(&route.profile_name)
                    .is_some_and(|entry| entry.protocol == ProtocolFamily::AnthropicMessages)
        })
    }

    pub(crate) fn effort_command_snapshot(
        &self,
        request: &LlmRequest,
    ) -> Result<Option<lingxi_core::host::effort::EffortCommandSnapshot>, LlmError> {
        let route = self
            .registry
            .resolve_in(&request.input.model, request.profile.as_deref())?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let protocol = self.protocol_for_model(&request.input.model, request.profile.as_deref())?;
        Ok(crate::model::effort::command_snapshot(
            request,
            protocol,
            &route.provider_id,
            &entry.profile,
            &route.request_model,
        ))
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
        let profiles = self.provider_service_profiles();
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
        .map(|services| services.with_credential_source(self.clone()))
    }

    /// Capture a configured profile's key/token through the original resolver.
    /// Signing and account-specific OAuth authentication cannot be represented
    /// by a lone service token and must keep their full SDK authenticator.
    pub async fn service_credential(
        &self,
        profile: &str,
    ) -> Result<lingxi_llm_client::protocol::Secret<String>, LlmError> {
        let entry = self.routes.get(profile).ok_or(LlmError::ModelUnavailable)?;
        if !matches!(
            entry.auth,
            AuthStrategy::ApiKey
                | AuthStrategy::Bearer
                | AuthStrategy::OAuthBearer
                | AuthStrategy::GcpToken
        ) {
            return Err(LlmError::UnsupportedCapability {
                capability: "this profile's authentication cannot be reduced to a service key/token"
                    .into(),
            });
        }
        let credential = self.load_credential(entry, profile).await?.ok_or_else(|| {
            LlmError::Authentication {
                message: "the selected service profile has no configured credential".into(),
            }
        })?;
        let token = match credential {
            Credential::ApiKey(token)
            | Credential::BearerToken(token)
            | Credential::AnthropicOAuth {
                access_token: token,
                ..
            } => token,
            Credential::ChatGptOAuth { .. } | Credential::AwsSigV4 { .. } => {
                return Err(LlmError::UnsupportedCapability {
                    capability: "the selected credential requires account headers or signing".into(),
                })
            }
        };
        if token.is_empty() {
            return Err(LlmError::Authentication {
                message: "the selected service credential is empty".into(),
            });
        }
        Ok(lingxi_llm_client::protocol::Secret::new(token))
    }
    /// Export exact independent-service profiles with their original authentication.
    /// This does not dispatch requests or read host credentials.
    pub fn provider_service_profiles(&self) -> Vec<lingxi_llm_client::protocol::ProviderProfile> {
        self.routes
            .values()
            .map(|entry| {
                let mut profile = entry.profile.clone();
                profile.auth = entry.auth;
                profile
            })
            .collect()
    }

    #[must_use]
    pub fn available_models(&self) -> Vec<ModelListing> {
        self.registry.available_models()
    }

    /// Inspect the credential source for the exact selected model/profile.
    /// Route resolution is shared with requests; authentication is not executed.
    pub async fn credential_source(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::CredentialSource, LlmError> {
        let route = self.registry.resolve_in(model, profile)?;
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        if entry.auth == AuthStrategy::None {
            return Ok(crate::CredentialSource::None);
        }
        let scope = credential_scope(entry, &route.profile_name);
        match &entry.credential {
            CredentialConfig::None => Ok(crate::CredentialSource::None),
            CredentialConfig::Env { var } => EnvCredentialProvider::new(var).source(&scope).await,
            CredentialConfig::Static { .. } | CredentialConfig::HostManaged { .. } => {
                match &self.credentials {
                    Some(provider) => provider.source(&scope).await,
                    None => Ok(crate::CredentialSource::None),
                }
            }
        }
    }

    /// Resolve the selected main route plus an optional same-profile vision delegate.
    pub fn resolve_media_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<MediaRoute, LlmError> {
        self.registry.resolve_media_route_in(model, profile)
    }

    /// Resolve the exact SDK profile used to project a persisted prompt source
    /// vector and its transient provider cache layout. This is a synchronous
    /// route lookup only; it does not prepare a request or touch credentials.
    pub fn prompt_cache_profile(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Option<lingxi_llm_client::protocol::ProviderProfile> {
        let route = self.registry.resolve_in(model, profile).ok()?;
        let entry = self.routes.get(&route.profile_name)?;
        Some(entry.profile.clone())
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
        // structured_output stay hard errors unless the explicit experimental
        // beta policy suppresses an automatic Anthropic JSON schema.)
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
        let host_failure = Arc::new(Mutex::new(None));
        let authenticator = Arc::new(
            crate::execution::HostAuthenticator::for_request(
                self.clone(),
                resolved_route.profile_name.clone(),
                None,
                host_failure.clone(),
            )
            .with_request_credentials(
                (resolved_route.provider_id == ProviderId::AnthropicFirstParty
                    && entry.protocol == ProtocolFamily::AnthropicMessages)
                    .then_some(request.execution.request_credentials.as_ref())
                    .flatten(),
            ),
        );
        let prompt_cache_scope = credential_scope(entry, &resolved_route.profile_name);
        // Fast mode is a first-party Anthropic request property, not a generic
        // Anthropic-wire feature. Resolve the route before encoding and strip
        // it for custom compatible endpoints, cloud transports, and models
        // without the canonical capability. Doing this before authentication
        // is essential for signed Bedrock/Vertex requests.
        let fast_mode_allowed = route_allows_first_party_fast_mode(&resolved_route, entry);
        let needs_speed_degrade = matches!(
            entry.protocol,
            ProtocolFamily::AnthropicMessages
                | ProtocolFamily::BedrockClaude
                | ProtocolFamily::VertexClaude
                | ProtocolFamily::FoundryClaude
        ) && request.input.service_tier
            == Some(lingxi_llm_client::protocol::ServiceTier::Fast)
            && !fast_mode_allowed;

        // Evaluate the native structured-output kill switch per selected route,
        // before capability validation and SDK encoding. Keep the caller's
        // schema intact so a fallback to another provider can still use it.
        let extra_body = if matches!(
            effective_protocol,
            ProtocolFamily::AnthropicMessages
                | ProtocolFamily::BedrockClaude
                | ProtocolFamily::VertexClaude
                | ProtocolFamily::FoundryClaude
        ) {
            crate::service::extra_body_object()?
        } else {
            None
        };
        let suppress_schema = crate::structured_output::disabled_for(
            request,
            effective_protocol,
            &resolved_route.request_model,
            resolved_route.capabilities.structured_output,
            extra_body.as_ref(),
        );
        let effort_policy = crate::model::effort::prepare(
            request,
            effective_protocol,
            &resolved_route.provider_id,
            &entry.profile,
            &resolved_route.request_model,
        );
        let mut owned: Option<LlmRequest> = None;
        if needs_reasoning_degrade
            || needs_speed_degrade
            || suppress_schema
            || effort_policy.is_some()
        {
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
            if suppress_schema {
                r.input.output_format = lingxi_llm_client::protocol::OutputFormat::Text;
            }
            if effort_policy.is_some() {
                if let Some(thinking) = r.input.thinking.as_mut() {
                    thinking.effort = None;
                    if *thinking == Default::default() {
                        r.input.thinking = None;
                    }
                }
            }
            owned = Some(r);
        }

        let mut computer_binding = None;
        let interactions = effective_protocol == ProtocolFamily::GeminiInteractions;
        if request.execution.computer_request || interactions {
            let credential = authenticator.captured_credential().await?;
            let account_required = interactions
                || request.execution.computer_native
                || request.input.continuation.is_some()
                || request.execution.expected_computer_binding.is_some()
                || request.execution.computer_submission.is_some();
            if credential.is_some() || account_required {
                let account_scope = crate::computer::credential_account_scope(credential.as_ref())?;
                if request
                    .input
                    .continuation
                    .as_ref()
                    .is_some_and(|reference| reference.account_scope != account_scope)
                {
                    return Err(LlmError::InvalidRequest {
                        message:
                            "native computer continuation belongs to a different captured account"
                                .into(),
                    });
                }
                let binding = lingxi_core::host::NativeContinuationBinding {
                    account: account_scope.clone(),
                    profile: resolved_route.profile_name.clone(),
                    model: resolved_route.request_model.clone(),
                    endpoint: lingxi_llm_client::files::provider_file_endpoint_fingerprint(
                        &entry.base_url,
                    ),
                    protocol: serde_json::to_value(effective_protocol)
                        .map_err(|error| LlmError::InvalidRequest {
                            message: error.to_string(),
                        })?
                        .as_str()
                        .expect("protocol serializes as string")
                        .into(),
                };
                if request
                    .execution
                    .expected_computer_binding
                    .as_ref()
                    .is_some_and(|expected| expected != &binding)
                {
                    return Err(LlmError::InvalidRequest { message: "native computer receipt does not match the current account, profile, model, endpoint or protocol".into() });
                }
                if request.execution.computer_request {
                    computer_binding = Some(binding);
                }
                owned
                    .get_or_insert_with(|| request.clone())
                    .execution
                    .account_scope = Some(account_scope);
            }
        }

        let mut prepared_prompt_cache = None;
        if let Some(context) = request.execution.prompt_cache.as_ref() {
            use lingxi_llm_client::providers::anthropic::system_prompt::{
                project_system_prompt, PromptCacheSubscriberState,
            };

            // The logical request may have been assembled before an account
            // change. Read the live Host generation immediately before the
            // request-scoped credential await, then again afterward. A stable
            // pair binds this call to the current account; a changed pair
            // keeps the earlier epoch so late responses cannot be attributed
            // to the new account using credential material captured mid-swap.
            let account_epoch_before = (context.current_account_epoch)();
            let subscriber = if context.native_bare_mode {
                PromptCacheSubscriberState::NotSubscriber
            } else if context.native_unix_socket {
                // Native resolves this path directly from the OAuth token env;
                // the selected Host credential scope does not prove that source.
                PromptCacheSubscriberState::Unknown
            } else if resolved_route.provider_id == ProviderId::AnthropicFirstParty
                && effective_protocol == ProtocolFamily::AnthropicMessages
                && entry.auth == AuthStrategy::OAuthBearer
                && entry.base_url.trim_end_matches('/') == "https://api.anthropic.com"
            {
                let selected_credential = authenticator.captured_credential().await?;
                match selected_credential.as_ref() {
                    Some(Credential::AnthropicOAuth {
                        access_token,
                        scopes,
                    }) if !access_token.is_empty()
                        && lingxi_llm_client::auth::oauth::anthropic::subscription_from_scopes(
                            scopes,
                        ) =>
                    {
                        PromptCacheSubscriberState::Subscriber
                    }
                    Some(Credential::AnthropicOAuth { .. }) | Some(_) | None => {
                        PromptCacheSubscriberState::NotSubscriber
                    }
                }
            } else if matches!(
                resolved_route.provider_id,
                ProviderId::Custom { .. } | ProviderId::OpenAICompatible { .. }
            ) || (resolved_route.provider_id == ProviderId::AnthropicFirstParty
                && effective_protocol == ProtocolFamily::AnthropicMessages
                && entry.auth == AuthStrategy::OAuthBearer)
            {
                PromptCacheSubscriberState::Unknown
            } else {
                PromptCacheSubscriberState::NotSubscriber
            };
            let account_epoch_after = (context.current_account_epoch)();
            let account_epoch_stale = account_epoch_before != account_epoch_after;
            let account_epoch = if account_epoch_stale {
                account_epoch_before
            } else {
                account_epoch_after
            };
            let is_subscriber = subscriber == PromptCacheSubscriberState::Subscriber;
            let mut cache_policy = context.policy.clone();
            cache_policy.prompt_cache_ttl_inputs.subscriber = subscriber;
            cache_policy.prompt_cache_ttl_inputs.is_using_overage = is_subscriber
                && !account_epoch_stale
                && (context.overage_for_scope)(&prompt_cache_scope, account_epoch);

            let request_for_encoding = owned.get_or_insert_with(|| request.clone());
            request_for_encoding.input.system.clear();
            request_for_encoding
                .input
                .prompt_cache
                .breakpoints
                .retain(|point| {
                    !matches!(
                        point.position,
                        lingxi_llm_client::protocol::CachePosition::System { .. }
                    )
                });
            let prefixed_system = context.native_system_prefix.as_ref().map(|(prefix, attribution)| {
                if request.execution.anthropic_request_kind == lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind::Main {
                    lingxi_llm_client::providers::anthropic::system_prompt::prepend_native_system_prefix(context.system.as_ref(), prefix, attribution, &entry.profile)
                } else {
                    lingxi_llm_client::providers::anthropic::system_prompt::prepend_native_attribution(context.system.as_ref(), prefix, attribution, &entry.profile)
                }
            });
            if let Some(system) = prefixed_system.as_ref().or(context.system.as_ref()) {
                let projection = project_system_prompt(
                    system,
                    Some(&entry.profile),
                    &request.input.model,
                    effective_protocol,
                    cache_policy,
                );
                request_for_encoding.input.system =
                    projection.iter().map(|item| item.block.clone()).collect();
                request_for_encoding.input.prompt_cache.breakpoints.extend(
                    projection
                        .into_iter()
                        .filter_map(|item| item.cache_breakpoint),
                );
            }
            let caching_enabled =
                lingxi_llm_client::providers::anthropic::system_prompt::prompt_caching_enabled(
                    &request.input.model,
                    effective_protocol,
                );
            lingxi_llm_client::providers::anthropic::system_prompt::apply_last_message_breakpoint(
                &mut request_for_encoding.input,
                caching_enabled,
            );
            prepared_prompt_cache = Some(PreparedPromptCacheContext {
                scope: prompt_cache_scope.clone(),
                account_epoch,
                account_epoch_stale,
                is_subscriber,
                pending_overage: context.pending_overage.clone(),
            });
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
        let (wire_draft, mut provider_request) = crate::execution::prepare(
            &self.cache,
            profile,
            entry.auth,
            &resolved_route,
            encoding_request,
            transport,
            authenticator.clone(),
            (
                if request.stream {
                    lingxi_llm_client::RequestMode::Stream
                } else {
                    lingxi_llm_client::RequestMode::Complete
                },
                fast_mode_allowed
                    .then_some(lingxi_llm_client::protocol::CapabilitySupport::Supported),
            ),
        )
        .await
        .map_err(|error| {
            host_failure
                .lock()
                .expect("host failure")
                .take()
                .unwrap_or(error)
        })?;
        provider_request.json_encoding =
            lingxi_llm_client::exact_json::JsonEncoding::for_protocol(effective_protocol);
        // Native Anthropic requests omit the default tool choice. Apply this
        // before authentication so the final body is also the signed body.
        if matches!(effective_protocol, ProtocolFamily::AnthropicMessages)
            && encoding_request.input.tool_choice == lingxi_llm_client::protocol::ToolChoice::Auto
        {
            if let Some(body) = provider_request.body_json.as_object_mut() {
                if body.get("tool_choice") == Some(&serde_json::json!({"type": "auto"})) {
                    body.shift_remove("tool_choice");
                }
            }
        }
        if authenticate {
            // Inspection and every later handshake/seal share this draft's
            // material. Only the initial inspection uses the caller's time;
            // SDK callbacks sign their final body at the current time.
            provider_request = Self::authenticate_at(
                &authenticator,
                &resolved_route.profile_name,
                provider_request,
                now,
            )
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
            request_session_id: request.execution.request_session_id.clone(),
            computer_binding,
            computer_submission: request.execution.computer_submission.clone(),
            server_fallback_lane: None,
            server_fallback: request.execution.server_fallback.clone(),
            server_fallback_betas: request
                .execution
                .thinking_recovery_scope
                .as_ref()
                .map(|scope| {
                    scope.server_fallback_betas(
                        &resolved_route.provider_id,
                        &resolved_route.profile_name,
                        effective_protocol,
                    )
                })
                .unwrap_or_default(),
            refusal_fallback_context: request.execution.refusal_fallback_context.clone(),
            prompt_cache: prepared_prompt_cache,
            thinking_display_probe: Default::default(),
            computed_beta_headers: Vec::new(),
            beta_rejection_state: request
                .execution
                .thinking_recovery_scope
                .as_ref()
                .map(|scope| scope.beta_rejections())
                .unwrap_or_default(),
            authenticator,
            fast_account_binding: None,
            host_failure,
            wire_draft: Some(wire_draft),
            wire_call: None,
            registered_attempt: request.execution.model_attempt.is_some(),
            anthropic_request_kind: request.execution.anthropic_request_kind,
            stream_fallback: request.execution.stream_fallback,
            extra_body,
            effort_policy,
            anthropic_context_management: request.execution.anthropic_context_management.clone(),
            native_thinking_display: request.execution.native_thinking_display.clone(),
            fast_mode_allowed,
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
        prepared.before_computer_submit().await?;
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
        prepared.before_computer_submit().await?;
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
            .request_mut()
            .url
            .clone_from(&prepared.provider_request.url);
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
        prepared.before_computer_submit().await?;
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
        let call = crate::execution::seal(draft, &host, None)
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
        let entry = self
            .routes
            .get(&route.profile_name)
            .ok_or(LlmError::ModelUnavailable)?;
        let mut owned;
        let request = if crate::structured_output::disabled_for(
            request,
            entry.protocol,
            &route.request_model,
            route.capabilities.structured_output,
            None,
        ) {
            owned = request.clone();
            owned.input.output_format = lingxi_llm_client::protocol::OutputFormat::Text;
            &owned
        } else {
            request
        };
        validate_capabilities(request, route.capabilities)?;
        if !matches!(entry.protocol, ProtocolFamily::AnthropicMessages) {
            return Err(LlmError::InvalidRequest {
                message: "count_tokens is only available on AnthropicMessages routes".into(),
            });
        }
        let failure = Arc::new(Mutex::new(None));
        let auth = Arc::new(crate::execution::HostAuthenticator::for_request(
            self.clone(),
            route.profile_name.clone(),
            None,
            failure.clone(),
        ));
        let (draft, mut host) = crate::execution::prepare(
            &self.cache,
            entry.profile.clone(),
            entry.auth,
            &route,
            request,
            transport,
            auth,
            (lingxi_llm_client::RequestMode::CountTokens, None),
        )
        .await?;
        let betas = crate::model::betas::assemble_beta_header(
            crate::model::betas::Provider::Anthropic,
            crate::model::betas::Endpoint::CountTokens,
            &crate::model::betas::BetaContext::for_model(route.request_model),
        );
        host.body_json["betas"] = serde_json::Value::Array(
            betas
                .split(',')
                .filter(|beta| !beta.is_empty())
                .map(|beta| serde_json::Value::String(beta.into()))
                .collect(),
        );
        lingxi_llm_client::providers::anthropic::request_policy::normalize_message_parameters(
            &mut host.body_json,
            &mut host.headers,
            &mut host.json_string_overrides,
            &mut host.url,
            lingxi_llm_client::RequestMode::CountTokens,
        )
        .map_err(crate::upstream::error)?;
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
            let call = crate::execution::seal(draft, &host, None)
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
        let auth = crate::execution::HostAuthenticator::live(self.clone(), None, failure.clone());
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
        let auth = crate::execution::HostAuthenticator::live(self.clone(), None, failure.clone());
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
        authenticator: &crate::execution::HostAuthenticator,
        profile_name: &str,
        mut request: ProviderRequest,
        now: std::time::SystemTime,
    ) -> Result<ProviderRequest, LlmError> {
        let mut wire = lingxi_llm_client::HttpRequest {
            http1_header_layout: None,
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
        authenticator
            .authenticate_at(profile_name, &mut wire, now)
            .await?;
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
        let scope = credential_scope(entry, profile_name);
        let credential = match &entry.credential {
            CredentialConfig::None => return Ok(None),
            CredentialConfig::Env { var } => {
                EnvCredentialProvider::new(var.clone()).load(&scope).await?
            }
            CredentialConfig::Static { .. } | CredentialConfig::HostManaged { .. } => {
                self.credentials
                    .as_ref()
                    .ok_or(LlmError::Authentication {
                        message: String::new(),
                    })?
                    .load(&scope)
                    .await?
            }
        };
        Ok(Some(credential))
    }

    pub(crate) async fn capture_request_credential(
        &self,
        snapshot: &crate::execution::RequestCredentialSnapshot,
    ) -> Result<Option<Credential>, LlmError> {
        let entry = self
            .routes
            .get(&snapshot.profile)
            .ok_or(LlmError::ModelUnavailable)?;
        if entry.auth == AuthStrategy::None {
            return Ok(None);
        }
        snapshot
            .credential
            .get_or_try_init(|| self.load_credential(entry, &snapshot.profile))
            .await
            .cloned()
    }
}

fn route_allows_first_party_fast_mode(route: &crate::ResolvedRoute, entry: &RouteEntry) -> bool {
    route.provider_id == ProviderId::AnthropicFirstParty
        && entry.protocol == ProtocolFamily::AnthropicMessages
        && entry.base_url.trim_end_matches('/') == "https://api.anthropic.com"
        && crate::model::fast::model_allowed(&route.request_model)
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
    pub(crate) request_session_id: Option<String>,
    pub(crate) computer_binding: Option<lingxi_core::host::NativeContinuationBinding>,
    pub(crate) computer_submission: Option<Arc<crate::computer::ComputerReceiptSubmission>>,
    pub(crate) server_fallback_lane:
        Option<lingxi_llm_client::providers::anthropic::fallback_request::ServerLane>,
    pub(crate) server_fallback:
        Option<lingxi_llm_client::providers::anthropic::fallback_request::RequestPolicy>,
    pub(crate) server_fallback_betas:
        lingxi_llm_client::providers::anthropic::fallback_request::ServerBetaState,
    pub(crate) refusal_fallback_context:
        Option<lingxi_core::host::refusal_driver::FallbackTargetContext>,
    /// Selected credential scope, subscriber fact and account generation used
    /// by the late prompt projection for response observations.
    pub(crate) prompt_cache: Option<PreparedPromptCacheContext>,
    pub(crate) thinking_display_probe:
        lingxi_llm_client::providers::anthropic::thinking_display::DisplayProbe,
    pub(crate) computed_beta_headers: Vec<String>,
    pub(crate) beta_rejection_state:
        lingxi_llm_client::providers::anthropic::beta_repair::ConversationBetaState,
    pub(crate) authenticator: Arc<crate::execution::HostAuthenticator>,
    pub(crate) fast_account_binding: Option<crate::model::fast_admission::Binding>,
    /// Native Fast admission captured before SDK validation and credential work.
    pub(crate) fast_mode_allowed: bool,
    pub(crate) anthropic_context_management:
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicContextManagement>,
    pub(crate) native_thinking_display:
        Option<lingxi_llm_client::providers::anthropic::thinking_display::ThinkingDisplayPolicy>,
    pub(crate) effort_policy:
        Option<lingxi_llm_client::providers::anthropic::request_policy::AnthropicEffortPolicy>,
    pub(crate) anthropic_request_kind:
        lingxi_llm_client::providers::anthropic::request_policy::AnthropicRequestKind,
    pub(crate) stream_fallback: bool,
    pub(crate) extra_body: Option<serde_json::Map<String, serde_json::Value>>,
    pub(crate) host_failure: crate::execution::HostFailure,
    pub(crate) wire_draft: Option<lingxi_llm_client::RequestDraft>,
    pub(crate) wire_call: Option<lingxi_llm_client::PreparedCall>,
    registered_attempt: bool,
    pub route: Route,
    pub provider_request: ProviderRequest,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedPromptCacheContext {
    pub scope: CredentialScope,
    /// Generation bound around the selected credential await. When stale is
    /// true, this remains the pre-await epoch so late observations are dropped.
    pub account_epoch: u64,
    pub account_epoch_stale: bool,
    pub is_subscriber: bool,
    pub pending_overage: Arc<Mutex<Option<crate::PendingPromptCacheObservation>>>,
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
        let call = crate::execution::seal(
            draft,
            &prepared.provider_request,
            Some(&prepared.route.resolved_route.provider_id),
        )
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
        http.url.clone_from(&prepared.provider_request.url);
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
        let mut callback_rejected = false;
        let mark = || {
            on_dispatch().map_err(|error| {
                callback_rejected = true;
                crate::execution::wire_error(error)
            })
        };
        prepared.before_computer_submit().await?;
        let received = if websocket {
            session.dispatch(call, mark).await
        } else {
            call.dispatch_once_with(mark).await
        };
        if callback_rejected {
            if let Some(submission) = prepared.computer_submission.as_ref() {
                submission.not_submitted().await?;
            }
        }
        let received = received.map_err(crate::upstream::error)?;
        Ok((prepared, received))
    }
}

impl ModelRuntime {
    pub(crate) async fn authenticate_wire(
        &self,
        profile: &str,
        request: &mut lingxi_llm_client::HttpRequest,
        now: std::time::SystemTime,
        snapshot: Option<&crate::execution::RequestCredentialSnapshot>,
    ) -> Result<(), LlmError> {
        use lingxi_llm_client::auth::{apply_credential, ClientIdentity, CredentialRef};
        if snapshot.is_some_and(|snapshot| snapshot.profile != profile) {
            return Err(LlmError::Authentication {
                message: "credential snapshot belongs to another provider profile".into(),
            });
        }
        let entry = self.routes.get(profile).ok_or(LlmError::ModelUnavailable)?;
        if entry.auth == AuthStrategy::None {
            return Ok(());
        }
        let live;
        let credential = if let Some(snapshot) = snapshot {
            snapshot
                .credential
                .get_or_try_init(|| self.load_credential(entry, profile))
                .await?
        } else {
            live = self.load_credential(entry, profile).await?;
            &live
        };
        let Some(credential) = credential else {
            return Ok(());
        };
        let material = match credential {
            Credential::ApiKey(secret) | Credential::BearerToken(secret) => {
                CredentialRef::Token(secret)
            }
            Credential::AnthropicOAuth { access_token, .. } => CredentialRef::Token(access_token),
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
    pub(crate) async fn before_computer_submit(&self) -> Result<(), LlmError> {
        if let Some(submission) = self.computer_submission.as_ref() {
            submission.before_submit().await?;
        }
        Ok(())
    }
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
#[path = "client_auth_snapshot_tests.rs"]
mod auth_snapshot_tests;

#[cfg(test)]
mod shared_client_regression {
    use super::*;

    #[derive(Debug)]
    struct MetadataOnlyProvider;

    impl CredentialProvider for MetadataOnlyProvider {
        fn source<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> crate::BoxFuture<'a, Result<crate::CredentialSource, LlmError>> {
            Box::pin(async move {
                assert_eq!(
                    scope.credential_id.as_deref(),
                    Some(scope.profile_name.as_str())
                );
                Ok(crate::CredentialSource::Environment {
                    variable: format!("{}_KEY", scope.profile_name),
                })
            })
        }

        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
            panic!("credential source inspection must not load or refresh credentials");
        }
    }

    #[tokio::test]
    async fn credential_source_uses_exact_profile_and_never_executes_authentication() {
        let profiles = ["alpha", "beta"].into_iter().map(|profile| {
            serde_json::json!({
                "provider_id":"open_ai", "profile_name":profile,
                "base_url":"https://api.openai.com/v1", "protocol":"open_ai_responses",
                "auth":"api_key", "credential":{"type":"host_managed","id":profile},
                "models":[{"display_model":"shared-model","request_model":"shared-model","billing_model":"shared-model",
                    "capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
            })
        }).collect::<Vec<_>>();
        let config = serde_json::from_value(serde_json::json!({"providers":profiles})).unwrap();
        let client = ModelRuntime::from_config(config)
            .unwrap()
            .with_credential_provider(Arc::new(MetadataOnlyProvider));
        for profile in ["alpha", "beta"] {
            assert_eq!(
                client
                    .credential_source("shared-model", Some(profile))
                    .await
                    .unwrap(),
                crate::CredentialSource::Environment {
                    variable: format!("{profile}_KEY")
                }
            );
        }
        assert!(client
            .credential_source("shared-model", Some("missing"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn credential_source_without_provider_metadata_is_unknown_not_absent() {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({"providers":[{
            "provider_id":"open_ai", "profile_name":"snapshot", "base_url":"https://api.openai.com/v1",
            "protocol":"open_ai_responses", "auth":"api_key", "credential":{"type":"host_managed","id":"snapshot-key"},
            "models":[{"display_model":"shared-model","request_model":"shared-model","billing_model":"shared-model",
                "capabilities":{"streaming":true,"tools":false,"vision":false,"documents":false,"reasoning":false,"structured_output":false}}]
        }]})).unwrap();
        #[derive(Debug)]
        struct UnsupportedMetadata;
        impl CredentialProvider for UnsupportedMetadata {
            fn load<'a>(
                &'a self,
                _: &'a CredentialScope,
            ) -> crate::BoxFuture<'a, Result<Credential, LlmError>> {
                panic!("metadata must not execute the provider");
            }
        }
        let client = ModelRuntime::from_config(config)
            .unwrap()
            .with_credential_provider(Arc::new(UnsupportedMetadata));
        assert_eq!(
            client
                .credential_source("shared-model", Some("snapshot"))
                .await
                .unwrap(),
            crate::CredentialSource::Unknown
        );
    }

    #[tokio::test]
    async fn canonical_openai_controls_survive_host_anthropic_wire_policy() {
        let config: ClientConfig = serde_json::from_value(serde_json::json!({"providers":[{
            "provider_id":"open_ai", "profile_name":"openai", "base_url":"https://api.openai.com/v1", "protocol":"open_ai_responses", "auth":"none", "credential":{"type":"none"},
            "models":[{"display_model":"gpt-5","request_model":"gpt-5","billing_model":"gpt-5","capabilities":{"streaming":true,"tools":true,"vision":false,"documents":false,"reasoning":true,"structured_output":true}}]
        }]})).unwrap();
        let client = ModelRuntime::from_config(config).unwrap();
        let mut request = LlmRequest::new("gpt-5").with_user_text("test");
        request.input.service_tier = Some(lingxi_llm_client::protocol::ServiceTier::Fast);
        request.input.tools.push(
            serde_json::from_value(serde_json::json!({
                "name": "Read",
                "description": "Read a file",
                "input_schema": {"type": "object", "properties": {}}
            }))
            .unwrap(),
        );
        let prepared = client.prepare(&request).await.unwrap();
        // This configured SDK catalog row uses FastWire::Fast. The host must
        // preserve the SDK choice instead of removing the selected tier.
        assert_eq!(prepared.provider_request.body_json["service_tier"], "fast");
        assert_eq!(prepared.provider_request.body_json["tool_choice"], "auto");
        assert_eq!(
            prepared.provider_request.body_json["tools"][0]["name"],
            "Read"
        );
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
