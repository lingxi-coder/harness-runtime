//! Account-only independent audio assembly without starting an Agent or reading credentials.
use lingxi_core::host::{Clock, HttpError, HttpTransport, SecureStorage};
use lingxi_core::types::{HttpRequest, HttpResponse};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

struct AudioClock;
impl Clock for AudioClock {
    fn now(&self) -> std::time::SystemTime {
        std::time::SystemTime::now()
    }
}
struct AccountOnlyHttp;
#[async_trait::async_trait]
impl HttpTransport for AccountOnlyHttp {
    async fn stream_sse(
        &self,
        _: HttpRequest,
    ) -> Result<lingxi_core::host::http::SseStream, HttpError> {
        Err(HttpError::InvalidRequest(
            "audio account HTTP streaming is unavailable".into(),
        ))
    }
    async fn request(&self, _: HttpRequest) -> Result<HttpResponse, HttpError> {
        Err(HttpError::InvalidRequest(
            "account authentication uses the shared provider transport".into(),
        ))
    }
}

/// Build services in the actual Host credential domain. Configuration is pure;
/// the first async service resolution reads the authoritative account directory
/// on a blocking worker and binds an exact eligible account through AccountPool.
pub fn build_account_audio_services(
    home: PathBuf,
    profiles_json: &str,
    region: &str,
    storage: Arc<dyn SecureStorage>,
) -> Result<
    (
        llm_runtime::services::ProviderServices,
        BTreeMap<String, String>,
    ),
    String,
> {
    if !home.is_absolute() || profiles_json.len() > 512 * 1024 {
        return Err("invalid audio account configuration".into());
    }
    let region = match region {
        "china_mainland" => llm_runtime::Region::ChinaMainland,
        "international" => llm_runtime::Region::International,
        _ => return Err("audio requires an explicit supported usage region".into()),
    };
    let user_providers = if profiles_json.trim().is_empty() {
        BTreeMap::new()
    } else {
        serde_json::from_str(profiles_json).map_err(|_| "invalid audio provider configuration")?
    };
    let mut assembled = provider_config::assemble_for_region(
        provider_config::AssembleInputs {
            official_catalog: None,
            anthropic_api_base: "https://api.anthropic.com".into(),
            anthropic_models: vec![],
            anthropic_has_api_key: false,
            anthropic_has_oauth: false,
            user_providers,
            routing: None,
        },
        region,
    );
    if !assembled.warnings.is_empty() {
        return Err("invalid audio provider configuration".into());
    }
    let profiles = assembled.client_config.providers.clone();
    let credentials = Arc::new(secret::CredentialManager::new(
        storage,
        Arc::new(AudioClock),
        Arc::new(AccountOnlyHttp),
    ));
    let scope = crate::provider_accounts::account_store_scope(
        &home,
        &format!("{:?}", credentials.account_storage_backend()),
    );
    let snapshot = provider_config::AccountRegistry::new(scope.clone()).snapshot();
    let pool = crate::account_pool_store::assemble_pool(&mut assembled, snapshot, home.clone())?;
    let ids = assembled
        .credential_sources
        .iter()
        .map(|source| (source.profile_name.clone(), source.credential_id.clone()))
        .collect();
    let provider = Arc::new(
        provider_config::MultiCredentialProvider::new(credentials.clone())
            .with_account_version_root(home.clone())
            .with_account_directory(pool.directory()),
    );
    let runtime = llm_runtime::ModelRuntime::from_config(assembled.client_config)
        .map_err(|_| "invalid audio account catalog")?
        .with_credential_provider(provider);
    let (runtime, _) = crate::account_pool_store::attach_mobile_account_runtime(runtime, pool);
    let transport =
        Arc::new(platform_common::provider_transport().map_err(|_| "audio transport unavailable")?);
    let store = Arc::new(
        crate::provider_accounts::ProviderAccountStore::new(home, scope, credentials.clone())
            .with_profiles(profiles),
    );
    let oauth = Arc::new(llm_runtime::auth::openai::AccountOAuthClient::new(
        lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig::default(),
        transport.clone(),
    ));
    credentials.set_account_credential_refresher(Arc::new(
        crate::provider_accounts::AccountOAuthCoordinator::new(store, oauth),
    ));
    let services = runtime
        .provider_services(region, transport)
        .map_err(|_| "invalid audio account services")?;
    Ok((services, ids))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::{SecureStorageBackend, SecureStorageError};
    use lingxi_core::types::SecureStorageData;

    struct NoSecretAccess;
    #[async_trait::async_trait]
    impl SecureStorage for NoSecretAccess {
        async fn store(
            &self,
            _: &str,
            _: &str,
            _: SecureStorageData,
        ) -> Result<(), SecureStorageError> {
            panic!("audio assembly must not write a credential")
        }
        async fn retrieve(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Option<SecureStorageData>, SecureStorageError> {
            panic!("audio assembly must not read a credential")
        }
        async fn contains(&self, _: &str, _: &str) -> Result<bool, SecureStorageError> {
            panic!("audio assembly must not probe a credential")
        }
        async fn delete(&self, _: &str, _: &str) -> Result<(), SecureStorageError> {
            panic!("audio assembly must not delete a credential")
        }
        async fn list(&self, _: &str) -> Result<Vec<String>, SecureStorageError> {
            panic!("audio assembly must not enumerate credentials")
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> SecureStorageBackend {
            SecureStorageBackend::MemorySession
        }
    }

    #[test]
    fn independent_audio_construction_does_not_touch_credentials_or_create_account_files() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("new-brand-home");
        build_account_audio_services(
            home.clone(),
            "{}",
            "international",
            Arc::new(NoSecretAccess),
        )
        .expect("pure account catalog construction");
        assert!(!home.exists());
    }
}
