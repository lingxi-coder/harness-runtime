use client::protocol::listings::{ModelDetailsDto, ProviderModelCatalogEntryDto};
use llm_runtime::oauth::anthropic::handle::OAuthHandle;
use llm_runtime::oauth::anthropic::{OAuthCredentialProvider, RefreshDriver};
use llm_runtime::oauth::openai as openai_oauth;
use llm_runtime::{Credential, CredentialConfig, CredentialProvider, CredentialScope, ProviderId};
use platform_api::http::HttpError;
use platform_api::{AuthHandle, HttpTransport};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{lower_model_details, MobileConfig, MobileEngineError};

/// Non-secret result of testing one provider endpoint from the mobile engine.
///
/// The engine performs the request so an already-saved credential never has to
/// cross back into Swift/Kotlin. A caller may supply an unsaved draft credential
/// for a one-off test; it is used only for this request and is never persisted.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConnectionTestDto {
    /// The endpoint authenticated successfully and the selected model is usable
    /// (or the provider returned no machine-readable model catalog).
    pub connected: bool,
    /// A server returned an HTTP response, even if authentication or the model
    /// check failed.
    pub reachable: bool,
    /// Authentication passed. This remains false when a rate limiter or proxy
    /// rejected the request before credentials could be verified.
    pub authenticated: bool,
    /// Whether the selected model appeared in a recognized model-list payload.
    pub model_available: bool,
    /// HTTP status when the provider responded.
    pub http_status: Option<u16>,
    /// End-to-end request duration, rounded down to milliseconds.
    pub latency_ms: u64,
    /// Log-safe user-facing detail. Provider response bodies and credentials are
    /// deliberately excluded.
    pub message: String,
    /// True when the credential came from the shared encrypted store; false for
    /// a one-off draft supplied by the settings form.
    pub used_stored_credential: bool,
}

/// Credential-free metadata used by mobile settings to render the same
/// provider choices the engine can actually assemble.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderCatalogEntryDto {
    pub profile_id: String,
    pub display_name: String,
    pub base_url: String,
    pub protocol: String,
    pub auth: String,
    pub credential_env: Option<String>,
    pub models: Vec<String>,
    pub model_details: Vec<ModelDetailsDto>,
}

/// Native OAuth authorization session returned to iOS/Android. The verifier
/// and state never cross the FFI boundary.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOAuthSessionDto {
    pub provider: String,
    pub flow_id: String,
    pub authorization_url: String,
    pub callback_url_scheme: String,
}

/// Non-secret OAuth status for a provider.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileOAuthStateDto {
    pub provider: String,
    pub signed_in: bool,
    pub account_label: Option<String>,
    pub account_id: Option<String>,
    pub organization_id: Option<String>,
    pub fedramp: bool,
}

pub(super) fn builtin_provider_catalog() -> Vec<ProviderCatalogEntryDto> {
    let to_entry = |provider: llm_runtime::ProviderProfile| {
        let display_name = if provider.profile_name == "anthropic" {
            "Anthropic".to_string()
        } else {
            provider.profile_name.clone()
        };
        let credential_env = match &provider.credential {
            CredentialConfig::Env { var } => Some(var.clone()),
            _ => None,
        };
        let listings = model_listings(std::slice::from_ref(&provider));
        let curated = listings
            .into_iter()
            .filter(|listing| {
                platform_api::is_curated_model(&listing.provider_id, &listing.request_model)
                    || !platform_api::provider_has_curated_list(&listing.provider_id)
            })
            .collect::<Vec<_>>();
        ProviderCatalogEntryDto {
            profile_id: provider.profile_name.clone(),
            display_name,
            base_url: provider.base_url,
            protocol: format!("{:?}", provider.protocol),
            auth: format!("{:?}", provider.auth),
            credential_env,
            models: curated
                .iter()
                .map(|listing| listing.request_model.clone())
                .collect(),
            model_details: curated.iter().map(lower_model_details).collect(),
        }
    };

    let anthropic = llm_runtime::anthropic_provider_profile(
        ANTHROPIC_OAUTH_API_BASE,
        llm_runtime::AuthStrategy::ApiKey,
        CredentialConfig::Env {
            var: "ANTHROPIC_API_KEY".to_string(),
        },
    );
    let mut entries = vec![to_entry(anthropic)];
    entries.extend(
        llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .map(to_entry),
    );
    entries
}

pub(super) fn provider_model_catalog_from_listings(
    listings: &[platform_api::ModelListing],
) -> Vec<ProviderModelCatalogEntryDto> {
    platform_api::provider_model_catalog(listings)
        .iter()
        .map(client::adapter::lowering::lower_provider_model_catalog_entry)
        .collect()
}

