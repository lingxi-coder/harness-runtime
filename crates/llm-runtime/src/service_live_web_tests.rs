//! Independent web calls follow the live catalog while each call owns one snapshot.

use super::ApiService;
use crate::client::ModelRuntimeSource;
use crate::{
    BoxFuture, ClientConfig, Credential, CredentialProvider, CredentialScope, LlmError,
    ModelRuntime, ProviderProfile, SubscriberState,
};
use lingxi_llm_client as sdk;
use sdk::providers::deepseek::web::{DeepSeekFetchRequest, DeepSeekSearchRequest};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, RwLock,
};

#[derive(Debug)]
struct LiveWebCatalog(RwLock<Arc<ModelRuntime>>);

impl ModelRuntimeSource for LiveWebCatalog {
    fn current(&self, _: &ModelRuntime) -> Arc<ModelRuntime> {
        self.0.read().unwrap().clone()
    }

    fn rebind_accounts(&self, _: &ModelRuntime) -> Arc<ModelRuntime> {
        self.0.read().unwrap().clone()
    }
}

impl LiveWebCatalog {
    fn publish(&self, runtime: ModelRuntime) {
        *self.0.write().unwrap() = Arc::new(runtime);
    }
}

#[derive(Debug, Default)]
struct WebCredentials {
    pause_next: AtomicBool,
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

impl CredentialProvider for WebCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            if self.pause_next.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.released.notified().await;
            }
            Ok(Credential::BearerToken(format!(
                "synthetic-{}",
                scope.credential_id.as_deref().unwrap()
            )))
        })
    }
}

#[derive(Default)]
struct WebTransport(Mutex<Vec<sdk::HttpRequest>>);

#[async_trait::async_trait]
impl crate::Transport for WebTransport {
    async fn send(
        &self,
        request: sdk::HttpRequest,
    ) -> Result<sdk::StreamResponse, sdk::protocol::LlmError> {
        let response = if request.url.contains("/count_tokens") {
            serde_json::json!({"input_tokens":37})
        } else if request.url.ends_with("/fetch") {
            serde_json::json!({"title":"Fixture article", "content":"Fixture body"})
        } else {
            serde_json::json!({"results":[]})
        };
        self.0.lock().unwrap().push(request);
        Ok(sdk::HttpResponse {
            status: 200,
            headers: vec![],
            body: response.to_string().into(),
        }
        .into())
    }
}

fn web_profile(name: &str, credential: &str, base_url: &str) -> ProviderProfile {
    let mut profile: ProviderProfile = serde_json::from_value(serde_json::json!({
        "provider_id":{"open_ai_compatible":{"name":"deepseek"}},
        "profile_name": name,
        "base_url": base_url,
        "protocol":"open_ai_chat",
        "auth":"bearer",
        "credential":{"type":"host_managed","id":credential},
        "models":[{"display_model":"deepseek-flash", "request_model":"deepseek-flash",
                   "billing_model":"deepseek-flash"}]
    }))
    .unwrap();
    profile.wire_profile = sdk::builtin_providers()
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.profile_name == "deepseek");
    assert!(profile.wire_profile.is_some());
    profile
}

fn web_runtime(profiles: Vec<ProviderProfile>, credentials: Arc<WebCredentials>) -> ModelRuntime {
    ModelRuntime::from_config(ClientConfig {
        providers: profiles,
    })
    .unwrap()
    .with_credential_provider(credentials)
}

fn web_fixture() -> (
    Arc<ApiService>,
    Arc<LiveWebCatalog>,
    Arc<WebCredentials>,
    Arc<WebTransport>,
) {
    let credentials = Arc::new(WebCredentials::default());
    let initial = web_runtime(
        vec![web_profile(
            "account-a",
            "account-a-v1",
            "https://api.deepseek.com",
        )],
        credentials.clone(),
    );
    let source = Arc::new(LiveWebCatalog(RwLock::new(Arc::new(initial.clone()))));
    let client = Arc::new(initial.with_catalog_source(source.clone()));
    let transport = Arc::new(WebTransport::default());
    let service = Arc::new(ApiService::new(
        client,
        transport.clone(),
        SubscriberState::default(),
        crate::model::user_agent::UserAgentEnv::default(),
        "live-web-catalog-regression",
        None,
        None,
    ));
    (service, source, credentials, transport)
}

