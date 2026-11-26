//! Hosted search and fetch operations.

use crate::service::ApiService;
use crate::{LlmError, ModelRuntime};
use lingxi_llm_client::providers::deepseek::web::{DeepSeekFetchRequest, DeepSeekFetchResponse};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEEPSEEK_WEB_TIMEOUT: Duration = Duration::from_secs(60);
const FETCH_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const FETCH_CACHE_MAX_BYTES: usize = 50 * 1024 * 1024;
const FETCH_CACHE_MAX_ENTRIES: usize = 64;

#[derive(PartialEq, Eq)]
struct DeepSeekFetchKey {
    profile: String,
    account: Option<crate::accounts::AccountResourceScope>,
    endpoint: String,
    credential_revision: Option<u64>,
    credential_digest: [u8; 32],
    url: String,
}

struct CachedPage {
    key: DeepSeekFetchKey,
    page: DeepSeekFetchResponse,
    inserted: Instant,
    fetched_at_ms: u64,
}

impl CachedPage {
    fn byte_size(&self) -> usize {
        self.page.title.len() + self.page.content.len() + self.key.url.len()
    }
}

#[derive(Default)]
pub(super) struct DeepSeekFetchCache(VecDeque<CachedPage>);

impl DeepSeekFetchCache {
    fn get(&mut self, key: &DeepSeekFetchKey) -> Option<(DeepSeekFetchResponse, u64)> {
        self.0
            .retain(|entry| entry.inserted.elapsed() < FETCH_CACHE_TTL);
        let index = self.0.iter().position(|entry| &entry.key == key)?;
        let entry = self.0.remove(index)?;
        let result = (entry.page.clone(), entry.fetched_at_ms);
        self.0.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, entry: CachedPage) {
        if entry.page.content.is_empty() || entry.byte_size() > FETCH_CACHE_MAX_BYTES {
            return;
        }
        self.0
            .retain(|old| old.key != entry.key && old.inserted.elapsed() < FETCH_CACHE_TTL);
        let mut bytes = self.0.iter().map(CachedPage::byte_size).sum::<usize>() + entry.byte_size();
        while bytes > FETCH_CACHE_MAX_BYTES || self.0.len() >= FETCH_CACHE_MAX_ENTRIES {
            let Some(old) = self.0.pop_front() else {
                break;
            };
            bytes -= old.byte_size();
        }
        self.0.push_back(entry);
    }
}

struct DeepSeekWebOperation {
    client: lingxi_llm_client::providers::DeepSeekClient,
    options: lingxi_llm_client::RequestOptions,
    source: Arc<ModelRuntime>,
    profile: String,
    credential_revision: Option<u64>,
    coordinator: Option<crate::accounts::AttemptCoordinator>,
}

#[derive(Clone, Copy)]
struct DeepSeekWebDeadline(tokio::time::Instant);

impl DeepSeekWebDeadline {
    fn new() -> Self {
        Self(tokio::time::Instant::now() + DEEPSEEK_WEB_TIMEOUT)
    }

    fn elapsed() -> LlmError {
        LlmError::TransportTimeout {
            message: "DeepSeek web operation deadline elapsed".into(),
        }
    }

    fn remaining(self) -> Result<Duration, LlmError> {
        self.0
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(Self::elapsed)
    }

    async fn run<T>(self, operation: impl Future<Output = T>) -> Result<T, LlmError> {
        self.remaining()?;
        tokio::time::timeout_at(self.0, operation)
            .await
            .map_err(|_| Self::elapsed())
    }
}

impl DeepSeekWebOperation {
    async fn settle_sdk<T>(
        &mut self,
        result: Result<T, lingxi_llm_client::protocol::LlmError>,
        deadline: DeepSeekWebDeadline,
    ) -> Result<T, LlmError> {
        let retry_after = result
            .as_ref()
            .err()
            .and_then(crate::services::sdk_retry_after);
        let result = result.map_err(crate::upstream::error);
        self.settle(result.as_ref().err(), retry_after, deadline)
            .await;
        result
    }

    async fn settle(
        &mut self,
        error: Option<&LlmError>,
        retry_after: Option<Duration>,
        deadline: DeepSeekWebDeadline,
    ) {
        // Return an observed provider outcome even if its fenced health write
        // cannot finish in the remaining time.
        let _ = deadline.run(self.record_health(error, retry_after)).await;
    }

