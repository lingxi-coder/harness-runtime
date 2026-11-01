//! Credential lookup traits and lightweight providers.

use std::fmt;

use crate::{BoxFuture, LlmError, ProviderId};

/// Scope used to resolve provider credentials.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CredentialScope {
    /// Provider identity requesting credentials.
    pub provider_id: ProviderId,
    /// Provider profile name requesting credentials.
    pub profile_name: String,
    /// Host-defined credential id from `CredentialConfig::Static` or
    /// `CredentialConfig::HostManaged`, when one was configured.
    pub credential_id: Option<String>,
    /// Expected account identity captured with the immutable execution route.
    pub expected_identity_generation: Option<u64>,
    /// Resolved endpoint, when the host knows the selected route. OAuth
    /// credentials must not fall back onto a different provider origin.
    pub base_url: Option<String>,
    /// Protocol of that resolved endpoint.
    pub protocol: Option<crate::ProtocolFamily>,
}

impl CredentialScope {
    /// Create a credential lookup scope.
    #[must_use]
    pub fn new(provider_id: ProviderId, profile_name: impl Into<String>) -> Self {
        Self {
            provider_id,
            profile_name: profile_name.into(),
            credential_id: None,
            expected_identity_generation: None,
            base_url: None,
            protocol: None,
        }
    }

    /// Attach the host-defined credential id to the scope.
    #[must_use]
    pub fn with_credential_id(mut self, credential_id: impl Into<String>) -> Self {
        self.credential_id = Some(credential_id.into());
        self
    }

    /// Attach the endpoint and protocol selected by the model router.
    #[must_use]
    pub fn with_route(
        mut self,
        base_url: impl Into<String>,
        protocol: crate::ProtocolFamily,
    ) -> Self {
        self.base_url = Some(base_url.into());
        self.protocol = Some(protocol);
        self
    }

    /// The built-in first-party route can choose API key or OAuth for each
    /// request. Custom endpoints and compatibility protocols cannot inherit
    /// an account's first-party OAuth token.
    #[must_use]
    pub fn selects_anthropic_auth(&self) -> bool {
        self.provider_id == ProviderId::AnthropicFirstParty
            && self.profile_name == "anthropic"
            && self.protocol == Some(crate::ProtocolFamily::AnthropicMessages)
            && self
                .base_url
                .as_deref()
                .is_some_and(|url| url.trim_end_matches('/') == "https://api.anthropic.com")
            && matches!(
                self.credential_id.as_deref(),
                Some("anthropic-api-key" | "anthropic-oauth")
            )
    }

    /// Whether known route metadata forbids forwarding first-party OAuth.
    #[must_use]
    pub fn forbids_anthropic_oauth(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|url| url.trim_end_matches('/') != "https://api.anthropic.com")
            || self
                .protocol
                .is_some_and(|protocol| protocol != crate::ProtocolFamily::AnthropicMessages)
            || self.provider_id != ProviderId::AnthropicFirstParty
    }
}

/// Redacted source of the selected credential. This contains no key material.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum CredentialSource {
    /// The route does not use a credential, or its selected source is absent.
    None,
    /// This provider cannot establish its source without executing authentication.
    #[default]
    Unknown,
    /// The exact environment variable selected by the credential resolver.
    Environment { variable: String },
    /// A credential in the host's persistent or process-local credential store.
    Stored,
    /// An explicitly supplied host configuration value.
    Configured,
    /// The configured API-key helper, without executing or refreshing it.
    ApiKeyHelper,
    /// An existing OAuth credential, without refreshing its token.
    OAuth,
}