pub(super) fn pooled_runtime(
    mut template: ProviderProfile,
    credentials: Arc<dyn CredentialProvider>,
) -> ModelRuntime {
    use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, CredentialRef};
    let binding = template.profile_name.clone();
    template.connection.group = Some(binding.clone());
    let mut profiles = vec![template.clone()];
    let mut accounts = Vec::new();
    for (priority, id) in ["a", "b"].into_iter().enumerate() {
        let mut profile = template.clone();
        profile.profile_name = format!("account.{id}");
        profile.connection.connection_id = Some(id.into());
        profile.connection.order = priority as u32;
        profile.credential = crate::CredentialConfig::Static {
            id: format!("acct.{id}"),
        };
        profiles.push(profile);
        accounts.push(AccountMetadata {
            account_id: id.into(),
            provider_id: binding.clone(),
            display_name: id.into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: binding.clone(),
            credential_ref: CredentialRef {
                store_scope: "synthetic".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: priority as u32,
            config_revision: 1,
            identity_generation: 1,
        });
    }
    ModelRuntime::from_config(ClientConfig {
        providers: profiles,
    })
    .unwrap()
    .with_credential_provider(credentials)
    .with_account_pool(Arc::new(crate::accounts::AccountPool::new(
        accounts,
        Arc::new(crate::accounts::MemoryAccountPoolStore::default()),
    )))
}

#[tokio::test]
async fn visible_deepseek_web_profile_selects_a_physical_account_for_search_and_fetch() {
    let runtime = pooled_runtime(
        web_profile(
            "deepseek",
            "unselected-template",
            "https://api.deepseek.com",
        ),
        Arc::new(WebCredentials::default()),
    );
    let transport = Arc::new(WebTransport::default());
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        SubscriberState::default(),
        crate::model::user_agent::UserAgentEnv::default(),
        "pooled-web",
        None,
        None,
    );
    search(&service, "deepseek").await.unwrap();
    service
        .deepseek_fetch(
            "deepseek-flash",
            Some("deepseek"),
            &DeepSeekFetchRequest::new("https://example.com/fixture"),
        )
        .await
        .unwrap();
    let requests = transport.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url, "https://api.deepseek.com/v1/tools/search");
    assert_eq!(requests[1].url, "https://api.deepseek.com/v1/tools/fetch");
    for request in requests.iter() {
        assert_eq!(request_authorization(request), "Bearer synthetic-acct.a");
    }
}

#[derive(Debug)]
pub(super) struct ApiKeyCredentials;
impl CredentialProvider for ApiKeyCredentials {
    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            Ok(Credential::ApiKey(format!(
                "synthetic-{}",
                scope.credential_id.as_deref().unwrap()
            )))
        })
    }
}

#[tokio::test]
async fn public_anthropic_token_counts_select_the_authorized_physical_account() {
    let template = crate::builtin_presets()
        .providers
        .into_iter()
        .find(|profile| profile.profile_name == "anthropic")
        .unwrap();
    let model = template.models[0].request_model.clone();
    let runtime = pooled_runtime(template, Arc::new(ApiKeyCredentials));
    let transport = Arc::new(WebTransport::default());
    let service = ApiService::new(
        Arc::new(runtime),
        transport.clone(),
        SubscriberState::default(),
        crate::model::user_agent::UserAgentEnv::default(),
        "pooled-token-count",
        None,
        None,
    );
    for profile in ["anthropic", "account.b"] {
        assert_eq!(
            service
                .count_tokens(
                    &model,
                    Some(profile),
                    Some("system"),
                    Vec::new(),
                    Vec::new()
                )
                .await
                .unwrap(),
            37
        );
        assert_eq!(
            service
                .count_tokens_exact(
                    &model,
                    Some(profile),
                    Some("system"),
                    Vec::new(),
                    Vec::new()
                )
                .await
                .unwrap(),
            Some(37)
        );
    }
    let requests = transport.0.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for (index, request) in requests.iter().enumerate() {
        assert!(request.url.contains("/messages/count_tokens"));
        assert_eq!(
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
                .map(|(_, value)| value.as_str()),
            Some(if index < 2 {
                "synthetic-acct.a"
            } else {
                "synthetic-acct.b"
            })
        );
    }
}