    async fn record_health(&mut self, error: Option<&LlmError>, retry_after: Option<Duration>) {
        let Some(coordinator) = self.coordinator.as_mut() else {
            return;
        };
        match error {
            None => coordinator.succeeded().await,
            Some(error) => {
                if matches!(
                    error.classification_source(),
                    LlmError::Authentication { .. } | LlmError::OAuthRefreshDead
                ) && self
                    .source
                    .current_service_credential_revision(&self.profile)
                    .await
                    .ok()
                    != Some(self.credential_revision)
                {
                    return;
                }
                coordinator.set_rejected_credential_revision(self.credential_revision);
                let _ = coordinator
                    .failed_with_retry_after(error, retry_after)
                    .await;
            }
        }
    }
}

impl ApiService {
    pub fn uses_deepseek_web(&self, model: &str, profile: Option<&str>) -> bool {
        self.client().deepseek_web_profile(model, profile).is_some()
    }

    async fn deepseek_web_client(
        &self,
        model: &str,
        profile: Option<&str>,
        operation: &'static str,
        deadline: DeepSeekWebDeadline,
    ) -> Result<DeepSeekWebOperation, LlmError> {
        if self.scope_output_quota.is_some() {
            return Err(LlmError::UnsupportedCapability {
                capability: "independent web requests are disabled for output-limited scopes"
                    .into(),
            });
        }
        // One operation owns one current directory, route and credential source.
        // The service lifetime can span account/catalog publications.
        let operation_client = deadline.run(self.client.account_call_snapshot()).await??;
        let profile = operation_client
            .deepseek_web_profile(model, profile)
            .ok_or_else(|| LlmError::UnsupportedCapability {
                capability: "independent DeepSeek web APIs require a first-party route".into(),
            })?;
        let mut coordinator = operation_client.service_account_coordinator(&profile, operation)?;
        if let Some(coordinator) = coordinator.as_mut() {
            coordinator.retain_supported(|candidate| {
                (!profile.starts_with("account.") || candidate.profile_name == profile)
                    && operation_client
                        .deepseek_web_profile(model, Some(&candidate.profile_name))
                        .is_some()
            });
        }
        let profile = match coordinator.as_mut() {
            Some(coordinator) => deadline.run(coordinator.select()).await??.profile_name,
            None => profile,
        };
        let services = operation_client
            .provider_services(
                operation_client.deepseek_web_region(&profile)?,
                self.transport.clone(),
            )
            .map_err(|error| LlmError::InvalidRequest {
                message: error.to_string(),
            })?;
        let client = services
            .client()
            .provider::<lingxi_llm_client::providers::DeepSeekClient>(&profile)
            .map_err(|error| LlmError::InvalidRequest {
                message: error.to_string(),
            })?;
        let credential = deadline
            .run(operation_client.service_credential_versioned(&profile))
            .await?;
        let (credential, credential_revision) = match credential {
            Ok(credential) => credential,
            Err(failure) => {
                let mut failed = DeepSeekWebOperation {
                    client,
                    options: Default::default(),
                    source: operation_client,
                    profile,
                    credential_revision: failure.credential_revision,
                    coordinator,
                };
                failed.settle(Some(&failure.error), None, deadline).await;
                return Err(failure.error);
            }
        };
        let options = lingxi_llm_client::RequestOptions {
            credential: Some(credential),
            ..Default::default()
        };
        Ok(DeepSeekWebOperation {
            client,
            options,
            source: operation_client,
            profile,
            credential_revision,
            coordinator,
        })
    }

    /// One independent request; retries and turn generation are not involved.
    pub async fn deepseek_search(
        &self,
        model: &str,
        profile: Option<&str>,
        request: &lingxi_llm_client::providers::deepseek::web::DeepSeekSearchRequest,
    ) -> Result<lingxi_llm_client::providers::deepseek::web::DeepSeekSearchResponse, LlmError> {
        let deadline = DeepSeekWebDeadline::new();
        request.validate().map_err(crate::upstream::error)?;
        let mut operation = self
            .deepseek_web_client(model, profile, "web/search", deadline)
            .await?;
        operation.options.total_timeout = Some(deadline.remaining()?);
        let result = deadline
            .run(operation.client.web().search(request, &operation.options))
            .await?;
        operation.settle_sdk(result, deadline).await
    }

    pub async fn deepseek_fetch(
        &self,
        model: &str,
        profile: Option<&str>,
        request: &DeepSeekFetchRequest,
    ) -> Result<DeepSeekFetchResponse, LlmError> {
        self.deepseek_fetch_with_cache(model, profile, request, false)
            .await
            .map(|(response, _)| response)
    }