/// Tag an OAuth failure with the STAGE it happened in, and log it.
///
/// Every OAuth failure used to collapse into a bare
/// `MobileEngineError::Internal(String)` that iOS renders through
/// `error.localizedDescription`, so "the login failed" could equally mean the
/// authorize URL was rejected, the callback never arrived, the token exchange
/// 400'd, or the keychain write failed — four very different bugs sharing one
/// indistinguishable message.
///
/// The `oauth/<provider>/<stage>: ` prefix is machine-readable and cheap.
/// `MobileEngineError` deliberately gains no new variant: it derives
/// `uniffi::Error`, so a new case would change the generated Swift enum.
pub(super) fn oauth_err(
    provider: &str,
    stage: &str,
    detail: impl std::fmt::Display,
) -> MobileEngineError {
    tracing::warn!(
        target: "lingxi::mobile_oauth",
        provider,
        stage,
        error = %detail,
        "oauth stage failed",
    );
    MobileEngineError::Internal(format!("oauth/{provider}/{stage}: {detail}"))
}

pub(super) const IOS_OAUTH_REDIRECT_URI: &str = "lingxi://oauth/callback";

pub(super) const IOS_OAUTH_CALLBACK_SCHEME: &str = "lingxi";

pub(super) const MOBILE_OAUTH_SESSION_TTL: std::time::Duration =
    std::time::Duration::from_secs(10 * 60);

pub(super) const ANTHROPIC_OAUTH_API_BASE: &str = "https://api.anthropic.com";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MobileOAuthProvider {
    Anthropic,
    OpenAi,
}

impl MobileOAuthProvider {
    pub(super) fn parse(value: &str) -> Result<Self, MobileEngineError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "anthropic" => Ok(Self::Anthropic),
            "openai" | "openai-chatgpt" => Ok(Self::OpenAi),
            _ => Err(MobileEngineError::Internal(
                "unsupported OAuth provider".to_string(),
            )),
        }
    }

    pub(super) fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai-chatgpt",
        }
    }
}

pub(super) struct PendingMobileOAuthSession {
    pub(super) provider: MobileOAuthProvider,
    pub(super) flow_id: String,
    pub(super) verifier: String,
    pub(super) state: String,
    pub(super) redirect_uri: String,
    pub(super) expires_at: std::time::Instant,
}

pub(super) fn parse_mobile_oauth_callback(
    callback_url: &str,
) -> Result<(String, String), MobileEngineError> {
    let callback = url::Url::parse(callback_url)
        .map_err(|_| MobileEngineError::Internal("invalid OAuth callback URL".to_string()))?;
    if callback.scheme() != IOS_OAUTH_CALLBACK_SCHEME
        || callback.host_str() != Some("oauth")
        || callback.path() != "/callback"
    {
        return Err(MobileEngineError::Internal(
            "invalid OAuth callback destination".to_string(),
        ));
    }
    let params: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
    let code = params
        .get("code")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| MobileEngineError::Internal("OAuth callback has no code".to_string()))?;
    let state = params
        .get("state")
        .filter(|value| !value.is_empty())
        .cloned()
        .ok_or_else(|| MobileEngineError::Internal("OAuth callback has no state".to_string()))?;
    Ok((code, state))
}

pub(super) fn validate_mobile_oauth_session(
    session: &PendingMobileOAuthSession,
    flow_id: &str,
    returned_state: &str,
) -> Result<(), MobileEngineError> {
    if std::time::Instant::now() >= session.expires_at {
        return Err(oauth_err(
            session.provider.id(),
            "session_expired",
            "the login was not completed before the session timed out",
        ));
    }
    if session.flow_id != flow_id || session.state != returned_state {
        return Err(oauth_err(
            session.provider.id(),
            "state_mismatch",
            "the callback did not echo this flow's CSRF state",
        ));
    }
    Ok(())
}

pub(super) fn take_mobile_oauth_session(
    pending: &mut Option<PendingMobileOAuthSession>,
    flow_id: &str,
    returned_state: &str,
) -> Result<PendingMobileOAuthSession, MobileEngineError> {
    let session = pending.as_ref().ok_or_else(|| {
        oauth_err(
            "unknown",
            "session_missing",
            "no OAuth login is in progress for this callback",
        )
    })?;
    if std::time::Instant::now() >= session.expires_at {
        // Read the provider off the borrow before clearing the slot.
        let provider = session.provider.id();
        *pending = None;
        return Err(oauth_err(
            provider,
            "session_expired",
            "the login was not completed before the session timed out",
        ));
    }
    validate_mobile_oauth_session(session, flow_id, returned_state)?;
    let session = PendingMobileOAuthSession {
        provider: session.provider,
        flow_id: session.flow_id.clone(),
        verifier: session.verifier.clone(),
        state: session.state.clone(),
        redirect_uri: session.redirect_uri.clone(),
        expires_at: session.expires_at,
    };
    // Consume the flow before network I/O. A code exchange, profile lookup,
    // or secure-store failure is terminal for this callback; leaving it
    // pending would block every subsequent login until TTL.
    *pending = None;
    Ok(session)
}