/// Secret material loaded by a credential provider.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// Provider API key.
    ApiKey(String),
    /// Bearer or OAuth access token.
    BearerToken(String),
    /// Anthropic OAuth material and its granted scopes, captured together.
    AnthropicOAuth {
        /// Current access token.
        access_token: String,
        /// Scopes granted for this token, not subscription/account roles.
        scopes: Vec<String>,
    },
    /// ChatGPT-account OAuth: bearer access token plus the `ChatGPT-Account-ID`
    /// header (and `FedRAMP` flag). Served by the openai-oauth credential provider.
    ChatGptOAuth {
        /// OAuth access token (bearer).
        access_token: String,
        /// `ChatGPT` workspace/account id (the `ChatGPT-Account-ID` header).
        account_id: Option<String>,
        /// Whether the account is `FedRAMP` (sets `X-OpenAI-Fedramp: true`).
        fedramp: bool,
    },
    /// AWS `SigV4` signing credentials.
    ///
    /// For `EnvCredentialProvider`, `SigV4` is not supported — use a
    /// `StaticCredentialProvider` or a host-managed credential store instead
    /// (loading them requires knowing the three fields together, which the
    /// single-env-variable model cannot express).
    AwsSigV4 {
        /// AWS access key ID.
        access_key_id: String,
        /// AWS secret access key.
        secret_access_key: String,
        /// Optional session token (for temporary credentials / STS).
        session_token: Option<String>,
    },
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApiKey(_) => formatter
                .debug_tuple("ApiKey")
                .field(&"[REDACTED]")
                .finish(),
            Self::BearerToken(_) => formatter
                .debug_tuple("BearerToken")
                .field(&"[REDACTED]")
                .finish(),
            Self::AnthropicOAuth { scopes, .. } => formatter
                .debug_struct("AnthropicOAuth")
                .field("access_token", &"[REDACTED]")
                .field("scopes", scopes)
                .finish(),
            Self::ChatGptOAuth {
                account_id,
                fedramp,
                ..
            } => formatter
                .debug_struct("ChatGptOAuth")
                .field("access_token", &"[REDACTED]")
                .field("account_id", account_id)
                .field("fedramp", fedramp)
                .finish(),
            Self::AwsSigV4 { .. } => formatter
                .debug_struct("AwsSigV4")
                .field("access_key_id", &"[REDACTED]")
                .field("secret_access_key", &"[REDACTED]")
                .field("session_token", &"[REDACTED]")
                .finish(),
        }
    }
}

/// Current first-party status sources, independent of the model's auth strategy.
/// Credentials retain their redacted Debug representation. The OAuth scope is
/// its actual refresh target, which may differ from the model credential id.
#[derive(Debug, Default, Clone)]
pub struct AnthropicAuthSnapshot {
    pub oauth: Option<(CredentialScope, Credential)>,
    pub api_key: Option<Credential>,
}
impl AnthropicAuthSnapshot {
    #[must_use]
    pub fn from_credential(scope: CredentialScope, credential: Credential) -> Self {
        match credential {
            credential @ Credential::AnthropicOAuth { .. } => Self {
                oauth: Some((scope, credential)),
                api_key: None,
            },
            credential @ Credential::ApiKey(_) => Self {
                oauth: None,
                api_key: Some(credential),
            },
            _ => Self::default(),
        }
    }
}

/// Credential material and its exact atomic vault revision, captured by one read.
#[derive(Debug, Clone)]
pub struct VersionedCredential {
    pub credential: Credential,
    pub credential_revision: Option<u64>,
}

/// Failure tied to the exact raw grant observed by the same versioned operation.
#[derive(Debug)]
pub struct VersionedCredentialError {
    pub error: LlmError,
    pub credential_revision: Option<u64>,
}
impl From<LlmError> for VersionedCredentialError {
    fn from(error: LlmError) -> Self {
        Self {
            error,
            credential_revision: None,
        }
    }
}