    /// Returns the original fetch time on a cache hit. Resolve the account and
    /// current credential first so neither failover nor rotation reuses a page
    /// from a different provider authorization.
    pub async fn deepseek_fetch_with_cache(
        &self,
        model: &str,
        profile: Option<&str>,
        request: &DeepSeekFetchRequest,
        use_cache: bool,
    ) -> Result<(DeepSeekFetchResponse, Option<u64>), LlmError> {
        let deadline = DeepSeekWebDeadline::new();
        request.validate().map_err(crate::upstream::error)?;
        let mut operation = self
            .deepseek_web_client(model, profile, "web/fetch", deadline)
            .await?;
        let cache_hit_allowed = !operation
            .coordinator
            .as_ref()
            .is_some_and(crate::accounts::AttemptCoordinator::active_is_recovery_probe);
        let key = if use_cache {
            operation
                .source
                .search_profile(model, Some(&operation.profile))
                .zip(operation.options.credential.as_ref())
                .map(|(profile, credential)| DeepSeekFetchKey {
                    profile: operation.profile.clone(),
                    account: operation
                        .coordinator
                        .as_ref()
                        .and_then(crate::accounts::AttemptCoordinator::active_scope),
                    endpoint: profile.base_url.clone(),
                    credential_revision: operation.credential_revision,
                    credential_digest: Sha256::digest(credential.expose_secret().as_bytes()).into(),
                    url: request.url.clone(),
                })
        } else {
            None
        };
        if let Some(key) = key.as_ref().filter(|_| cache_hit_allowed) {
            if let Some((page, fetched_at_ms)) = deadline
                .run(self.deepseek_fetch_cache.lock())
                .await?
                .get(key)
            {
                return Ok((page, Some(fetched_at_ms)));
            }
        }
        operation.options.total_timeout = Some(deadline.remaining()?);
        let result = deadline
            .run(operation.client.web().fetch(request, &operation.options))
            .await?;
        let page = operation.settle_sdk(result, deadline).await?;
        if let Some(key) = key {
            if let Ok(mut cache) = deadline.run(self.deepseek_fetch_cache.lock()).await {
                cache.insert(CachedPage {
                    key,
                    page: page.clone(),
                    inserted: Instant::now(),
                    fetched_at_ms: u64::try_from(Self::now_ms()).unwrap_or(u64::MAX),
                });
            }
        }
        Ok((page, None))
    }

    pub fn hosted_search_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<crate::ResolvedRoute, LlmError> {
        self.client().hosted_search_route(model, profile)
    }

    pub fn uses_codex_standalone_search(&self, model: &str, profile: Option<&str>) -> bool {
        self.client()
            .search_profile(model, profile)
            .is_some_and(lingxi_llm_client::providers::openai::codex_search::supports)
    }

    pub fn supports_hosted_search(&self, model: &str, profile: Option<&str>) -> bool {
        self.uses_deepseek_web(model, profile)
            || self.supports_hosted_search_config(model, profile, &Default::default())
    }

    pub fn supports_hosted_search_config(
        &self,
        model: &str,
        profile: Option<&str>,
        config: &lingxi_llm_client::protocol::WebSearchConfig,
    ) -> bool {
        if self.uses_deepseek_web(model, profile) {
            return config.max_uses.is_none();
        }
        let Ok(route) = self.hosted_search_route(model, profile) else {
            return false;
        };
        self.client()
            .search_profile(model, profile)
            .is_some_and(|profile| {
                lingxi_llm_client::hosted_search::supports_model_with_config(
                    profile,
                    &route.request_model,
                    config,
                )
            })
    }

    pub fn hosted_search_max_uses(&self, model: &str, profile: Option<&str>) -> Option<u32> {
        self.client()
            .search_profile(model, profile)
            .filter(|p| {
                p.extra
                    .get("web_search")
                    .and_then(serde_json::Value::as_str)
                    == Some("anthropic")
            })
            .map(|_| 8)
    }

