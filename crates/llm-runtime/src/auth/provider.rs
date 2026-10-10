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
}

impl CredentialScope {
    /// Create a credential lookup scope.
    #[must_use]
    pub fn new(provider_id: ProviderId, profile_name: impl Into<String>) -> Self {
        Self {
            provider_id,
            profile_name: profile_name.into(),
            credential_id: None,
        }
    }

    /// Attach the host-defined credential id to the scope.
    #[must_use]
    pub fn with_credential_id(mut self, credential_id: impl Into<String>) -> Self {
        self.credential_id = Some(credential_id.into());
        self
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
pub struct CopilotExchangeCredentialProvider {
    inner: std::sync::Arc<dyn CredentialProvider>,
    http: std::sync::Arc<dyn lingxi_llm_client::transport::Transport>,
    credential_id: String,
    cached: std::sync::Mutex<Option<lingxi_llm_client::auth::oauth::copilot::ExchangedToken>>,
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
            cached: std::sync::Mutex::new(None),
        }
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
    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        if scope.credential_id.as_deref() == Some(self.credential_id.as_str()) {
            Box::pin(async { Ok(None) })
        } else {
            self.inner.refresh(scope, rejected)
        }
    }
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            // Only the Copilot credential id is exchanged; everything else is a
            // straight passthrough to the wrapped provider.
            if scope.credential_id.as_deref() != Some(self.credential_id.as_str()) {
                return self.inner.load(scope).await;
            }

            // Serve a still-fresh cached bearer with no network round-trip. The
            // lock is scoped so it is never held across an await.
            {
                let guard = self.cached.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(token) = guard.as_ref() {
                    if token.is_fresh(Self::now_unix()) {
                        return Ok(Credential::BearerToken(token.bearer().to_string()));
                    }
                }
            }

            // Resolve the raw GitHub OAuth token from the inner provider, then
            // exchange it for a short-lived Copilot bearer and cache it.
            let raw = match self.inner.load(scope).await? {
                Credential::ApiKey(token) | Credential::BearerToken(token) => token,
                other => {
                    return Err(LlmError::InvalidRequest {
                        message: format!(
                            "copilot credential must be an api-key/bearer OAuth token, got {other:?}"
                        ),
                    });
                }
            };
            let exchanged = lingxi_llm_client::auth::oauth::copilot::exchange_copilot_token(
                &*self.http,
                &raw,
                crate::auth::copilot::EXCHANGE_IDENTITY,
            )
            .await
            .map_err(|error| match error {
                lingxi_llm_client::protocol::LlmError::InvalidRequest { message } => {
                    LlmError::InvalidRequest { message }
                }
                lingxi_llm_client::protocol::LlmError::TransportTimeout { message } => {
                    LlmError::TransportTimeout { message }
                }
                _ => LlmError::Transport {
                    message: "Copilot token exchange failed".into(),
                },
            })?;
            let bearer = exchanged.bearer().to_string();
            *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = Some(exchanged);
            Ok(Credential::BearerToken(bearer))
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