/// Loads credentials for a provider/profile scope.
///
/// `load` is async so implementations can refresh expiring material
/// (e.g. OAuth) inside the lookup.
pub trait CredentialProvider: fmt::Debug + Send + Sync {
    /// Inspect the scoped credential source without helper execution, token
    /// exchange, refresh or network requests. Unsupported providers return
    /// `Unknown`; this is distinct from proving that no credential exists.
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<CredentialSource, LlmError>> {
        Box::pin(async { Ok(CredentialSource::Unknown) })
    }

    /// Load credential material for a scope.
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>>;

    fn load_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<VersionedCredential, VersionedCredentialError>> {
        Box::pin(async move {
            self.load(scope)
                .await
                .map(|credential| VersionedCredential {
                    credential,
                    credential_revision: None,
                })
                .map_err(VersionedCredentialError::from)
        })
    }
    fn refresh_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<VersionedCredential>, VersionedCredentialError>> {
        Box::pin(async move {
            self.refresh(scope, rejected)
                .await
                .map(|value| {
                    value.map(|credential| VersionedCredential {
                        credential,
                        credential_revision: None,
                    })
                })
                .map_err(VersionedCredentialError::from)
        })
    }
    /// A provider-local derived-token cache may prove an old rejection is obsolete without I/O.
    fn rejected_credential_is_current<'a>(
        &'a self,
        _scope: &'a CredentialScope,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<bool>, LlmError>> {
        Box::pin(async { Ok(None) })
    }
    /// Observe the published vault revision without refreshing or performing provider requests.
    fn current_credential_revision<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async { Ok(None) })
    }
    /// Read independent first-party OAuth and cached/static API-key sources.
    /// No helper execution, token exchange or expiry-driven refresh is allowed.
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async { Ok(AnthropicAuthSnapshot::default()) })
    }

    /// Refresh rejected credential material when this source supports renewal.
    /// `None` means this source has no refresh capability.
    fn refresh<'a>(
        &'a self,
        _scope: &'a CredentialScope,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async { Ok(None) })
    }
}

/// Credential provider backed by one static credential.
#[derive(Debug, Clone)]
pub struct StaticCredentialProvider {
    credential: Credential,
}

impl StaticCredentialProvider {
    /// Create a provider returning the same credential for every scope.
    #[must_use]
    pub fn new(credential: Credential) -> Self {
        Self { credential }
    }
}

impl CredentialProvider for StaticCredentialProvider {
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<CredentialSource, LlmError>> {
        Box::pin(async { Ok(CredentialSource::Configured) })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(if scope.provider_id == ProviderId::AnthropicFirstParty {
                AnthropicAuthSnapshot::from_credential(scope.clone(), self.credential.clone())
            } else {
                AnthropicAuthSnapshot::default()
            })
        })
    }
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let credential = self.credential.clone();
        Box::pin(async move { Ok(credential) })
    }
}

/// Credential provider backed by an environment variable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCredentialProvider {
    variable_name: String,
}

impl EnvCredentialProvider {
    /// Create a provider that reads an API key from an environment variable.
    #[must_use]
    pub fn new(variable_name: impl Into<String>) -> Self {
        Self {
            variable_name: variable_name.into(),
        }
    }
}

impl CredentialProvider for EnvCredentialProvider {
    fn source<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<CredentialSource, LlmError>> {
        Box::pin(async move {
            Ok(if std::env::var(&self.variable_name).is_ok() {
                CredentialSource::Environment {
                    variable: self.variable_name.clone(),
                }
            } else {
                CredentialSource::None
            })
        })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            Ok(if scope.provider_id == ProviderId::AnthropicFirstParty {
                std::env::var(&self.variable_name).ok().map_or_else(
                    AnthropicAuthSnapshot::default,
                    |key| {
                        AnthropicAuthSnapshot::from_credential(
                            scope.clone(),
                            Credential::ApiKey(key),
                        )
                    },
                )
            } else {
                AnthropicAuthSnapshot::default()
            })
        })
    }
    fn load<'a>(
        &'a self,
        _scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        let result = std::env::var(&self.variable_name)
            .map(Credential::ApiKey)
            .map_err(|_| LlmError::Authentication {
                message: String::new(),
            });
        Box::pin(async move { result })
    }
}