    pub(super) fn log_deepseek_prepared_request(
        model: &str,
        prepared: &crate::PreparedLlmCall,
        stream: bool,
    ) {
        if !tracing::enabled!(target: "llm_runtime::service", tracing::Level::DEBUG) {
            return;
        }
        let body = &prepared.provider_request.body_json;
        let is_deepseek = model.contains("deepseek")
            || prepared.provider_request.url.contains("api.deepseek.com")
            || body
                .string_at("/model")
                .is_some_and(|model| model.contains("deepseek"));
        if !is_deepseek {
            return;
        }

        let messages = crate::protocol::json_array_items(body, "/messages").unwrap_or_default();
        let messages_with_reasoning_content = messages
            .iter()
            .filter(|message| {
                lingxi_llm_client::providers::anthropic::request_policy::has_field_path(
                    message,
                    &["reasoning_content"],
                )
            })
            .count();
        let last_assistant = messages
            .iter()
            .rev()
            .find(|message| message.string_at("/role").as_deref() == Some("assistant"));
        let last_assistant_reasoning_len = last_assistant
            .and_then(|message| message.string_at("/reasoning_content"))
            .map_or(0, |text| text.len());
        tracing::debug!(
            target = "llm_runtime::service",
            event = "deepseek_prepared_request",
            stream = stream,
            model = %model,
            provider_url = %prepared.provider_request.url,
            message_count = messages.len(),
            messages_with_reasoning_content = messages_with_reasoning_content,
            last_assistant_reasoning_len = last_assistant_reasoning_len,
            tool_choice = ?body.raw_source_at("/tool_choice"),
            tools = body.array_len("/tools").unwrap_or_default(),
            thinking = ?body.raw_source_at("/thinking"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::{
        AccountPool, AccountPoolStore, FailureScope, MemoryAccountPoolStore, PoolOperation,
        PoolResult,
    };
    use crate::{ClientConfig, Credential, CredentialConfig, CredentialProvider, CredentialScope};
    use futures::future::BoxFuture;
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, AccountMetadata, AccountSnapshot, CredentialRef,
    };
    use lingxi_llm_client::providers::deepseek::web::DeepSeekSearchRequest;
    use lingxi_llm_client::{HttpRequest, HttpResponse, StreamResponse};
    use std::sync::atomic::{AtomicU16, Ordering};

    const MODEL: &str = "deepseek-flash";
    const PROFILE: Option<&str> = Some("deepseek");

    struct WebTransport(AtomicU16);

    #[async_trait::async_trait]
    impl crate::Transport for WebTransport {
        async fn send(
            &self,
            request: HttpRequest,
        ) -> Result<StreamResponse, lingxi_llm_client::protocol::LlmError> {
            let body = match request.url.as_str() {
                "https://api.deepseek.com/v1/tools/search" => r#"{"results":[]}"#,
                "https://api.deepseek.com/v1/tools/fetch" => {
                    r#"{"title":"Article","content":"Page"}"#
                }
                _ => panic!("unexpected request to {}", request.url),
            };
            Ok(HttpResponse {
                status: self.0.load(Ordering::Relaxed),
                headers: vec![("retry-after".into(), "3600".into())],
                body: body.into(),
            }
            .into())
        }
    }

    #[derive(Debug)]
    struct TestCredential;

    impl CredentialProvider for TestCredential {
        fn load<'a>(
            &'a self,
            _: &'a CredentialScope,
        ) -> BoxFuture<'a, Result<Credential, LlmError>> {
            Box::pin(async { Ok(Credential::ApiKey("test-key".into())) })
        }
    }

