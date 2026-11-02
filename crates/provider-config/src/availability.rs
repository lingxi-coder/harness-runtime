//! Account-scoped credential presence for the model catalog.
//! Only enabled registered accounts contribute availability; secrets are not read.

use std::sync::Arc;

use llm_runtime::ProviderId;

use crate::CredentialSource;

/// Availability of one provider profile for the picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAvailability {
    /// Provider identity.
    pub provider_id: ProviderId,
    /// Human profile name (the engine map keys by this; spec §6.2).
    pub profile_name: String,
    /// Credential id this availability was computed for.
    pub credential_id: String,
    /// Whether a usable credential is present.
    pub available: bool,
}

/// Account-aware presence for the model catalog. Managed bindings never regain
/// invisible environment/provider-wide credentials after their last account is removed.
pub async fn compute_availability_with_accounts(
    credentials: &Arc<secret::CredentialManager>,
    sources: &[CredentialSource],
    snapshot: &lingxi_core::host::provider_accounts::AccountSnapshot,
) -> Vec<ProviderAvailability> {
    let mut presence = std::collections::BTreeMap::new();
    for account in &snapshot.accounts {
        let pending = snapshot.health.iter().any(|h| {
            h.account_id == account.account_id
                && h.reason.as_deref() == Some("credential_commit_pending")
        });
        let present = account.enabled
            && !pending
            && credentials
                .has_account_credential(&account.credential_ref)
                .await
                .unwrap_or(false);
        presence.insert(account.account_id.as_str(), present);
    }
    sources
        .iter()
        .map(|source| {
            let exact = snapshot
                .accounts
                .iter()
                .find(|a| a.credential_ref.secret_ref == source.credential_id);
            let available = if let Some(account) = exact {
                presence
                    .get(account.account_id.as_str())
                    .copied()
                    .unwrap_or(false)
            } else {
                snapshot
                    .accounts
                    .iter()
                    .filter(|a| {
                        a.execution_binding_id == source.profile_name
                            || a.execution_binding_id == source.credential_id
                    })
                    .any(|a| {
                        presence
                            .get(a.account_id.as_str())
                            .copied()
                            .unwrap_or(false)
                    })
            };
            ProviderAvailability {
                provider_id: source.provider_id.clone(),
                profile_name: source.profile_name.clone(),
                credential_id: source.credential_id.clone(),
                available,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::ProviderId;
    use std::sync::Arc;

    #[derive(Default)]
    struct MemStorage {
        map: std::sync::Mutex<
            std::collections::HashMap<(String, String), lingxi_core::types::SecureStorageData>,
        >,
        retrieves: std::sync::atomic::AtomicUsize,
        contains: std::sync::atomic::AtomicUsize,
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
            self.retrieves
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .map
                .lock()
                .unwrap()
                .get(&(service.into(), account.into()))
                .cloned())
        }
        async fn contains(
            &self,
            service: &str,
            account: &str,
        ) -> Result<bool, lingxi_core::host::SecureStorageError> {
            self.contains
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(self
                .map
                .lock()
                .unwrap()
                .contains_key(&(service.into(), account.into())))
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
            panic!("availability tests must not perform HTTP");
        }
        async fn stream_sse(
            &self,
            _req: lingxi_core::types::HttpRequest,
        ) -> Result<lingxi_core::host::http::SseStream, lingxi_core::host::HttpError> {
            panic!("availability tests must not perform HTTP");
        }
    }
    fn src(name: &str, env: Option<&str>) -> crate::CredentialSource {
        crate::CredentialSource {
            provider_id: ProviderId::OpenAICompatible {
                name: name.to_string(),
            },
            profile_name: name.to_string(),
            credential_id: name.to_string(),
            env_var: env.map(str::to_string),
            kind: crate::CredentialKind::ApiKey,
        }
    }

    #[tokio::test]
    async fn account_availability_contains_only_and_empty_or_disabled_never_uses_ambient() {
        use lingxi_core::host::provider_accounts::{
            AccountAuthKind, AccountMetadata, CredentialRef,
        };
        let storage = Arc::new(MemStorage::default());
        let credentials = Arc::new(secret::CredentialManager::new(
            storage.clone(),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ));
        storage.map.lock().unwrap().insert(
            ("lingxi".into(), "provider-key-openai".into()),
            lingxi_core::types::SecureStorageData::new(
                b"old-singleton".to_vec(),
                lingxi_core::types::SecureStorageMetadata {
                    created_at: std::time::SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: lingxi_core::types::SecretKindDto("generic_api_key".into()),
                },
            ),
        );
        let mut registry = crate::AccountRegistry::new("test_account_availability".into());
        let account = AccountMetadata {
            account_id: uuid::Uuid::new_v4().to_string(),
            provider_id: "openai".into(),
            display_name: "fixture".into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "openai".into(),
            credential_ref: CredentialRef {
                store_scope: "test_account_availability".into(),
                secret_ref: String::new(),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 0,
            identity_generation: 1,
        };
        let mut account = account;
        account.credential_ref.secret_ref = format!("acct.{}", account.account_id);
        registry.add(account.clone(), 0).unwrap();
        credentials
            .store_account_credential(
                &account.credential_ref,
                &secret::AccountCredential {
                    credential_revision: 1,
                    identity_generation: 1,
                    kind: secret::AccountCredentialKind::ApiKey {
                        key: lingxi_core::types::Secret::new("fixture-account-key".into()),
                    },
                },
            )
            .await
            .unwrap();
        let mut exact = src("account.fixture", None);
        exact.credential_id = account.credential_ref.secret_ref.clone();
        let sources = [src("openai", Some("PATH")), exact];
        let rows =
            compute_availability_with_accounts(&credentials, &sources, &registry.snapshot()).await;
        assert!(rows.iter().all(|r| r.available));
        assert_eq!(
            storage.retrieves.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert_eq!(
            storage.contains.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let mut disabled = registry.snapshot();
        disabled.accounts[0].enabled = false;
        assert!(
            compute_availability_with_accounts(&credentials, &sources, &disabled)
                .await
                .iter()
                .all(|r| !r.available)
        );
        disabled.accounts.clear();
        assert!(
            compute_availability_with_accounts(&credentials, &sources, &disabled)
                .await
                .iter()
                .all(|r| !r.available)
        );
        assert_eq!(
            storage.retrieves.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
    }
}