/// Mobile OAuth facade shared by iOS and Android. Provider-specific OAuth
/// implementations stay in `llm-runtime`; this type only owns callback state,
/// validates the custom-scheme return, and lowers identity metadata.
pub struct MobileOAuthManager {
    pub(super) anthropic: Arc<OAuthHandle>,
    pub(super) openai: Arc<openai_oauth::OpenAiOAuthHandle>,
    pub(super) anthropic_refresh: Option<Arc<RefreshDriver>>,
    pub(super) openai_refresh: Option<Arc<openai_oauth::RefreshDriver>>,
    pub(super) anthropic_refresh_spawner: Option<Arc<dyn platform_api::RuntimeSpawner>>,
    pub(super) openai_refresh_spawner: Option<Arc<dyn platform_api::RuntimeSpawner>>,
    pub(super) http: Arc<dyn HttpTransport>,
    pub(super) pending: Mutex<Option<PendingMobileOAuthSession>>,
}

impl MobileOAuthManager {
    pub(super) fn new(
        anthropic: Arc<OAuthHandle>,
        openai: Arc<openai_oauth::OpenAiOAuthHandle>,
        anthropic_refresh: Option<Arc<RefreshDriver>>,
        openai_refresh: Option<Arc<openai_oauth::RefreshDriver>>,
        anthropic_refresh_spawner: Option<Arc<dyn platform_api::RuntimeSpawner>>,
        openai_refresh_spawner: Option<Arc<dyn platform_api::RuntimeSpawner>>,
        http: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            anthropic,
            openai,
            anthropic_refresh,
            openai_refresh,
            anthropic_refresh_spawner,
            openai_refresh_spawner,
            http,
            pending: Mutex::new(None),
        }
    }

    pub(super) async fn begin(
        &self,
        provider: String,
        redirect_uri: String,
    ) -> Result<MobileOAuthSessionDto, MobileEngineError> {
        if redirect_uri != IOS_OAUTH_REDIRECT_URI {
            return Err(oauth_err(
                "unknown",
                "redirect_uri",
                format!("host supplied an unexpected redirect URI: {redirect_uri}"),
            ));
        }
        let provider = MobileOAuthProvider::parse(&provider)?;
        let mut pending = self.pending.lock().await;
        // Evict an abandoned flow before refusing on conflict. Only
        // `validate_`/`take_mobile_oauth_session` checked `expires_at`, so a
        // login the user backgrounded (no `cancel_o_auth`) left the slot
        // occupied and every retry failed for the whole
        // `MOBILE_OAUTH_SESSION_TTL`.
        if pending
            .as_ref()
            .is_some_and(|session| std::time::Instant::now() >= session.expires_at)
        {
            *pending = None;
        }
        if pending.is_some() {
            return Err(oauth_err(
                provider.id(),
                "session_conflict",
                "another OAuth login is already in progress",
            ));
        }
        let (authorization_url, verifier, state) = match provider {
            MobileOAuthProvider::Anthropic => {
                self.anthropic.begin_mobile_browser_login(&redirect_uri)
            }
            MobileOAuthProvider::OpenAi => self.openai.begin_mobile_browser_login(&redirect_uri),
        };
        // Log the URL actually opened. It carries no secret — the PKCE
        // *challenge* is public by construction and the verifier never leaves
        // Rust — and it is the only way to tell an authorize-page rejection
        // apart from a client-side bug without rebuilding the app.
        tracing::info!(
            target: "lingxi::mobile_oauth",
            provider = provider.id(),
            url = %authorization_url,
            "opening the authorize URL",
        );
        let flow_id = uuid::Uuid::new_v4().to_string();
        *pending = Some(PendingMobileOAuthSession {
            provider,
            flow_id: flow_id.clone(),
            verifier,
            state,
            redirect_uri,
            expires_at: std::time::Instant::now() + MOBILE_OAUTH_SESSION_TTL,
        });
        Ok(MobileOAuthSessionDto {
            provider: provider.id().to_string(),
            flow_id,
            authorization_url,
            callback_url_scheme: IOS_OAUTH_CALLBACK_SCHEME.to_string(),
        })
    }

    pub(super) async fn complete(
        &self,
        flow_id: String,
        callback_url: String,
    ) -> Result<MobileOAuthStateDto, MobileEngineError> {
        let (code, returned_state) = parse_mobile_oauth_callback(&callback_url)?;

        let session = {
            let mut pending = self.pending.lock().await;
            take_mobile_oauth_session(&mut pending, &flow_id, &returned_state)?
        };

        match session.provider {
            MobileOAuthProvider::Anthropic => self
                .anthropic
                .complete_mobile_browser_login(
                    &code,
                    &session.verifier,
                    &session.state,
                    &session.redirect_uri,
                )
                .await
                .map(|info| MobileOAuthStateDto {
                    provider: session.provider.id().to_string(),
                    signed_in: true,
                    account_label: Some(info.email),
                    account_id: None,
                    organization_id: Some(info.org_id),
                    fedramp: false,
                })
                .map_err(|error| oauth_err(session.provider.id(), "exchange", error)),
            MobileOAuthProvider::OpenAi => self
                .openai
                .complete_mobile_browser_login(&code, &session.verifier, &session.redirect_uri)
                .await
                .map(|info| MobileOAuthStateDto {
                    provider: session.provider.id().to_string(),
                    signed_in: true,
                    account_label: info.account_id.clone(),
                    account_id: info.account_id,
                    organization_id: None,
                    fedramp: info.fedramp,
                })
                .map_err(|error| oauth_err(session.provider.id(), "exchange", error)),
        }
    }

    pub(super) async fn cancel(&self, flow_id: String) {
        let mut pending = self.pending.lock().await;
        if pending
            .as_ref()
            .is_some_and(|value| value.flow_id == flow_id)
        {
            *pending = None;
        }
    }

    pub(super) async fn logout(&self, provider: String) -> Result<(), MobileEngineError> {
        let provider = MobileOAuthProvider::parse(&provider)?;
        {
            let mut pending = self.pending.lock().await;
            if pending
                .as_ref()
                .is_some_and(|value| value.provider == provider)
            {
                *pending = None;
            }
        }
        match provider {
            MobileOAuthProvider::Anthropic => {
                self.anthropic.logout().await.map_err(|error| {
                    MobileEngineError::Internal(format!("OAuth logout failed: {error}"))
                })?;
                if let (Some(driver), Some(spawner)) =
                    (&self.anthropic_refresh, &self.anthropic_refresh_spawner)
                {
                    driver.invalidate(spawner.as_ref()).await;
                }
                Ok(())
            }
            MobileOAuthProvider::OpenAi => {
                self.openai.logout().await.map_err(|error| {
                    MobileEngineError::Internal(format!("OAuth logout failed: {error}"))
                })?;
                if let (Some(driver), Some(spawner)) =
                    (&self.openai_refresh, &self.openai_refresh_spawner)
                {
                    driver.invalidate(spawner.as_ref()).await;
                }
                Ok(())
            }
        }
    }

    pub(super) async fn state(
        &self,
        provider: String,
    ) -> Result<MobileOAuthStateDto, MobileEngineError> {
        match MobileOAuthProvider::parse(&provider)? {
            MobileOAuthProvider::Anthropic => Ok(match self.anthropic.current_user().await {
                Some(info) => MobileOAuthStateDto {
                    provider: MobileOAuthProvider::Anthropic.id().to_string(),
                    signed_in: true,
                    account_label: Some(info.email),
                    account_id: None,
                    organization_id: Some(info.org_id),
                    fedramp: false,
                },
                None => MobileOAuthStateDto {
                    provider: MobileOAuthProvider::Anthropic.id().to_string(),
                    signed_in: false,
                    account_label: None,
                    account_id: None,
                    organization_id: None,
                    fedramp: false,
                },
            }),
            MobileOAuthProvider::OpenAi => Ok(match self.openai.current_user().await {
                Some(info) => MobileOAuthStateDto {
                    provider: MobileOAuthProvider::OpenAi.id().to_string(),
                    signed_in: true,
                    account_label: info.account_id.clone(),
                    account_id: info.account_id,
                    organization_id: None,
                    fedramp: info.fedramp,
                },
                None => MobileOAuthStateDto {
                    provider: MobileOAuthProvider::OpenAi.id().to_string(),
                    signed_in: false,
                    account_label: None,
                    account_id: None,
                    organization_id: None,
                    fedramp: false,
                },
            }),
        }
    }

    /// Probe OAuth-backed provider metadata without issuing an inference call.
    pub(super) async fn test(
        &self,
        provider: String,
        api_base: String,
        model: String,
    ) -> ProviderConnectionTestDto {
        let provider = match MobileOAuthProvider::parse(&provider) {
            Ok(provider) => provider,
            Err(_) => {
                return provider_connection_failure(
                    "OAuth Provider 标识无效",
                    false,
                    false,
                    None,
                    0,
                    true,
                );
            }
        };
        let (token, account_id, fedramp) = match provider {
            MobileOAuthProvider::Anthropic => {
                let Some(driver) = &self.anthropic_refresh else {
                    return provider_connection_failure(
                        "请先登录 Anthropic OAuth",
                        false,
                        false,
                        None,
                        0,
                        true,
                    );
                };
                let credential = OAuthCredentialProvider::new(driver.clone())
                    .load(&CredentialScope::new(
                        ProviderId::AnthropicFirstParty,
                        "anthropic",
                    ))
                    .await;
                match credential {
                    Ok(Credential::BearerToken(token)) => (token, None, false),
                    _ => {
                        return provider_connection_failure(
                            "Anthropic OAuth 会话已失效，请重新登录",
                            false,
                            false,
                            None,
                            0,
                            true,
                        );
                    }
                }
            }
            MobileOAuthProvider::OpenAi => {
                let Some(driver) = &self.openai_refresh else {
                    return provider_connection_failure(
                        "请先登录 ChatGPT OAuth",
                        false,
                        false,
                        None,
                        0,
                        true,
                    );
                };
                let credential = openai_oauth::OpenAiOAuthCredentialProvider::new(driver.clone())
                    .load(&CredentialScope::new(
                        ProviderId::OpenAICompatible {
                            name: "openai-chatgpt".to_string(),
                        },
                        "openai-chatgpt",
                    ))
                    .await;
                match credential {
                    Ok(Credential::ChatGptOAuth {
                        access_token,
                        account_id,
                        fedramp,
                    }) => (access_token, account_id, fedramp),
                    _ => {
                        return provider_connection_failure(
                            "ChatGPT OAuth 会话已失效，请重新登录",
                            false,
                            false,
                            None,
                            0,
                            true,
                        );
                    }
                }
            }
        };
        let endpoint = match provider {
            MobileOAuthProvider::Anthropic => {
                let configured_base = api_base.trim().trim_end_matches('/');
                if configured_base != ANTHROPIC_OAUTH_API_BASE {
                    return provider_connection_failure(
                        "Anthropic OAuth 仅支持官方 HTTPS API 地址",
                        false,
                        false,
                        None,
                        0,
                        true,
                    );
                }
                provider_models_endpoint(ANTHROPIC_OAUTH_API_BASE, "anthropic")
            }
            MobileOAuthProvider::OpenAi => Ok("https://chatgpt.com/backend-api/models".to_string()),
        };
        let endpoint = match endpoint {
            Ok(endpoint) => endpoint,
            Err(message) => {
                return provider_connection_failure(message, false, false, None, 0, true);
            }
        };
        let mut headers = vec![
            ("accept".to_string(), "application/json".to_string()),
            ("authorization".to_string(), format!("Bearer {token}")),
        ];
        match provider {
            MobileOAuthProvider::Anthropic => {
                headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
                headers.push(("anthropic-beta".to_string(), "oauth-2025-04-20".to_string()));
            }
            MobileOAuthProvider::OpenAi => {
                if let Some(account_id) = account_id {
                    headers.push(("ChatGPT-Account-ID".to_string(), account_id));
                }
                if fedramp {
                    headers.push(("X-OpenAI-Fedramp".to_string(), "true".to_string()));
                }
            }
        }
        let started = std::time::Instant::now();
        let response = self
            .http
            .request(protocol::HttpRequest {
                method: protocol::HttpMethod::Get,
                url: endpoint,
                headers,
                body: None,
                body_bytes: None,
                timeout: Some(PROVIDER_CONNECTION_TIMEOUT),
            })
            .await;
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        classify_provider_connection_response(response, model.trim(), latency_ms, true)
    }
}