    fn service(store: Arc<dyn AccountPoolStore>) -> (ApiService, Arc<WebTransport>) {
        let mut template = crate::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "deepseek")
            .unwrap();
        // Exercise both independent operations on a host-admitted mainland-only route.
        template.regions = vec![crate::Region::ChinaMainland];
        template.connection.group = Some("deepseek".into());
        let mut account = template.clone();
        account.profile_name = "account.a".into();
        account.connection.connection_id = Some("a".into());
        account.credential = CredentialConfig::Static { id: "test".into() };
        let pool = AccountPool::new(
            vec![AccountMetadata {
                account_id: "a".into(),
                provider_id: "deepseek".into(),
                display_name: "a".into(),
                auth_kind: AccountAuthKind::ApiKey,
                execution_binding_id: "deepseek".into(),
                credential_ref: CredentialRef {
                    store_scope: "test".into(),
                    secret_ref: "test".into(),
                },
                enabled: true,
                additional_openai_models: false,
                user_priority: 0,
                config_revision: 1,
                identity_generation: 1,
            }],
            store,
        );
        let runtime = ModelRuntime::from_config(ClientConfig {
            providers: vec![template, account],
        })
        .unwrap()
        .with_credential_provider(Arc::new(TestCredential))
        .with_account_pool(Arc::new(pool));
        let transport = Arc::new(WebTransport(AtomicU16::new(200)));
        let service = ApiService::new(
            Arc::new(runtime),
            transport.clone(),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        );
        (service, transport)
    }

    #[tokio::test]
    async fn deepseek_web_quota_keeps_the_provider_recovery_time() {
        for search in [true, false] {
            let (service, transport) = service(Arc::new(MemoryAccountPoolStore::default()));
            transport.0.store(402, Ordering::Relaxed);
            let start_ms = u64::try_from(ApiService::now_ms()).unwrap();
            let error = if search {
                service
                    .deepseek_search(MODEL, PROFILE, &DeepSeekSearchRequest::new("test"))
                    .await
                    .unwrap_err()
            } else {
                service
                    .deepseek_fetch(
                        MODEL,
                        PROFILE,
                        &DeepSeekFetchRequest::new("https://example.com"),
                    )
                    .await
                    .unwrap_err()
            };
            assert!(matches!(
                error.classification_source(),
                LlmError::QuotaExceeded
            ));
            let end_ms = u64::try_from(ApiService::now_ms()).unwrap();
            let error = service
                .client()
                .account_coordinator(MODEL, PROFILE)
                .unwrap()
                .unwrap()
                .select()
                .await
                .unwrap_err();
            assert!(matches!(error, LlmError::AllAccountsUnavailable {
                earliest_recovery_ms: Some(t), ..
            } if (start_ms + 3_600_000..=end_ms + 3_600_000).contains(&t)));
        }
    }

    #[tokio::test]
    async fn deepseek_web_isolates_endpoint_health_and_shares_account_failures() {
        let (service, transport) = service(Arc::new(MemoryAccountPoolStore::default()));
        let search = DeepSeekSearchRequest::new("test");
        let fetch = DeepSeekFetchRequest::new("https://example.com");

        transport.0.store(500, Ordering::Relaxed);
        assert!(matches!(
            service
                .deepseek_search(MODEL, PROFILE, &search)
                .await
                .unwrap_err()
                .classification_source(),
            LlmError::ProviderInternal
        ));
        let chat = || {
            service
                .client()
                .account_coordinator(MODEL, PROFILE)
                .unwrap()
                .unwrap()
        };
        let mut attempt = chat();
        attempt.select().await.unwrap();
        assert!(attempt.failed(&LlmError::ProviderInternal).await);

        transport.0.store(200, Ordering::Relaxed);
        service
            .deepseek_fetch(MODEL, PROFILE, &fetch)
            .await
            .unwrap();
        assert!(matches!(
            chat().select().await,
            Err(LlmError::AllAccountsUnavailable { accounts, .. })
                if matches!(&accounts[0].scope, FailureScope::Endpoint(_))
        ));
        assert!(matches!(
            service.deepseek_search(MODEL, PROFILE, &search).await,
            Err(LlmError::AllAccountsUnavailable { accounts, .. })
                if matches!(&accounts[0].scope, FailureScope::Endpoint(_))
        ));

        transport.0.store(401, Ordering::Relaxed);
        assert!(matches!(
            service
                .deepseek_fetch(MODEL, PROFILE, &fetch)
                .await
                .unwrap_err()
                .classification_source(),
            LlmError::Authentication { .. }
        ));
        assert!(matches!(
            chat().select().await,
            Err(LlmError::AllAccountsUnavailable { accounts, .. })
                if accounts[0].scope == FailureScope::Account
        ));
    }

    #[derive(Debug)]
    struct DelayedStore {
        refresh_delay: Duration,
        inner: MemoryAccountPoolStore,
    }

    #[async_trait::async_trait]
    impl AccountPoolStore for DelayedStore {
        async fn latest_snapshot(&self) -> Result<Option<AccountSnapshot>, String> {
            tokio::time::sleep(self.refresh_delay).await;
            Ok(None)
        }

        async fn transact(&self, operation: PoolOperation) -> Result<PoolResult, String> {
            if matches!(operation, PoolOperation::Finish { .. }) {
                return std::future::pending().await;
            }
            self.inner.transact(operation).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn deepseek_web_deadline_covers_snapshot_and_preserves_observed_result() {
        for (refresh_seconds, status) in [(65, 200), (25, 200), (25, 500)] {
            let (service, transport) = service(Arc::new(DelayedStore {
                refresh_delay: Duration::from_secs(refresh_seconds),
                inner: Default::default(),
            }));
            transport.0.store(status, Ordering::Relaxed);
            let started = tokio::time::Instant::now();
            let result = service
                .deepseek_search(MODEL, PROFILE, &DeepSeekSearchRequest::new("test"))
                .await;
            assert_eq!(started.elapsed(), DEEPSEEK_WEB_TIMEOUT);
            if refresh_seconds > DEEPSEEK_WEB_TIMEOUT.as_secs() {
                assert!(matches!(result, Err(LlmError::TransportTimeout { .. })));
            } else if status == 200 {
                assert!(result.unwrap().results.is_empty());
            } else {
                assert!(matches!(
                    result.unwrap_err().classification_source(),
                    LlmError::ProviderInternal
                ));
            }
        }
    }
}