/// A [`CredentialProvider`] decorator that mints a short-lived GitHub Copilot
/// bearer from the raw GitHub OAuth token.
///
/// `api.githubcopilot.com` rejects the raw OAuth token — it requires a token
/// minted from `copilot_internal/v2/token` (see
/// [`lingxi_llm_client::auth::oauth::copilot::exchange_copilot_token`]). This decorator wraps an inner
/// provider (which yields the raw OAuth token for the Copilot credential id);
/// for that one credential id it exchanges + caches the short-lived bearer and
/// re-exchanges once it is within [`lingxi_llm_client::auth::oauth::copilot::COPILOT_TOKEN_REFRESH_SKEW_SECS`]
/// of expiry. Every other credential id passes straight through unchanged, so
/// this can safely wrap the host's composite credential provider.
#[derive(Default)]
struct CopilotCache {
    tokens:
        std::collections::BTreeMap<String, lingxi_llm_client::auth::oauth::copilot::ExchangedToken>,
    current: std::collections::BTreeMap<String, String>,
}
pub struct CopilotExchangeCredentialProvider {
    inner: std::sync::Arc<dyn CredentialProvider>,
    http: std::sync::Arc<dyn lingxi_llm_client::transport::Transport>,
    credential_id: String,
    cached: std::sync::Mutex<CopilotCache>,
    exchange_locks: std::sync::Mutex<
        std::collections::BTreeMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>,
    >,
}

impl CopilotExchangeCredentialProvider {
    /// Wrap `inner`, exchanging the raw OAuth token it resolves for
    /// `credential_id` (e.g. `"github-copilot"`) into a short-lived Copilot
    /// bearer via `http`.
    #[must_use]
    pub fn new(
        inner: std::sync::Arc<dyn CredentialProvider>,
        http: std::sync::Arc<dyn lingxi_llm_client::transport::Transport>,
        credential_id: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            http,
            credential_id: credential_id.into(),
            cached: Default::default(),
            exchange_locks: Default::default(),
        }
    }

    fn is_copilot(&self, scope: &CredentialScope) -> bool {
        scope.credential_id.as_deref() == Some(self.credential_id.as_str())
            || matches!(&scope.provider_id,ProviderId::OpenAICompatible{name}|ProviderId::Custom{name} if name==&self.credential_id)
    }
    fn scope_key(scope: &CredentialScope) -> String {
        use sha2::Digest;
        format!(
            "{:x}",
            sha2::Sha256::digest(format!("{scope:?}").as_bytes())
        )
    }
    async fn exchanged(
        &self,
        scope: &CredentialScope,
        rejected: Option<&Credential>,
    ) -> Result<VersionedCredential, VersionedCredentialError> {
        use sha2::Digest;
        let raw = self.inner.load_versioned(scope).await?;
        let revision = raw.credential_revision;
        let token = match raw.credential {
            Credential::ApiKey(token) | Credential::BearerToken(token) => token,
            _ => {
                return Err(VersionedCredentialError {
                    error: LlmError::InvalidRequest {
                        message: "Copilot requires an explicitly stored GitHub token".into(),
                    },
                    credential_revision: revision,
                })
            }
        };
        let scope_key = Self::scope_key(scope);
        let key = format!(
            "{}:{:x}:{revision:?}",
            scope_key,
            sha2::Sha256::digest(token.as_bytes())
        );
        let lock = self
            .exchange_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key.clone())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        {
            let mut cache = self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache
                .tokens
                .retain(|_, token| token.is_fresh(Self::now_unix()));
            if let Some(cached) = cache.tokens.get(&key) {
                let credential = Credential::BearerToken(cached.bearer().to_owned());
                if rejected.is_none_or(|rejected| rejected != &credential) {
                    return Ok(VersionedCredential {
                        credential,
                        credential_revision: revision,
                    });
                }
            }
            cache.tokens.remove(&key);
        }
        let exchanged = lingxi_llm_client::auth::oauth::copilot::exchange_copilot_token(
            &*self.http,
            &token,
            crate::auth::copilot::EXCHANGE_IDENTITY,
        )
        .await
        .map_err(|error| {
            let error = match error.classification_source() {
                lingxi_llm_client::protocol::LlmError::Authentication { .. } => {
                    LlmError::Authentication {
                        message: "Copilot upstream credential rejected".into(),
                    }
                }
                lingxi_llm_client::protocol::LlmError::InvalidRequest { message } => {
                    LlmError::InvalidRequest {
                        message: message.clone(),
                    }
                }
                lingxi_llm_client::protocol::LlmError::TransportTimeout { .. } => {
                    LlmError::TransportTimeout {
                        message: "Copilot token exchange timed out".into(),
                    }
                }
                _ => LlmError::Transport {
                    message: "Copilot token exchange failed".into(),
                },
            };
            VersionedCredentialError {
                error,
                credential_revision: revision,
            }
        })?;
        let credential = Credential::BearerToken(exchanged.bearer().to_owned());
        let mut cache = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        cache.current.insert(scope_key, key.clone());
        cache.tokens.insert(key, exchanged);
        Ok(VersionedCredential {
            credential,
            credential_revision: revision,
        })
    }
    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}