/// First-party Anthropic models the mobile engine routes by default, plus the
/// configured `default_model` and any env-configured small-fast / haiku model a
/// `prompt` hook may resolve to. The `llm_runtime` registry resolves a request
/// model by exact id, so every model the host may request must appear here
/// (Phase 2a-mobile: this becomes the Anthropic profile `assemble` declares).
///
/// The mobile sibling of `harness_runtime::desktop::anthropic_models_for`, minus the
/// `fallback_model` (mobile has no fallback-model config knob). The env small-
/// fast / haiku ids are read inline because main has no public
/// `small_fast_model_env_ids` helper.
pub(super) fn anthropic_models(default_model: &str) -> Vec<llm_runtime::ModelProfile> {
    let caps = llm_runtime::Capabilities {
        streaming: true,
        tools: true,
        vision: true,
        documents: true,
        reasoning: true,
        structured_output: true,
    };
    // Every id `platform_api::is_curated_model` lists under the "anthropic" arm must
    // appear here, otherwise the client picker's ANTHROPIC section renders only
    // the subset this registry happens to route (the section used to show just
    // Sonnet 4.6 + Haiku 4.5 while Sonnet 5 / Opus 4.8 / Fable 5 were curated
    // but unroutable), plus the extra Opus routes the host may still request.
    let mut ids: Vec<String> = vec![
        "claude-opus-5".to_string(),
        "claude-opus-4-8".to_string(),
        "claude-opus-4-6".to_string(),
        "claude-sonnet-5".to_string(),
        "claude-sonnet-4-6".to_string(),
        "claude-haiku-4-5".to_string(),
        "claude-fable-5-1".to_string(),
    ];
    // The configured default, when it routes here — see `anthropic_route_id`.
    ids.extend(anthropic_route_id(default_model));
    // Env-configured small-fast / haiku model a `prompt` hook may resolve to
    // (matching `hook_prompt_runner::resolve_model`'s precedence:
    // `ANTHROPIC_SMALL_FAST_MODEL` > `ANTHROPIC_DEFAULT_HAIKU_MODEL` > default
    // Haiku), so such a request resolves instead of failing `ModelUnavailable`.
    for var in [
        "ANTHROPIC_SMALL_FAST_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    ] {
        if let Ok(m) = std::env::var(var) {
            // Same routing rule as the configured default above. Pushing the
            // raw env value bypassed the guard entirely — `ANTHROPIC_SMALL_
            // FAST_MODEL=anthropic/claude-haiku-4-5` registered the qualified
            // string as a model id, and a foreign ref leaked a foreign model
            // into this registry by the very path the guard exists to close.
            ids.extend(anthropic_route_id(&m));
        }
    }
    ids.sort();
    ids.dedup();
    ids.into_iter()
        .map(|id| llm_runtime::ModelProfile {
            display_model: id.clone(),
            request_model: id.clone(),
            billing_model: id,
            aliases: Vec::new(),
            description: None,
            metadata: Default::default(),
            capabilities: caps,
        })
        .collect()
}

