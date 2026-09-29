//! Composite credential provider: the single `llm_runtime::CredentialProvider`
//! slot backing every routable profile.
//!
//! Dispatch (on `scope.credential_id`):
//! - any registered OAuth delegate id (e.g. `"anthropic-oauth"`, `"openai-chatgpt"`)
//!   → delegate to the corresponding per-id provider.
//! - `"anthropic-api-key"` → the configured Anthropic key.
//! - any other id → keychain[id] → env[recorded var] → `Err(Authentication)`
//!   (matching `EnvCredentialProvider`; spec §6.1/§6.6).
//!
//! Secrets are returned as `Credential::ApiKey`; `ModelRuntime::load_secret`
//! extracts `ApiKey | BearerToken` uniformly and the profile's `AuthStrategy`
//! picks the header (so Copilot's `CopilotBearer` rides this path).

use std::collections::BTreeMap;
use std::sync::Arc;

use llm_runtime::{BoxFuture, Credential, CredentialProvider, CredentialScope, LlmError};

use crate::CredentialSource;

/// The single composite credential slot for all provider profiles.
pub struct MultiCredentialProvider {
    credentials: Arc<secret::CredentialManager>,
    sources: BTreeMap<String, CredentialSource>,
    anthropic_api_key: Option<String>,
    anthropic_api_key_helper: Option<String>,
    anthropic_api_key_helper_cache: llm_runtime::oauth::anthropic::ApiKeyHelperCache,
    oauth_delegates: BTreeMap<String, Arc<dyn CredentialProvider>>,
}

