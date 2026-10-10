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
//! OAuth delegates retain their account/scope credential material. The host
//! passes selected credentials to the SDK authenticator; the profile strategy
//! determines its wire headers.

use std::collections::BTreeMap;
use std::sync::Arc;

use llm_runtime::{
    BoxFuture, Credential, CredentialProvider, CredentialScope, CredentialSource as Source,
    LlmError,
};

use crate::CredentialSource;

/// The single composite credential slot for all provider profiles.
pub struct MultiCredentialProvider {
    credentials: Arc<secret::CredentialManager>,
    sources: BTreeMap<String, CredentialSource>,
    anthropic_api_key: Option<String>,
    anthropic_api_key_source: Source,
    anthropic_api_key_helper: Option<String>,
    anthropic_api_key_helper_cache: llm_runtime::auth::anthropic::ApiKeyHelperCache,
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
            anthropic_api_key_source: Source::Configured,
            anthropic_api_key_helper,
            anthropic_api_key_helper_cache: llm_runtime::auth::anthropic::ApiKeyHelperCache::new(),
            oauth_delegates,
        }
    }

    /// Carry the origin supplied by the host that resolved the static key.
    #[must_use]
    pub fn with_anthropic_api_key_source(mut self, source: Source) -> Self {
        self.anthropic_api_key_source = source;
        self
    }

    async fn direct_anthropic_api_key(&self) -> Option<(Credential, Source)> {
        if let Some(key) = self
            .credentials
            .get_provider_key_ephemeral("anthropic-api-key")
            .await
        {
            return Some((
                Credential::ApiKey(key.expose_secret().clone()),
                Source::Stored,
            ));
        }
        self.anthropic_api_key.clone().map(|key| {
            (
                Credential::ApiKey(key),
                self.anthropic_api_key_source.clone(),
            )
        })
    }

    async fn load_anthropic_api_key(&self) -> Result<Credential, LlmError> {
        // A signed Desktop host can rotate the session's broker-owned key at
        // runtime. That process-local value must supersede the immutable boot
        // snapshot, while the normal static/helper/persistent precedence below
        // remains unchanged for CLI and TUI.
        if let Some((credential, _)) = self.direct_anthropic_api_key().await {
            return Ok(credential);
        }
        // claude-code `jM()` (getAnthropicApiKeyWithSource) resolves in order:
        // approved env → managed → apiKeyHelper → config/macOS-keychain → none.
        // So the `apiKeyHelper` runs BEFORE the OS secure store and a configured
        // helper SHADOWS a stored key (the port previously consulted the store
        // first, inverting this). A helper that yields nothing (or fails) falls
        // through to the store; only when the store is ALSO empty does a helper
        // failure surface, preserving its diagnostic message.
        let helper_error = if let Some(helper) = self.anthropic_api_key_helper.as_deref() {
            let ttl_ms = llm_runtime::auth::anthropic::api_key_helper_ttl_ms();
            match llm_runtime::auth::anthropic::fetch_api_key_result(
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
        let (credential, _) = self.provider_key_selection(credential_id).await;
        credential.ok_or(LlmError::Authentication {
            message: String::new(),
        })
    }

    /// The same selection serves request loading and redacted source inspection.
    async fn provider_key_selection(&self, credential_id: &str) -> (Option<Credential>, Source) {
        let store_unavailable;
        match self.credentials.get_provider_key(credential_id).await {
            Ok(Some(secret)) => {
                return (
                    Some(Credential::ApiKey(secret.expose_secret().clone())),
                    Source::Stored,
                );
            }
            Ok(None) => store_unavailable = false,
            // Keychain unavailable (headless): fall through to env so env keys
            // still work; a truly-missing key surfaces as the per-turn 401.
            Err(_) => store_unavailable = true,
        }
        if let Some(var) = self
            .sources
            .get(credential_id)
            .and_then(|s| s.env_var.as_deref())
        {
            if let Ok(val) = std::env::var(var) {
                return (
                    Some(Credential::ApiKey(val)),
                    Source::Environment {
                        variable: var.to_owned(),
                    },
                );
            }
        }
        (
            None,
            if store_unavailable {
                Source::Unknown
            } else {
                Source::None
            },
        )
    }
    async fn status_api_key(&self) -> Result<Option<Credential>, LlmError> {
        if let Some(key) = self
            .credentials
            .get_provider_key_ephemeral("anthropic-api-key")
            .await
        {
            return Ok(Some(Credential::ApiKey(key.expose_secret().clone())));
        }
        if let Some(key) = self
            .anthropic_api_key
            .as_ref()
            .filter(|key| !key.is_empty())
        {
            return Ok(Some(Credential::ApiKey(key.clone())));
        }
        // Native mb shadows the store with a configured helper, including
        // a cold cache. NQn reads it without executing or checking the TTL.
        if self.anthropic_api_key_helper.is_some() {
            return Ok(self
                .anthropic_api_key_helper_cache
                .snapshot()
                .map(Credential::ApiKey));
        }
        self.credentials
            .get_anthropic_api_key()
            .await
            .map(|key| key.map(|key| Credential::ApiKey(key.expose_secret().clone())))
            .map_err(|_| LlmError::Authentication {
                message: String::new(),
            })
    }
}

impl CredentialProvider for MultiCredentialProvider {
    fn source<'a>(&'a self, scope: &'a CredentialScope) -> BoxFuture<'a, Result<Source, LlmError>> {
        Box::pin(async move {
            let Some(credential_id) = scope.credential_id.as_deref() else {
                return Ok(Source::None);
            };
            if let Some(delegate) = self.oauth_delegates.get(credential_id) {
                return delegate.source(scope).await;
            }
            if credential_id == "anthropic-api-key" {
                if let Some((_, source)) = self.direct_anthropic_api_key().await {
                    return Ok(source);
                }
                // A configured helper shadows the store before its first call.
                // Inspection cannot run it or claim that a failed future call
                // will fall through to a particular stored credential.
                if self.anthropic_api_key_helper.is_some() {
                    return Ok(Source::ApiKeyHelper);
                }
                return Ok(match self.credentials.get_anthropic_api_key().await {
                    Ok(Some(_)) => Source::Stored,
                    Ok(None) => Source::None,
                    Err(_) => Source::Unknown,
                });
            }
            Ok(self.provider_key_selection(credential_id).await.1)
        })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            if scope.provider_id != llm_runtime::ProviderId::AnthropicFirstParty {
                return Ok(llm_runtime::AnthropicAuthSnapshot::default());
            }
            let oauth_scope = scope.clone().with_credential_id("anthropic-oauth");
            let oauth = if let Some(delegate) = self.oauth_delegates.get("anthropic-oauth") {
                delegate
                    .anthropic_auth_snapshot(&oauth_scope)
                    .await
                    .ok()
                    .and_then(|snapshot| snapshot.oauth)
            } else {
                None
            };
            // Native Xb suppresses key lookup failure without discarding OAuth.
            let api_key = self.status_api_key().await.unwrap_or(None);
            Ok(llm_runtime::AnthropicAuthSnapshot { oauth, api_key })
        })
    }
    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            let delegate = scope
                .credential_id
                .as_deref()
                .and_then(|id| self.oauth_delegates.get(id));
            match delegate {
                Some(delegate) => delegate.refresh(scope, rejected).await,
                None => Ok(None),
            }
        })
    }
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

    #[derive(Debug)]
    struct RefreshDelegate;
    impl CredentialProvider for RefreshDelegate {
        fn anthropic_auth_snapshot<'a>(
            &'a self,
            scope: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
            Box::pin(async move {
                Ok(llm_runtime::AnthropicAuthSnapshot::from_credential(
                    scope.clone(),
                    Credential::AnthropicOAuth {
                        access_token: "old".into(),
                        scopes: vec!["user:profile".into()],
                    },
                ))
            })
        }

        fn load<'a>(
            &'a self,
            _: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async {
                Ok(Credential::AnthropicOAuth {
                    access_token: "old".into(),
                    scopes: vec!["user:profile".into()],
                })
            })
        }
        fn refresh<'a>(
            &'a self,
            scope: &'a CredentialScope,
            rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
            Box::pin(async move {
                assert_eq!(scope.credential_id.as_deref(), Some("anthropic-oauth"));
                assert!(
                    matches!(rejected, Credential::AnthropicOAuth { access_token, .. } if access_token=="old")
                );
                Ok(Some(Credential::AnthropicOAuth {
                    access_token: "new".into(),
                    scopes: vec!["user:profile".into()],
                }))
            })
        }
    }
    #[tokio::test]
    async fn status_oauth_remains_independent_of_the_model_api_key() {
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![],
            Some("model-key".into()),
            None,
            BTreeMap::from([(
                "anthropic-oauth".into(),
                Arc::new(RefreshDelegate) as Arc<dyn CredentialProvider>,
            )]),
        );
        let model_scope = CredentialScope::new(ProviderId::AnthropicFirstParty, "anthropic")
            .with_credential_id("anthropic-api-key");
        assert_eq!(
            provider.load(&model_scope).await.unwrap(),
            Credential::ApiKey("model-key".into())
        );
        let snapshot = provider
            .anthropic_auth_snapshot(&model_scope)
            .await
            .unwrap();
        assert_eq!(
            snapshot.api_key,
            Some(Credential::ApiKey("model-key".into()))
        );
        let (oauth_scope, credential) = snapshot.oauth.unwrap();
        assert_eq!(
            oauth_scope.credential_id.as_deref(),
            Some("anthropic-oauth")
        );
        assert_eq!(oauth_scope.profile_name, model_scope.profile_name);
        assert!(
            matches!(credential, Credential::AnthropicOAuth {ref access_token, ..} if access_token == "old")
        );
        assert!(provider
            .refresh(&oauth_scope, &credential)
            .await
            .unwrap()
            .is_some());
        assert!(!format!("{credential:?}").contains("old"));
    }

    #[tokio::test]
    async fn explicit_refresh_reaches_only_the_selected_oauth_delegate() {
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![],
            None,
            None,
            BTreeMap::from([(
                "anthropic-oauth".into(),
                Arc::new(RefreshDelegate) as Arc<dyn CredentialProvider>,
            )]),
        );
        let scope = CredentialScope::new(ProviderId::AnthropicFirstParty, "anthropic")
            .with_credential_id("anthropic-oauth");
        let old = provider.load(&scope).await.unwrap();
        assert!(
            matches!(provider.refresh(&scope, &old).await.unwrap(), Some(Credential::AnthropicOAuth { access_token, .. }) if access_token=="new")
        );
        let other =
            CredentialScope::new(ProviderId::OpenAI, "other").with_credential_id("unregistered");
        assert_eq!(provider.refresh(&other, &old).await.unwrap(), None);
    }

    #[tokio::test]
    async fn native_fast_key_snapshots_preserve_helper_shadowing_without_execution() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../llm-runtime/tests/fixtures/fast_auth_sources_2_1_287.json"
        ))
        .unwrap();
        let marker = std::env::temp_dir().join(format!(
            "harness-fast-helper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let marker_quoted = marker.to_string_lossy().replace('\'', "'\\''");
        for row in fixture["key_sources"].as_array().unwrap() {
            let input = &row["input"];
            let credentials = manager();
            if let Some(key) = input["stored"].as_str() {
                credentials.store_anthropic_api_key(key).await.unwrap();
            }
            let provider = MultiCredentialProvider::new(
                credentials,
                vec![],
                input["static_key"].as_str().map(str::to_owned),
                input["helper"]
                    .as_bool()
                    .unwrap()
                    .then(|| format!("touch '{marker_quoted}'; printf unexpected")),
                BTreeMap::new(),
            );
            if let Some(key) = input["cached"].as_str() {
                provider.anthropic_api_key_helper_cache.store(key.into());
                assert!(provider
                    .anthropic_api_key_helper_cache
                    .get_fresh(0)
                    .is_none());
            }
            let current_scope = scope(
                ProviderId::AnthropicFirstParty,
                "anthropic",
                "anthropic-oauth",
            );
            let actual = provider
                .anthropic_auth_snapshot(&current_scope)
                .await
                .unwrap()
                .api_key;
            let actual = actual.map(|credential| match credential {
                Credential::ApiKey(key) => key,
                other => panic!("unexpected alternate credential: {other:?}"),
            });
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                row["expected"],
                "{row}"
            );
            assert!(!marker.exists(), "Fast snapshots must not execute helpers");
            assert_eq!(
                provider
                    .anthropic_auth_snapshot(&scope(ProviderId::OpenAI, "other", "other"))
                    .await
                    .unwrap()
                    .api_key,
                None
            );
        }
    }

    /// Shared in-memory `CredentialManager` harness (re-used by availability.rs too).
    #[derive(Default)]
    struct MemStorage {
        map: std::sync::Mutex<
            std::collections::HashMap<(String, String), lingxi_core::types::SecureStorageData>,
        >,
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::SecureStorage for MemStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: lingxi_core::types::SecureStorageData,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
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
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
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
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.map
                .lock()
                .unwrap()
                .remove(&(service.into(), account.into()));
            Ok(())
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
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
        fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
            lingxi_core::host::SecureStorageBackend::PlainText
        }
    }
    struct FixedClock;
    impl lingxi_core::host::Clock for FixedClock {
        fn now(&self) -> std::time::SystemTime {
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000)
        }
    }
    struct NoHttp;
    #[async_trait::async_trait]
    impl lingxi_core::host::HttpTransport for NoHttp {
        async fn request(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::types::HttpResponse, lingxi_core::host::HttpError> {
            panic!("credential tests must not perform HTTP");
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
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
    async fn credential_source_matches_selected_profile_key_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let variable = "CREDENTIAL_SOURCE_REGRESSION_KEY";
        let previous = std::env::var_os(variable);
        std::env::set_var(variable, "environment-credential");
        let credentials = manager();
        credentials
            .set_provider_key("custom-key", "stored-credential")
            .await
            .unwrap();
        let provider = MultiCredentialProvider::new(
            credentials.clone(),
            vec![source(
                ProviderId::OpenAI,
                "custom-key",
                Some(variable),
                crate::CredentialKind::ApiKey,
            )],
            None,
            None,
            Default::default(),
        );
        let selected = scope(ProviderId::OpenAI, "custom-profile", "custom-key");
        assert_eq!(provider.source(&selected).await.unwrap(), Source::Stored);
        assert_eq!(
            provider.load(&selected).await.unwrap(),
            Credential::ApiKey("stored-credential".into())
        );
        credentials.delete_provider_key("custom-key").await.unwrap();
        assert_eq!(
            provider.source(&selected).await.unwrap(),
            Source::Environment {
                variable: variable.into()
            }
        );
        assert_eq!(
            provider.load(&selected).await.unwrap(),
            Credential::ApiKey("environment-credential".into())
        );
        std::env::remove_var(variable);
        assert_eq!(provider.source(&selected).await.unwrap(), Source::None);
        assert!(provider.load(&selected).await.is_err());
        match previous {
            Some(value) => std::env::set_var(variable, value),
            None => std::env::remove_var(variable),
        }
    }

    #[tokio::test]
    async fn credential_source_keeps_helper_precedence_without_executing_it() {
        let credentials = manager();
        credentials
            .store_anthropic_api_key("stored-credential")
            .await
            .unwrap();
        let marker = std::env::temp_dir().join(format!(
            "harness-credential-source-helper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let provider = MultiCredentialProvider::new(
            credentials,
            vec![],
            None,
            Some(format!("touch '{}'", marker.display())),
            Default::default(),
        );
        let selected = scope(
            ProviderId::AnthropicFirstParty,
            "anthropic",
            "anthropic-api-key",
        );
        assert_eq!(
            provider.source(&selected).await.unwrap(),
            Source::ApiKeyHelper
        );
        assert!(
            !marker.exists(),
            "metadata inspection must not run a helper"
        );
    }

    #[tokio::test]
    async fn credential_source_retains_host_supplied_static_origin() {
        let provider = MultiCredentialProvider::new(
            manager(),
            vec![],
            Some("configured-credential".into()),
            None,
            Default::default(),
        )
        .with_anthropic_api_key_source(Source::Environment {
            variable: "ANTHROPIC_API_KEY".into(),
        });
        let selected = scope(
            ProviderId::AnthropicFirstParty,
            "anthropic",
            "anthropic-api-key",
        );
        assert_eq!(
            provider.source(&selected).await.unwrap(),
            Source::Environment {
                variable: "ANTHROPIC_API_KEY".into()
            }
        );
    }

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