/// The BARE model id `model_ref` contributes to the Anthropic profile's
/// exact-id registry, or `None` when it names a model on another provider.
///
/// One rule for every source that can add a route (the configured default and
/// the `ANTHROPIC_SMALL_FAST_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` env ids):
/// the ref must ROUTE to anthropic (a `claude-*` id, an unqualified custom id,
/// or an `anthropic/…` ref) AND the remainder must be a BARE model id. A ref
/// qualified for another provider must never land here — a client that stored a
/// qualified id and re-qualified it on the way back in
/// (`anthropic/deepseek/deepseek-flash`) otherwise registered a `deepseek`
/// model inside the Anthropic profile, and the picker then rendered that
/// model's name under the ANTHROPIC header in Anthropic's colour.
pub(super) fn anthropic_route_id(model_ref: &str) -> Option<String> {
    let (profile, bare) = llm_runtime::split_profile_model(model_ref.trim());
    (profile == "anthropic" && !bare.is_empty() && !bare.contains('/')).then_some(bare)
}

/// The assembled provider profiles flattened into the [`platform_api::ModelListing`]s
/// that [`resolve_default_model_ref`] and [`platform_api::parse_model_ref`] resolve
/// against.
///
/// `display_model` / `provider_label` are immaterial to parsing, so
/// `request_model` and the profile name stand in for both. Shared with the
/// tests so they cannot drift from the shape production actually feeds in.
pub(super) fn model_listings(
    providers: &[llm_runtime::ProviderProfile],
) -> Vec<platform_api::ModelListing> {
    llm_runtime::ModelRegistry::from_config(llm_runtime::ClientConfig {
        providers: providers.to_vec(),
    })
    .map(|registry| {
        registry
            .available_models()
            .into_iter()
            .map(orchestrator::provider_adapter::lower_model_listing)
            .collect()
    })
    .unwrap_or_default()
}