async fn search(service: &ApiService, profile: &str) -> Result<(), LlmError> {
    service
        .deepseek_search(
            "deepseek-flash",
            Some(profile),
            &DeepSeekSearchRequest::new("fixture query"),
        )
        .await
        .map(|_| ())
}

fn request_authorization(request: &sdk::HttpRequest) -> &str {
    request
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.as_str())
        .unwrap()
}

#[tokio::test]
async fn independent_web_search_and_fetch_admit_accounts_added_after_warmup() {
    crate::catalog_task::scope(async {
        let (service, source, credentials, transport) = web_fixture();
        search(&service, "account-a").await.unwrap();
        source.publish(web_runtime(
            vec![
                web_profile("account-a", "account-a-v1", "https://api.deepseek.com"),
                web_profile(
                    "account-b",
                    "account-b-v1",
                    "https://api.deepseek.com/anthropic",
                ),
            ],
            credentials,
        ));
        search(&service, "account-b").await.unwrap();
        let fetched = service
            .deepseek_fetch(
                "deepseek-flash",
                Some("account-b"),
                &DeepSeekFetchRequest::new("https://example.com/fixture"),
            )
            .await
            .unwrap();
        assert_eq!(fetched.content, "Fixture body");
        let requests = transport.0.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            request_authorization(&requests[0]),
            "Bearer synthetic-account-a-v1"
        );
        for request in &requests[1..] {
            assert_eq!(
                request_authorization(request),
                "Bearer synthetic-account-b-v1"
            );
        }
        assert_eq!(requests[1].url, "https://api.deepseek.com/v1/tools/search");
        assert_eq!(requests[2].url, "https://api.deepseek.com/v1/tools/fetch");
    })
    .await;
}

#[tokio::test]
async fn independent_web_reconfiguration_and_removal_do_not_reuse_a_warm_route() {
    let (service, source, credentials, transport) = web_fixture();
    search(&service, "account-a").await.unwrap();
    source.publish(web_runtime(
        vec![web_profile(
            "account-a",
            "account-a-v2",
            "https://api.deepseek.com/v1",
        )],
        credentials.clone(),
    ));
    search(&service, "account-a").await.unwrap();
    assert_eq!(
        request_authorization(&transport.0.lock().unwrap()[1]),
        "Bearer synthetic-account-a-v2"
    );
    source.publish(web_runtime(
        vec![web_profile(
            "account-a",
            "custom-key",
            "https://custom.example/v1",
        )],
        credentials.clone(),
    ));
    assert!(search(&service, "account-a").await.is_err());
    source.publish(web_runtime(
        vec![web_profile(
            "account-b",
            "account-b-v1",
            "https://api.deepseek.com",
        )],
        credentials,
    ));
    assert!(search(&service, "account-a").await.is_err());
    assert_eq!(transport.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn independent_web_operation_keeps_its_catalog_during_credential_resolution() {
    let (service, source, credentials, transport) = web_fixture();
    credentials.pause_next.store(true, Ordering::SeqCst);
    let running = tokio::spawn({
        let service = service.clone();
        async move { search(&service, "account-a").await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        credentials.entered.notified(),
    )
    .await
    .unwrap();
    source.publish(web_runtime(
        vec![web_profile(
            "account-b",
            "account-b-v1",
            "https://api.deepseek.com",
        )],
        credentials.clone(),
    ));
    credentials.released.notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    search(&service, "account-b").await.unwrap();
    let requests = transport.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        request_authorization(&requests[0]),
        "Bearer synthetic-account-a-v1"
    );
    assert_eq!(
        request_authorization(&requests[1]),
        "Bearer synthetic-account-b-v1"
    );
}