// `credentials` (an `Arc<secret::CredentialManager>`) is intentionally omitted —
// it is not `Debug` and would leak nothing useful; the redacted summary below is
// the deliberate shape.
#[allow(clippy::missing_fields_in_debug)]
impl std::fmt::Debug for MultiCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiCredentialProvider")
            .field("source_ids", &self.sources.keys().collect::<Vec<_>>())
            .field("has_anthropic_api_key", &self.anthropic_api_key.is_some())
            .field(
                "has_anthropic_api_key_helper",
                &self.anthropic_api_key_helper.is_some(),
            )
            .field(
                "oauth_delegate_ids",
                &self.oauth_delegates.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl MultiCredentialProvider {
    /// Build the composite from the assembled `credential_sources`, an optional
    /// Anthropic API key, and a map of OAuth delegates keyed by `credential_id`
    /// (e.g. `"anthropic-oauth"`, `"openai-chatgpt"`).
    #[must_use]
    pub fn new(
        credentials: Arc<secret::CredentialManager>,
        sources: Vec<CredentialSource>,
        anthropic_api_key: Option<String>,
        anthropic_api_key_helper: Option<String>,
        oauth_delegates: BTreeMap<String, Arc<dyn CredentialProvider>>,
    ) -> Self {
        let sources = sources
            .into_iter()
            .map(|s| (s.credential_id.clone(), s))
            .collect();
        Self {
            credentials,
            sources,
            anthropic_api_key,
            anthropic_api_key_helper,
            anthropic_api_key_helper_cache: llm_runtime::oauth::anthropic::ApiKeyHelperCache::new(),
            oauth_delegates,
        }
    }

    async fn load_anthropic_api_key(&self) -> Result<Credential, LlmError> {
        // A signed Desktop host can rotate the session's broker-owned key at
        // runtime. That process-local value must supersede the immutable boot
        // snapshot, while the normal static/helper/persistent precedence below
        // remains unchanged for CLI and TUI.
        if let Some(key) = self
            .credentials
            .get_provider_key_ephemeral("anthropic-api-key")
            .await
        {
            return Ok(Credential::ApiKey(key.expose_secret().clone()));
        }
        if let Some(key) = self.anthropic_api_key.clone() {
            return Ok(Credential::ApiKey(key));
        }
        // claude-code `jM()` (getAnthropicApiKeyWithSource) resolves in order:
        // approved env → managed → apiKeyHelper → config/macOS-keychain → none.
        // So the `apiKeyHelper` runs BEFORE the OS secure store and a configured
        // helper SHADOWS a stored key (the port previously consulted the store
        // first, inverting this). A helper that yields nothing (or fails) falls
        // through to the store; only when the store is ALSO empty does a helper
        // failure surface, preserving its diagnostic message.
        let helper_error = if let Some(helper) = self.anthropic_api_key_helper.as_deref() {
            let ttl_ms = llm_runtime::oauth::anthropic::api_key_helper_ttl_ms();
            match llm_runtime::oauth::anthropic::fetch_api_key_result(
                helper,
                &self.anthropic_api_key_helper_cache,
                ttl_ms,
            )
            .await
            {
                Ok(key) => return Ok(Credential::ApiKey(key)),
                Err(e) => Some(LlmError::InvalidRequest {
                    message: format!("apiKeyHelper failed: {e}"),
                }),
            }
        } else {
            None
        };
        if let Ok(Some(key)) = self.credentials.get_anthropic_api_key().await {
            return Ok(Credential::ApiKey(key.expose_secret().clone()));
        }
        Err(helper_error.unwrap_or(LlmError::Authentication {
            message: String::new(),
        }))
    }

    /// Resolve a non-Anthropic provider key: keychain[id] → env[var] → Authentication.
    async fn load_provider_key(&self, credential_id: &str) -> Result<Credential, LlmError> {
        // The `Ok(None)` and `Err(_)` arms are kept distinct on purpose: a missing
        // key vs an unavailable keychain are semantically different even though both
        // fall through to env (see comment). Suppress the same-body lint.
        #[allow(clippy::match_same_arms)]
        match self.credentials.get_provider_key(credential_id).await {
            Ok(Some(secret)) => return Ok(Credential::ApiKey(secret.expose_secret().clone())),
            Ok(None) => {}
            // Keychain unavailable (headless): fall through to env so env keys
            // still work; a truly-missing key surfaces as the per-turn 401.
            Err(_) => {}
        }
        if let Some(var) = self
            .sources
            .get(credential_id)
            .and_then(|s| s.env_var.as_deref())
        {
            if let Ok(val) = std::env::var(var) {
                return Ok(Credential::ApiKey(val));
            }
        }
        Err(LlmError::Authentication {
            message: String::new(),
        })
    }
}

impl CredentialProvider for MultiCredentialProvider {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let Some(credential_id) = scope.credential_id.as_deref() else {
                return Err(LlmError::Authentication {
                    message: String::new(),
                });
            };
            // Any registered OAuth delegate wins for its credential_id
            // (anthropic-oauth, openai-chatgpt, …).
            if let Some(delegate) = self.oauth_delegates.get(credential_id) {
                return delegate.load(scope).await;
            }
            match credential_id {
                "anthropic-api-key" => self.load_anthropic_api_key().await,
                other => self.load_provider_key(other).await,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{
        Capabilities, Credential, CredentialProvider, CredentialScope, LlmError, LlmRequest,
        ModelProfile, ModelRuntime, ProviderId,
    };
    use std::sync::Arc;

    /// Shared in-memory `CredentialManager` harness (re-used by availability.rs too).
    #[derive(Default)]
    struct MemStorage {
        map: std::sync::Mutex<
            std::collections::HashMap<(String, String), protocol::SecureStorageData>,
        >,
    }
    #[async_trait::async_trait]
    impl platform_api::SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: protocol::SecureStorageData,
        ) -> Result<(), platform_api::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .insert((service.into(), account.into()), data);
            Ok(())
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<Option<protocol::SecureStorageData>, platform_api::SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), platform_api::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, platform_api::SecureStorageError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|(s, _)| s == service)
                .map(|(_, a)| a.clone())
                .collect())
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> platform_api::SecureStorageBackend {
            platform_api::SecureStorageBackend::PlainText
        }
    }
    struct FixedClock;
    impl platform_api::Clock for FixedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)
        }
    }
    struct NoHttp;
    #[async_trait::async_trait]
    impl platform_api::HttpTransport for NoHttp {
        async fn request(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<protocol::HttpResponse, platform_api::HttpError> {
            panic!("credential tests must not perform HTTP");
        }
        async fn stream_sse(
            &self,
            _req: protocol::HttpRequest,
        ) -> Result<platform_api::http::SseStream, platform_api::HttpError> {
            panic!("credential tests must not perform HTTP");
        }
    }
    fn manager() -> Arc<secret::CredentialManager> {
        Arc::new(secret::CredentialManager::new(
            Arc::new(MemStorage::default()),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ))
    }
    fn source(
        provider_id: ProviderId,
        credential_id: &str,
        env_var: Option<&str>,
        kind: crate::CredentialKind,
    ) -> crate::CredentialSource {
        crate::CredentialSource {
            provider_id,
            profile_name: credential_id.to_string(),
            credential_id: credential_id.to_string(),
            env_var: env_var.map(str::to_string),
            kind,
        }
    }
    fn scope(provider: ProviderId, profile: &str, cred_id: &str) -> CredentialScope {
        CredentialScope::new(provider, profile).with_credential_id(cred_id.to_string())
    }

    /// Serialize env mutation across tests (process-global env).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn anthropic_api_key_dispatch_returns_configured_key() {
        let provider = MultiCredentialProvider::new(
            manager(),
            Vec::new(),
            Some("sk-ant-test".to_string()),
            None,
            Default::default(),
        );
        let got = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect("api-key dispatch");
        assert_eq!(got, Credential::ApiKey("sk-ant-test".to_string()));
    }

    #[tokio::test]
    async fn ephemeral_anthropic_key_replaces_the_boot_snapshot() {
        let credentials = manager();
        let provider = MultiCredentialProvider::new(
            credentials.clone(),
            Vec::new(),
            Some("boot-key".to_string()),
            None,
            Default::default(),
        );
        credentials
            .set_provider_key_ephemeral("anthropic", "rotated-key")
            .await;

        let got = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect("rotated api-key dispatch");
        assert_eq!(got, Credential::ApiKey("rotated-key".to_string()));
    }

    #[tokio::test]
    async fn anthropic_api_key_dispatch_reads_the_shared_secure_store() {
        let credentials = manager();
        credentials
            .store_anthropic_api_key("sk-ant-shared")
            .await
            .expect("seed shared store");
        let provider =
            MultiCredentialProvider::new(credentials, Vec::new(), None, None, Default::default());

        let got = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect("shared api-key dispatch");
        assert_eq!(got, Credential::ApiKey("sk-ant-shared".to_string()));
    }

    #[tokio::test]
    async fn persistent_anthropic_key_rotation_and_delete_are_live() {
        let credentials = manager();
        credentials
            .store_anthropic_api_key("sk-ant-old")
            .await
            .expect("seed old key");
        // Desktop passes no immutable snapshot when the resolved key came
        // from this shared store.
        let provider = MultiCredentialProvider::new(
            credentials.clone(),
            Vec::new(),
            None,
            None,
            Default::default(),
        );
        let scope = scope(
            ProviderId::AnthropicFirstParty,
            "anthropic",
            "anthropic-api-key",
        );

        credentials
            .store_anthropic_api_key("sk-ant-new")
            .await
            .expect("rotate key");
        assert_eq!(
            provider.load(&scope).await.expect("rotated key"),
            Credential::ApiKey("sk-ant-new".to_string())
        );

        credentials
            .delete_anthropic_api_key()
            .await
            .expect("delete key");
        assert!(matches!(
            provider.load(&scope).await,
            Err(LlmError::Authentication { .. })
        ));
    }

    #[tokio::test]
    async fn anthropic_api_key_dispatch_missing_key_is_authentication_error() {
        let provider =
            MultiCredentialProvider::new(manager(), Vec::new(), None, None, Default::default());
        let err = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect_err("no key configured");
        assert_eq!(
            err,
            LlmError::Authentication {
                message: String::new()
            }
        );
    }

    #[tokio::test]
    async fn cold_anthropic_route_authenticates_after_key_is_added() {
        let credentials = manager();
        let assembled = crate::assemble::assemble(crate::AssembleInputs {
            anthropic_api_base: "https://api.anthropic.com".to_string(),
            anthropic_models: vec![ModelProfile {
                display_model: "claude-sonnet-4-6".to_string(),
                request_model: "claude-sonnet-4-6".to_string(),
                billing_model: "claude-sonnet-4-6".to_string(),
                aliases: Vec::new(),
                description: None,
                metadata: Default::default(),
                capabilities: Capabilities::default(),
            }],
            anthropic_has_api_key: false,
            anthropic_has_oauth: false,
            user_providers: Default::default(),
            routing: None,
        });
        let mut client = ModelRuntime::from_config(assembled.client_config)
            .expect("cold-start route config must be valid");
        client = client.with_credential_provider(Arc::new(MultiCredentialProvider::new(
            credentials.clone(),
            assembled.credential_sources,
            None,
            None,
            Default::default(),
        )));
        let request = LlmRequest::new("claude-sonnet-4-6").with_user_text("hello");
        assert!(client.prepare(&request).await.is_err());

        credentials
            .set_provider_key_ephemeral("anthropic", "sk-ant-after-boot")
            .await;
        let prepared = client
            .prepare(&request)
            .await
            .expect("the same live route must load a key added after boot");
        assert_eq!(
            prepared
                .provider_request
                .headers
                .get("x-api-key")
                .map(String::as_str),
            Some("sk-ant-after-boot")
        );
    }

    #[tokio::test]
    async fn anthropic_api_key_helper_supplies_key_when_no_static_key() {
        let provider = MultiCredentialProvider::new(
            manager(),
            Vec::new(),
            None,
            Some("printf '  sk-helper-123  \\n'".to_string()),
            Default::default(),
        );
        let got = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect("helper key");
        assert_eq!(got, Credential::ApiKey("sk-helper-123".to_string()));
    }

    #[tokio::test]
    async fn anthropic_api_key_helper_failure_preserves_error() {
        let provider = MultiCredentialProvider::new(
            manager(),
            Vec::new(),
            None,
            Some("echo boom 1>&2; exit 3".to_string()),
            Default::default(),
        );
        let err = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-api-key",
            ))
            .await
            .expect_err("helper failure");
        assert_eq!(
            err,
            LlmError::InvalidRequest {
                message: "apiKeyHelper failed: exited 3: boom".to_string()
            }
        );
    }

    #[tokio::test]
    async fn missing_credential_id_is_authentication_error() {
        let provider =
            MultiCredentialProvider::new(manager(), Vec::new(), None, None, Default::default());
        let err = provider
            .load(&CredentialScope::new(
                ProviderId::AnthropicFirstParty,
                "anthropic",
            ))
            .await
            .expect_err("no credential id");
        assert_eq!(
            err,
            LlmError::Authentication {
                message: String::new()
            }
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn keychain_wins_over_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let cm = manager();
        cm.set_provider_key("openrouter", "key-from-keychain")
            .await
            .expect("store");
        std::env::set_var("OPENROUTER_API_KEY", "key-from-env");
        let provider = MultiCredentialProvider::new(
            cm,
            vec![source(
                ProviderId::OpenAICompatible {
                    name: "openrouter".to_string(),
                },
                "openrouter",
                Some("OPENROUTER_API_KEY"),
                crate::CredentialKind::Keychain,
            )],
            None,
            None,
            Default::default(),
        );
        let got = provider
            .load(&scope(
                ProviderId::OpenAICompatible {
                    name: "openrouter".to_string(),
                },
                "openrouter",
                "openrouter",
            ))
            .await
            .expect("resolve");
        std::env::remove_var("OPENROUTER_API_KEY");
        assert_eq!(got, Credential::ApiKey("key-from-keychain".to_string()));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn env_used_when_keychain_empty() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("DEEPSEEK_API_KEY", "key-from-env");
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![source(
                ProviderId::OpenAICompatible {
                    name: "deepseek".to_string(),
                },
                "deepseek",
                Some("DEEPSEEK_API_KEY"),
                crate::CredentialKind::ApiKey,
            )],
            None,
            None,
            Default::default(),
        );
        let got = provider
            .load(&scope(
                ProviderId::OpenAICompatible {
                    name: "deepseek".to_string(),
                },
                "deepseek",
                "deepseek",
            ))
            .await
            .expect("resolve");
        std::env::remove_var("DEEPSEEK_API_KEY");
        assert_eq!(got, Credential::ApiKey("key-from-env".to_string()));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn none_when_neither_keychain_nor_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("GLM_NO_SUCH_VAR");
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![source(
                ProviderId::Custom {
                    name: "glm-coding".to_string(),
                },
                "glm-coding",
                Some("GLM_NO_SUCH_VAR"),
                crate::CredentialKind::ApiKey,
            )],
            None,
            None,
            Default::default(),
        );
        let err = provider
            .load(&scope(
                ProviderId::Custom {
                    name: "glm-coding".to_string(),
                },
                "glm-coding",
                "glm-coding",
            ))
            .await
            .expect_err("nothing configured");
        assert_eq!(
            err,
            LlmError::Authentication {
                message: String::new()
            }
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // serialize env-var mutation across async tests
    async fn copilot_via_github_token_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("GITHUB_TOKEN", "ghp-token");
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![source(
                ProviderId::OpenAICompatible {
                    name: "github-copilot".to_string(),
                },
                "github-copilot",
                Some("GITHUB_TOKEN"),
                crate::CredentialKind::Keychain,
            )],
            None,
            None,
            Default::default(),
        );
        let got = provider
            .load(&scope(
                ProviderId::OpenAICompatible {
                    name: "github-copilot".to_string(),
                },
                "github-copilot",
                "github-copilot",
            ))
            .await
            .expect("resolve copilot");
        std::env::remove_var("GITHUB_TOKEN");
        assert_eq!(got, Credential::ApiKey("ghp-token".to_string()));
    }

    #[derive(Debug)]
    struct StubOAuth {
        token: String,
    }
    impl CredentialProvider for StubOAuth {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> llm_runtime::BoxFuture<'a, Result<Credential, LlmError>> {
            let tok = self.token.clone();
            Box::pin(async move { Ok(Credential::BearerToken(tok)) })
        }
    }

    #[tokio::test]
    async fn oauth_id_delegates_to_delegate() {
        let delegate: Arc<dyn CredentialProvider> = Arc::new(StubOAuth {
            token: "oauth-access".to_string(),
        });
        let mut delegates: BTreeMap<String, Arc<dyn CredentialProvider>> = BTreeMap::new();
        delegates.insert("anthropic-oauth".into(), delegate);
        let provider = MultiCredentialProvider::new(manager(), Vec::new(), None, None, delegates);
        let got = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-oauth",
            ))
            .await
            .expect("delegate");
        assert_eq!(got, Credential::BearerToken("oauth-access".to_string()));
    }

    #[tokio::test]
    async fn oauth_id_without_delegate_is_authentication_error() {
        let provider =
            MultiCredentialProvider::new(manager(), Vec::new(), None, None, Default::default());
        let err = provider
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-oauth",
            ))
            .await
            .expect_err("no delegate");
        assert_eq!(
            err,
            LlmError::Authentication {
                message: String::new()
            }
        );
    }

    /// A stub that always returns the fixed `Credential` it was constructed with.
    #[derive(Debug)]
    struct StubProvider(Credential);
    impl CredentialProvider for StubProvider {
        fn load<'a>(
            &'a self,
            _scope: &'a CredentialScope,
        ) -> llm_runtime::BoxFuture<'a, Result<Credential, LlmError>> {
            let c = self.0.clone();
            Box::pin(async move { Ok(c) })
        }
    }

    #[tokio::test]
    async fn routes_oauth_delegate_by_credential_id() {
        let anthropic = Arc::new(StubProvider(Credential::BearerToken("ANT".into())));
        let openai = Arc::new(StubProvider(Credential::ChatGptOAuth {
            access_token: "OAI".into(),
            account_id: Some("a".into()),
            fedramp: false,
        }));
        let mut delegates: BTreeMap<String, Arc<dyn CredentialProvider>> = BTreeMap::new();
        delegates.insert("anthropic-oauth".into(), anthropic);
        delegates.insert("openai-chatgpt".into(), openai);
        let mcp = MultiCredentialProvider::new(manager(), vec![], None, None, delegates);
        let got = mcp
            .load(&scope(
                ProviderId::OpenAICompatible {
                    name: "openai-chatgpt".into(),
                },
                "openai-chatgpt",
                "openai-chatgpt",
            ))
            .await
            .unwrap();
        assert!(matches!(got, Credential::ChatGptOAuth { .. }));
        let got_ant = mcp
            .load(&scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-oauth",
            ))
            .await
            .unwrap();
        assert!(matches!(got_ant, Credential::BearerToken(_)));
    }
}