/// Parse the configured `default_model` into `(request_model, profile)`, and
/// self-heal a reference that routes to NO registered provider.
///
/// A client persists its last-picked model and hands it back on the next
/// launch, so a client-side bug can hand us a reference no profile serves (iOS
/// re-qualified an already-qualified id into `anthropic/deepseek/deepseek-v4-
/// flash`). [`platform_api::parse_model_ref`] then returns the whole string as a bare
/// id, which boots the session onto an unroutable model: the picker shows a
/// junk row and the first turn fails `ModelUnavailable`. Rewriting it to a
/// model that IS registered keeps the session usable and lets the user re-pick.
///
/// Bare custom ids still resolve — [`anthropic_models`] registers them under
/// the Anthropic profile — so only genuinely unroutable refs are rewritten.
///
/// The replacement is picked FROM `listings`, never from a constant: the mobile
/// allowlist (`mobileEnabledProfiles`) is fail-closed and can strip the
/// Anthropic profile entirely, and healing onto a hardcoded `claude-sonnet-5`
/// there would swap one unroutable ref for another while the log claimed the
/// session was repaired. The chosen profile is returned too — a bare
/// `ClientEvent::ModelList { current }` matches none of the provider-qualified
/// rows `platform_api::curated_model_refs` emits, so the client's picker would render
/// with nothing selected.
pub(super) fn resolve_default_model_ref(
    default_model: &str,
    listings: &[platform_api::ModelListing],
) -> (String, Option<String>) {
    let (model, profile) = platform_api::parse_model_ref(default_model, listings);
    // `parse_model_ref` returns `Some(profile)` only after matching a listing on
    // that exact `(provider_id, request_model)` pair, so a qualified ref is
    // already proven routable and keeps its profile as-is.
    if profile.is_some() || listings.is_empty() {
        return (model, profile);
    }
    // A BARE id is routable when some listing serves it — and when exactly one
    // does, scope it to that provider. `curated_model_refs` performs the same
    // unique-provider inference for the rows it emits, so leaving the profile
    // unscoped made `ModelList { current }` bare while every row was qualified,
    // and the client's picker rendered with nothing selected. That is the
    // default on every fresh launch, since `MobileEngineConfig::default()`'s
    // `default_model` is a bare id.
    let mut serving = listings.iter().filter(|l| l.request_model == model);
    match (serving.next(), serving.next()) {
        // Ambiguous across profiles — stay unscoped and let the registry report
        // the ambiguity rather than silently picking a provider.
        (Some(_), Some(_)) => return (model, profile),
        (Some(only), None) => return (model, Some(only.provider_id.clone())),
        (None, _) => {}
    }
    (model, profile)
}

pub(super) const MOBILE_ENABLED_PROFILES_KEY: &str = "mobileEnabledProfiles";