impl fmt::Debug for CopilotExchangeCredentialProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CopilotExchangeCredentialProvider")
            .field("credential_id", &self.credential_id)
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl CredentialProvider for CopilotExchangeCredentialProvider {
    fn source<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<CredentialSource, LlmError>> {
        self.inner.source(scope)
    }
    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<AnthropicAuthSnapshot, LlmError>> {
        self.inner.anthropic_auth_snapshot(scope)
    }
    fn current_credential_revision<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        self.inner.current_credential_revision(scope)
    }
    fn rejected_credential_is_current<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<bool>, LlmError>> {
        if !self.is_copilot(scope) {
            return self.inner.rejected_credential_is_current(scope, rejected);
        }
        Box::pin(async move {
            let cache = self
                .cached
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(key) = cache.current.get(&Self::scope_key(scope)) else {
                return Ok(None);
            };
            let Some(token) = cache.tokens.get(key) else {
                return Ok(Some(false));
            };
            Ok(Some(
                token.is_fresh(Self::now_unix())
                    && &Credential::BearerToken(token.bearer().to_owned()) == rejected,
            ))
        })
    }
    fn load_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<VersionedCredential, VersionedCredentialError>> {
        if !self.is_copilot(scope) {
            return self.inner.load_versioned(scope);
        }
        Box::pin(async move { self.exchanged(scope, None).await })
    }
    fn refresh_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<VersionedCredential>, VersionedCredentialError>> {
        if !self.is_copilot(scope) {
            return self.inner.refresh_versioned(scope, rejected);
        }
        Box::pin(async move { self.exchanged(scope, Some(rejected)).await.map(Some) })
    }
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            self.load_versioned(scope)
                .await
                .map(|value| value.credential)
                .map_err(|error| error.error)
        })
    }
    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            self.refresh_versioned(scope, rejected)
                .await
                .map(|value| value.map(|value| value.credential))
                .map_err(|error| error.error)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chatgpt_oauth_credential_redacts_token_in_debug() {
        let c = Credential::ChatGptOAuth {
            access_token: "sk-secret".to_string(),
            account_id: Some("acc_1".to_string()),
            fedramp: false,
        };
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-secret"));
        assert!(dbg.contains("ChatGptOAuth"));
    }

    // ── CopilotExchangeCredentialProvider ────────────────────────────────────

    #[derive(Debug)]
    struct RawInner(String);
    impl CredentialProvider for RawInner {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            let v = self.0.clone();
            Box::pin(async move { Ok(Credential::ApiKey(v)) })
        }
    }

    struct MockExchangeHttp {
        calls: std::sync::Mutex<u32>,
        expires_at: u64,
    }
    #[async_trait::async_trait]
    impl lingxi_llm_client::transport::Transport for MockExchangeHttp {
        async fn send(
            &self,
            _request: lingxi_llm_client::transport::HttpRequest,
        ) -> Result<
            lingxi_llm_client::transport::StreamResponse,
            lingxi_llm_client::protocol::LlmError,
        > {
            *self.calls.lock().unwrap() += 1;
            let body =
                serde_json::json!({"token": "copilot-bearer", "expires_at": self.expires_at});
            Ok(lingxi_llm_client::transport::HttpResponse {
                status: 200,
                headers: vec![],
                body: serde_json::to_vec(&body).unwrap().into(),
            }
            .into())
        }
    }

    fn copilot_scope() -> CredentialScope {
        CredentialScope::new(
            ProviderId::OpenAICompatible {
                name: "github-copilot".to_string(),
            },
            "github-copilot",
        )
        .with_credential_id("github-copilot")
    }

    #[derive(Debug)]
    struct AccountCopilotRaw;
    impl CredentialProvider for AccountCopilotRaw {
        fn load<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async move {
                self.load_versioned(scope)
                    .await
                    .map(|value| value.credential)
                    .map_err(|error| error.error)
            })
        }
        fn load_versioned<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<VersionedCredential, VersionedCredentialError>> {
            let second = scope.credential_id.as_deref() == Some("acct.B");
            Box::pin(async move {
                Ok(VersionedCredential {
                    credential: Credential::ApiKey(
                        if second {
                            "synthetic-pat-B"
                        } else {
                            "synthetic-pat-A"
                        }
                        .into(),
                    ),
                    credential_revision: Some(if second { 11 } else { 7 }),
                })
            })
        }
    }
    struct AccountCopilotHttp {
        calls: std::sync::atomic::AtomicUsize,
        status: std::sync::atomic::AtomicU16,
    }
    #[async_trait::async_trait]
    impl lingxi_llm_client::Transport for AccountCopilotHttp {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            let attempt = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let raw = request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .unwrap()
                .1
                .clone();
            let body = serde_json::json!({"token":format!("derived-{raw}-{attempt}"),"expires_at":9_999_999_999u64});
            Ok(lingxi_llm_client::HttpResponse {
                status: self.status.load(std::sync::atomic::Ordering::SeqCst),
                headers: vec![],
                body: serde_json::to_vec(&body).unwrap().into(),
            }
            .into())
        }
    }
    #[tokio::test]
    async fn copilot_accounts_partition_derived_tokens_and_preserve_atomic_vault_version() {
        let http = std::sync::Arc::new(AccountCopilotHttp {
            calls: Default::default(),
            status: std::sync::atomic::AtomicU16::new(200),
        });
        let provider = CopilotExchangeCredentialProvider::new(
            std::sync::Arc::new(AccountCopilotRaw),
            http.clone(),
            "github-copilot",
        );
        let scope = |name: &str| {
            let mut scope = copilot_scope();
            scope.profile_name = format!("account.{name}");
            scope.credential_id = Some(format!("acct.{name}"));
            scope.expected_identity_generation = Some(1);
            scope
        };
        let a = scope("A");
        let b = scope("B");
        let first = provider.load_versioned(&a).await.unwrap();
        let second = provider.load_versioned(&b).await.unwrap();
        assert_eq!(first.credential_revision, Some(7));
        assert_eq!(second.credential_revision, Some(11));
        assert_ne!(first.credential, second.credential);
        assert_eq!(
            provider.load_versioned(&a).await.unwrap().credential,
            first.credential
        );
        let renewed = provider
            .refresh_versioned(&a, &first.credential)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(renewed.credential, first.credential);
        assert_eq!(renewed.credential_revision, Some(7));
        assert_eq!(
            provider
                .rejected_credential_is_current(&a, &first.credential)
                .await
                .unwrap(),
            Some(false)
        );
        assert_eq!(
            provider
                .rejected_credential_is_current(&a, &renewed.credential)
                .await
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            provider.load_versioned(&b).await.unwrap().credential,
            second.credential
        );
        assert_eq!(http.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
        let outcomes = futures::future::join_all(
            (0..50).map(|_| provider.refresh_versioned(&a, &renewed.credential)),
        )
        .await;
        let mut current = None;
        for outcome in outcomes {
            let value = outcome.unwrap().unwrap();
            assert_eq!(value.credential_revision, Some(7));
            if let Some(expected) = &current {
                assert_eq!(&value.credential, expected);
            } else {
                current = Some(value.credential);
            }
        }
        assert_eq!(
            http.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "one derived renewal across concurrent callers"
        );
        http.status.store(401, std::sync::atomic::Ordering::SeqCst);
        let error = provider
            .refresh_versioned(&a, &current.unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error.error, LlmError::Authentication { .. }));
        assert_eq!(
            error.credential_revision,
            Some(7),
            "raw exchange rejection retains the exact PAT revision"
        );
    }
    #[tokio::test]
    async fn exchanges_and_caches_copilot_bearer() {
        let http = std::sync::Arc::new(MockExchangeHttp {
            calls: std::sync::Mutex::new(0),
            expires_at: 9_999_999_999, // far future → cache stays fresh
        });
        let provider = CopilotExchangeCredentialProvider::new(
            std::sync::Arc::new(RawInner("gho_raw_oauth".to_string())),
            http.clone(),
            "github-copilot",
        );
        let scope = copilot_scope();

        let first = provider.load(&scope).await.expect("first load");
        assert_eq!(first, Credential::BearerToken("copilot-bearer".to_string()));
        // Second load is served from cache → no second exchange.
        let second = provider.load(&scope).await.expect("second load");
        assert_eq!(
            second,
            Credential::BearerToken("copilot-bearer".to_string())
        );
        assert_eq!(
            *http.calls.lock().unwrap(),
            1,
            "exchanged exactly once (cached)"
        );
    }

    #[tokio::test]
    async fn stale_token_is_re_exchanged() {
        let http = std::sync::Arc::new(MockExchangeHttp {
            calls: std::sync::Mutex::new(0),
            expires_at: 1, // already past → never fresh → re-exchange each load
        });
        let provider = CopilotExchangeCredentialProvider::new(
            std::sync::Arc::new(RawInner("gho_raw_oauth".to_string())),
            http.clone(),
            "github-copilot",
        );
        let scope = copilot_scope();
        provider.load(&scope).await.expect("load 1");
        provider.load(&scope).await.expect("load 2");
        assert_eq!(*http.calls.lock().unwrap(), 2, "stale token re-exchanged");
    }

    #[tokio::test]
    async fn non_copilot_scope_passes_through_without_exchange() {
        let http = std::sync::Arc::new(MockExchangeHttp {
            calls: std::sync::Mutex::new(0),
            expires_at: 9_999_999_999,
        });
        let provider = CopilotExchangeCredentialProvider::new(
            std::sync::Arc::new(RawInner("openai_key".to_string())),
            http.clone(),
            "github-copilot",
        );
        // A different credential id → straight passthrough (raw key, no exchange).
        let scope =
            CredentialScope::new(ProviderId::OpenAI, "openai").with_credential_id("openai-api-key");
        let got = provider.load(&scope).await.expect("passthrough");
        assert_eq!(got, Credential::ApiKey("openai_key".to_string()));
        assert_eq!(
            *http.calls.lock().unwrap(),
            0,
            "no exchange for non-copilot"
        );
    }
}