/// File-backed settings override legacy native launch defaults using the same
/// field merge rules as desktop. Explicit file profiles remain selectable even
/// when an older native launcher sends its own profile allowlist.
pub(super) fn mobile_provider_settings(
    cfg: &MobileConfig,
) -> Result<lingxi_core::settings::SettingsJson, lingxi_core::settings::SettingsError> {
    use lingxi_core::settings::{
        FileLayerScope, LoadInputs, Settings, SettingsJson, SupplementalLayers,
    };

    let env = std::env::vars().collect();
    let layered = Settings::load_with_layers_from_user_path(
        LoadInputs {
            env: &env,
            project_dir: &cfg.cwd,
            defaults: SettingsJson::default(),
        },
        FileLayerScope::ALL,
        SupplementalLayers::default(),
        Some(&cfg.lingxi_home.join("settings.json")),
    )?
    .settings;
    let explicit_allowlist = layered
        .routing
        .as_ref()
        .and_then(|routing| routing.get(MOBILE_ENABLED_PROFILES_KEY))
        .is_some();
    let file_profiles: Vec<String> = layered
        .providers
        .as_ref()
        .map(|providers| providers.keys().cloned().collect())
        .unwrap_or_default();
    let mut merged = lingxi_core::settings::merger::merge(
        SettingsJson {
            providers: cfg.provider_profiles.clone(),
            routing: cfg.routing.clone(),
            ..SettingsJson::default()
        },
        layered,
    );
    if !explicit_allowlist {
        if let Some(enabled) = merged
            .routing
            .as_mut()
            .and_then(|routing| routing.get_mut(MOBILE_ENABLED_PROFILES_KEY))
            .and_then(serde_json::Value::as_array_mut)
        {
            for profile in file_profiles {
                let value = serde_json::Value::String(profile);
                if !enabled.contains(&value) {
                    enabled.push(value);
                }
            }
        }
    }
    Ok(merged)
}

/// Apply the mobile host's explicit provider profile allowlist after shared
/// provider assembly and before any model catalog or client is constructed.
///
/// The reserved routing key is interpreted only in this mobile composition
/// root, so desktop assembly remains unchanged. Absence preserves the shared
/// catalog for backward-compatible hosts. Presence is fail-closed: a malformed
/// value is treated as an empty allowlist.
pub(super) fn apply_mobile_profile_allowlist(
    assembled: &mut provider_config::Assembled,
    routing: Option<&serde_json::Value>,
) {
    let Some(value) = routing.and_then(|routing| routing.get(MOBILE_ENABLED_PROFILES_KEY)) else {
        return;
    };
    let enabled_profiles: std::collections::BTreeSet<String> = match value.as_array() {
        Some(items) => {
            let mut profiles = std::collections::BTreeSet::new();
            for item in items {
                let Some(profile) = item.as_str().filter(|profile| !profile.is_empty()) else {
                    profiles.clear();
                    break;
                };
                profiles.insert(profile.to_string());
            }
            profiles
        }
        None => std::collections::BTreeSet::new(),
    };

    let allowed_routes: Vec<(llm_runtime::ProviderId, String)> = assembled
        .client_config
        .providers
        .iter()
        .filter(|provider| enabled_profiles.contains(&provider.profile_name))
        .flat_map(|provider| {
            let provider_id = provider.provider_id.clone();
            provider
                .models
                .iter()
                .map(move |model| (provider_id.clone(), model.request_model.clone()))
        })
        .collect();

    assembled
        .client_config
        .providers
        .retain(|provider| enabled_profiles.contains(&provider.profile_name));
    assembled
        .credential_sources
        .retain(|source| enabled_profiles.contains(&source.profile_name));
    assembled.chains.aliases.retain(|_, target| {
        target
            .split_once('/')
            .is_some_and(|(profile, _)| enabled_profiles.contains(profile))
    });
    assembled.chains.chains.retain(|_, entries| {
        entries.retain(|entry| {
            allowed_routes.iter().any(|(provider_id, model)| {
                provider_id == &entry.provider_id && model == &entry.model
            })
        });
        !entries.is_empty()
    });
}

/// Lower an `Option<LoginInfo>` to the auth-state DTO (the inverse copy of the
/// bridge-server router's helper — kept private to the shared host so iOS /
/// Android cannot drift).
pub(super) fn lower_auth_state(
    info: Option<platform_api::auth::LoginInfo>,
) -> client::protocol::listings::AuthStateDto {
    match info {
        Some(li) => client::protocol::listings::AuthStateDto::SignedIn {
            email: li.email,
            org_id: li.org_id,
        },
        None => client::protocol::listings::AuthStateDto::SignedOut,
    }
}

pub(super) fn provider_id_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().enumerate().all(|(index, ch)| {
            ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || (index > 0 && matches!(ch, '-' | '_' | '.'))
        })
}

pub(super) const PROVIDER_CONNECTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(15);

pub(crate) const LOCAL_APPS_MCP_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

pub(super) fn provider_models_endpoint(
    api_base: &str,
    provider_preset: &str,
) -> Result<String, &'static str> {
    let base = api_base.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("请填写 API 地址");
    }
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err("API 地址必须以 https:// 或 http:// 开头");
    }
    if base
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '#' | '?'))
        || base.split_once("://").is_some_and(|(_, authority)| {
            authority
                .split('/')
                .next()
                .is_some_and(|host| host.contains('@'))
        })
    {
        return Err("API 地址格式无效");
    }

    if base.ends_with("/models") {
        return Ok(base.to_string());
    }
    if let Some(prefix) = base.strip_suffix("/chat/completions") {
        return Ok(format!("{prefix}/models"));
    }
    if provider_preset == "anthropic" && !base.ends_with("/v1") {
        return Ok(format!("{base}/v1/models"));
    }
    Ok(format!("{base}/models"))
}

pub(super) fn provider_connection_headers(
    provider_preset: &str,
    credential: &str,
) -> Vec<(String, String)> {
    let mut headers = vec![("accept".to_string(), "application/json".to_string())];
    match provider_preset {
        "anthropic" => {
            headers.push(("x-api-key".to_string(), credential.to_string()));
            headers.push(("anthropic-version".to_string(), "2023-06-01".to_string()));
        }
        "google" => {
            headers.push(("x-goog-api-key".to_string(), credential.to_string()));
        }
        _ => {
            headers.push(("authorization".to_string(), format!("Bearer {credential}")));
        }
    }
    headers
}

pub(super) fn provider_connection_failure(
    message: impl Into<String>,
    reachable: bool,
    authenticated: bool,
    http_status: Option<u16>,
    latency_ms: u64,
    used_stored_credential: bool,
) -> ProviderConnectionTestDto {
    ProviderConnectionTestDto {
        connected: false,
        reachable,
        authenticated,
        model_available: false,
        http_status,
        latency_ms,
        message: message.into(),
        used_stored_credential,
    }
}

pub(super) fn provider_model_ids(body: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))?
        .as_array()?;
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                entry
                    .get("id")
                    .or_else(|| entry.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(|id| id.strip_prefix("models/").unwrap_or(id).to_string())
            })
            .collect(),
    )
}

pub(super) fn classify_provider_connection_response(
    response: Result<protocol::HttpResponse, HttpError>,
    model: &str,
    latency_ms: u64,
    used_stored_credential: bool,
) -> ProviderConnectionTestDto {
    match response {
        Ok(response) if (200..300).contains(&response.status) => {
            let model_ids = provider_model_ids(&response.body);
            let model_available = model.is_empty()
                || model_ids
                    .as_ref()
                    .is_some_and(|ids| ids.iter().any(|id| id == model));
            match model_ids {
                Some(_) if !model_available => provider_connection_failure(
                    format!("连接与认证成功，但模型 `{model}` 不在可用列表中"),
                    true,
                    true,
                    Some(response.status),
                    latency_ms,
                    used_stored_credential,
                ),
                Some(_) => ProviderConnectionTestDto {
                    connected: true,
                    reachable: true,
                    authenticated: true,
                    model_available: true,
                    http_status: Some(response.status),
                    latency_ms,
                    message: format!("连接成功 · {latency_ms} ms"),
                    used_stored_credential,
                },
                None => ProviderConnectionTestDto {
                    connected: true,
                    reachable: true,
                    authenticated: true,
                    model_available: false,
                    http_status: Some(response.status),
                    latency_ms,
                    message: format!("连接与认证成功 · {latency_ms} ms（未能校验模型列表）"),
                    used_stored_credential,
                },
            }
        }
        Ok(response) => {
            classify_provider_connection_status(response.status, latency_ms, used_stored_credential)
        }
        Err(HttpError::Status { status, .. }) => {
            classify_provider_connection_status(status, latency_ms, used_stored_credential)
        }
        Err(HttpError::Timeout(_)) => provider_connection_failure(
            "连接超时，请检查网络或 API 地址",
            false,
            false,
            None,
            latency_ms,
            used_stored_credential,
        ),
        Err(HttpError::Connection(_)) => provider_connection_failure(
            "无法连接服务，请检查网络、DNS、TLS 或 API 地址",
            false,
            false,
            None,
            latency_ms,
            used_stored_credential,
        ),
        Err(HttpError::InvalidRequest(_)) => provider_connection_failure(
            "API 地址或请求配置无效",
            false,
            false,
            None,
            latency_ms,
            used_stored_credential,
        ),
        Err(HttpError::InvalidResponse(_)) => provider_connection_failure(
            "服务响应格式无效",
            true,
            false,
            None,
            latency_ms,
            used_stored_credential,
        ),
        Err(HttpError::Cancelled) => provider_connection_failure(
            "连接测试已取消",
            false,
            false,
            None,
            latency_ms,
            used_stored_credential,
        ),
    }
}

pub(super) fn classify_provider_connection_status(
    status: u16,
    latency_ms: u64,
    used_stored_credential: bool,
) -> ProviderConnectionTestDto {
    let (message, authenticated) = match status {
        400 | 422 => ("服务可达，但请求格式不受支持", false),
        401 => ("认证失败，请检查 API Key", false),
        402 => ("认证成功，但账户余额不足", true),
        403 => ("服务拒绝访问，请检查 Key 权限", false),
        404 => ("服务可达，但模型列表端点不存在；请检查 API 地址", false),
        429 => ("服务可达，但请求频率已达上限，请稍后重试", false),
        500..=599 => ("Provider 服务暂时不可用，请稍后重试", false),
        _ => ("Provider 返回了无法识别的响应", false),
    };
    provider_connection_failure(
        message,
        true,
        authenticated,
        Some(status),
        latency_ms,
        used_stored_credential,
    )
}
