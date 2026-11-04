//! Exact-account credential provider shared by native and Desktop hosts.
//! Every request resolves one account identity and its published credential
//! revision. Missing accounts never fall back to provider-wide credentials.

use std::collections::BTreeMap;
use std::sync::Arc;

use llm_runtime::{
    BoxFuture, Credential, CredentialProvider, CredentialScope, CredentialSource as Source,
    LlmError,
};

/// Resolves credentials for exact accounts without provider-wide fallback.
pub struct MultiCredentialProvider {
    account_version_root: Option<std::path::PathBuf>,
    account_directory: Option<lingxi_core::host::provider_accounts::SharedAccountDirectory>,
    accounts: BTreeMap<String, lingxi_core::host::provider_accounts::AccountMetadata>,
    credentials: Arc<secret::CredentialManager>,
}

impl std::fmt::Debug for MultiCredentialProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiCredentialProvider")
            .field("has_live_directory", &self.account_directory.is_some())
            .field("account_count", &self.accounts.len())
            .finish_non_exhaustive()
    }
}

impl MultiCredentialProvider {
    /// Create a provider using the supplied secure credential manager.
    #[must_use]
    pub fn new(credentials: Arc<secret::CredentialManager>) -> Self {
        Self {
            account_directory: None,
            account_version_root: None,
            accounts: BTreeMap::new(),
            credentials,
        }
    }

    /// Bind exact host-local account references; no account may fall back to ambient keys.
    #[must_use]
    pub fn with_accounts(
        mut self,
        accounts: Vec<lingxi_core::host::provider_accounts::AccountMetadata>,
    ) -> Self {
        self.accounts = accounts
            .into_iter()
            .map(|a| (a.credential_ref.secret_ref.clone(), a))
            .collect();
        self
    }
    /// Resolve account references from a shared live directory when present.
    #[must_use]
    pub fn with_account_directory(
        mut self,
        directory: lingxi_core::host::provider_accounts::SharedAccountDirectory,
    ) -> Self {
        self.account_directory = Some(directory);
        self
    }
    /// Verify account credential revisions against the ledger under this root.
    #[must_use]
    pub fn with_account_version_root(mut self, root: std::path::PathBuf) -> Self {
        self.account_version_root = Some(root);
        self
    }
    async fn published_revision(
        &self,
        account: &lingxi_core::host::provider_accounts::AccountMetadata,
    ) -> Result<Option<u64>, LlmError> {
        if let Some(root) = &self.account_version_root {
            let root = root.clone();
            let account = account.clone();
            return tokio::task::spawn_blocking(move || {
                crate::read_account_credential_version(&root, &account)
            })
            .await
            .map_err(|_| LlmError::AccountStorageUnavailable)?
            .map_err(|_| LlmError::AccountStorageUnavailable);
        }
        self.credentials
            .get_account_credential(&account.credential_ref)
            .await
            .map(|value| {
                value
                    .filter(|value| value.identity_generation == account.identity_generation)
                    .map(|value| value.credential_revision)
            })
            .map_err(|_| LlmError::AccountStorageUnavailable)
    }
    fn verify_account_scope(&self, scope: &CredentialScope) -> Result<(), LlmError> {
        if let Some(expected) = scope.expected_identity_generation {
            if scope
                .credential_id
                .as_deref()
                .and_then(|id| self.account(id))
                .is_none_or(|a| a.identity_generation != expected)
            {
                return Err(LlmError::InvalidRequest {
                    message: "stale account route identity".into(),
                });
            }
        }
        Ok(())
    }
    fn account(&self, id: &str) -> Option<lingxi_core::host::provider_accounts::AccountMetadata> {
        if let Some(directory) = &self.account_directory {
            return directory
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .accounts
                .iter()
                .find(|a| a.credential_ref.secret_ref == id)
                .cloned();
        }
        self.accounts.get(id).cloned()
    }
    async fn load_account_versioned(
        &self,
        id: &str,
    ) -> Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError> {
        let mut rejected_revision = None;
        let result: Result<_, LlmError> = async {
            let account = self.account(id).ok_or_else(|| LlmError::Authentication {
                message: "unknown account".into(),
            })?;
            if !account.enabled {
                return Err(LlmError::Authentication {
                    message: "account disabled".into(),
                });
            }
            let stored = self
                .credentials
                .get_account_credential(&account.credential_ref)
                .await
                .map_err(|_| LlmError::AccountStorageUnavailable)?;
            let mut record = match stored {
                Some(record) => record,
                None => self
                    .credentials
                    .refresh_account_credential(account.clone(), 0)
                    .await
                    .map_err(account_refresh_error)?
                    .ok_or_else(|| LlmError::Authentication {
                        message: "account requires authentication".into(),
                    })?,
            };
            rejected_revision = Some(record.credential_revision);
            if record.identity_generation < account.identity_generation {
                record = self
                    .credentials
                    .refresh_account_credential(account.clone(), 0)
                    .await
                    .map_err(|_| LlmError::AccountStorageUnavailable)?
                    .ok_or(LlmError::AccountStorageUnavailable)?;
            }
            if record.identity_generation != account.identity_generation {
                return Err(LlmError::AccountStorageUnavailable);
            }
            if matches!(&record.kind,secret::AccountCredentialKind::OAuth{expires_at,..} if *expires_at<=std::time::SystemTime::now()+std::time::Duration::from_secs(60))
            {
                record = self
                    .credentials
                    .refresh_account_credential(account.clone(), record.credential_revision)
                    .await
                    .map_err(account_refresh_error)?
                    .ok_or_else(|| LlmError::Authentication {
                        message: "account token expired".into(),
                    })?;
            }
            if self.account_version_root.is_some() {
                let published = self.published_revision(&account).await?;
                if published != Some(record.credential_revision) {
                    let rejected =
                        if published.is_some_and(|revision| revision > record.credential_revision) {
                            record.credential_revision
                        } else {
                            record.credential_revision.saturating_sub(1)
                        };
                    record = self
                        .credentials
                        .refresh_account_credential(account.clone(), rejected)
                        .await
                        .map_err(|_| LlmError::AccountStorageUnavailable)?
                        .ok_or(LlmError::AccountStorageUnavailable)?;
                    if self.published_revision(&account).await? != Some(record.credential_revision) {
                        return Err(LlmError::AccountStorageUnavailable);
                    }
                }
            }
            rejected_revision = Some(record.credential_revision);
            let revision = record.credential_revision;
            account_credential_to_runtime(&account, record).map(|credential| {
                llm_runtime::VersionedCredential {
                    credential,
                    credential_revision: Some(revision),
                }
            })
        }
        .await;
        result.map_err(|error| llm_runtime::VersionedCredentialError {
            error,
            credential_revision: rejected_revision,
        })
    }
    async fn load_account(&self, id: &str) -> Result<Credential, LlmError> {
        self.load_account_versioned(id)
            .await
            .map(|value| value.credential)
            .map_err(|error| error.error)
    }
    async fn refresh_account_versioned(
        &self,
        id: &str,
        rejected: &Credential,
    ) -> Result<Option<llm_runtime::VersionedCredential>, llm_runtime::VersionedCredentialError>
    {
        let mut rejected_revision = None;
        let result: Result<_, LlmError> = async {
            let account = self.account(id).ok_or_else(|| LlmError::Authentication {
                message: "unknown account".into(),
            })?;
            if account.auth_kind != lingxi_core::host::provider_accounts::AccountAuthKind::OAuth {
                return Ok(None);
            }
            let current = self
                .credentials
                .get_account_credential(&account.credential_ref)
                .await
                .map_err(|_| LlmError::AccountStorageUnavailable)?
                .ok_or_else(|| LlmError::Authentication {
                    message: "account requires authentication".into(),
                })?;
            rejected_revision = Some(current.credential_revision);
            let resolved = account_credential_to_runtime(&account, clone_record(&current)?)?;
            if self.account_version_root.is_some()
                && self.published_revision(&account).await? != Some(current.credential_revision)
            {
                return Err(LlmError::AccountStorageUnavailable);
            }
            if &resolved != rejected {
                return Ok(Some(llm_runtime::VersionedCredential {
                    credential: resolved,
                    credential_revision: Some(current.credential_revision),
                }));
            }
            let record = self
                .credentials
                .refresh_account_credential(account.clone(), current.credential_revision)
                .await
                .map_err(account_refresh_error)?;
            let Some(record) = record else {
                return Ok(None);
            };
            if self.account_version_root.is_some()
                && self.published_revision(&account).await? != Some(record.credential_revision)
            {
                return Err(LlmError::AccountStorageUnavailable);
            }
            let revision = record.credential_revision;
            account_credential_to_runtime(&account, record).map(|credential| {
                Some(llm_runtime::VersionedCredential {
                    credential,
                    credential_revision: Some(revision),
                })
            })
        }
        .await;
        result.map_err(|error| llm_runtime::VersionedCredentialError {
            error,
            credential_revision: rejected_revision,
        })
    }

    async fn refresh_account(
        &self,
        id: &str,
        rejected: &Credential,
    ) -> Result<Option<Credential>, LlmError> {
        self.refresh_account_versioned(id, rejected)
            .await
            .map(|value| value.map(|value| value.credential))
            .map_err(|error| error.error)
    }
}

impl CredentialProvider for MultiCredentialProvider {
    fn load_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<
        'a,
        Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            let id = scope
                .credential_id
                .as_deref()
                .ok_or_else(|| LlmError::Authentication {
                    message: "account credential required".into(),
                })?;
            self.load_account_versioned(id).await
        })
    }

    fn refresh_versioned<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<
        'a,
        Result<Option<llm_runtime::VersionedCredential>, llm_runtime::VersionedCredentialError>,
    > {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            match scope
                .credential_id
                .as_deref()
                .filter(|id| self.account(id).is_some())
            {
                Some(id) => self.refresh_account_versioned(id, rejected).await,
                None => Ok(None),
            }
        })
    }

    fn current_credential_revision<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            match scope
                .credential_id
                .as_deref()
                .and_then(|id| self.account(id))
            {
                Some(account) => self.published_revision(&account).await,
                None => Ok(None),
            }
        })
    }

    fn source<'a>(&'a self, scope: &'a CredentialScope) -> BoxFuture<'a, Result<Source, LlmError>> {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            let Some(id) = scope.credential_id.as_deref() else {
                return Ok(Source::None);
            };
            let account = self.account(id).ok_or_else(|| LlmError::Authentication {
                message: "unknown account".into(),
            })?;
            self.credentials
                .has_account_credential(&account.credential_ref)
                .await
                .map(|present| {
                    if present {
                        Source::Stored
                    } else {
                        Source::None
                    }
                })
                .map_err(|_| LlmError::AccountStorageUnavailable)
        })
    }

    fn anthropic_auth_snapshot<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<llm_runtime::AnthropicAuthSnapshot, LlmError>> {
        Box::pin(async move {
            let credential = self.load(scope).await?;
            Ok(llm_runtime::AnthropicAuthSnapshot::from_credential(
                scope.clone(),
                credential,
            ))
        })
    }

    fn refresh<'a>(
        &'a self,
        scope: &'a CredentialScope,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Option<Credential>, LlmError>> {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            match scope
                .credential_id
                .as_deref()
                .filter(|id| self.account(id).is_some())
            {
                Some(id) => self.refresh_account(id, rejected).await,
                None => Ok(None),
            }
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            self.verify_account_scope(scope)?;
            let id = scope
                .credential_id
                .as_deref()
                .ok_or_else(|| LlmError::Authentication {
                    message: "account credential required".into(),
                })?;
            self.load_account(id).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm_runtime::{LlmRequest, ModelRuntime, ProviderId};
    #[derive(Default)]
    struct MemStorage {
        map: std::sync::Mutex<
            std::collections::HashMap<(String, String), lingxi_core::types::SecureStorageData>,
        >,
        fail_reads: std::sync::atomic::AtomicBool,
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
            if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(lingxi_core::host::SecureStorageError::BackendUnavailable(
                    "fixture vault locked".into(),
                ));
            }
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
    #[tokio::test]
    async fn exact_chatgpt_account_uses_workspace_header_and_keeps_principal_separate() {
        use lingxi_core::host::provider_accounts::{
            AccountAuthKind, AccountMetadata, CredentialRef,
        };
        let cm = manager();
        let id = "11111111-1111-4111-8111-111111111111";
        let reference = CredentialRef {
            store_scope: "header_test".into(),
            secret_ref: format!("acct.{id}"),
        };
        cm.store_account_credential(
            &reference,
            &secret::AccountCredential {
                credential_revision: 1,
                identity_generation: 1,
                kind: secret::AccountCredentialKind::OAuth {
                    access_token: lingxi_core::types::Secret::new("token-A".into()),
                    refresh_token: None,
                    expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                    scopes: vec![],
                    issuer: "https://auth.openai.com".into(),
                    subject: Some("oidc-user-A".into()),
                    organization_id: Some("workspace-shared".into()),
                    email: None,
                    fedramp: false,
                },
            },
        )
        .await
        .unwrap();
        let account = AccountMetadata {
            account_id: id.into(),
            provider_id: "openai".into(),
            display_name: "A".into(),
            auth_kind: AccountAuthKind::OAuth,
            execution_binding_id: "openai-chatgpt".into(),
            credential_ref: reference.clone(),
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: 1,
        };
        let mut profile = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|p| p.profile_name == "openai-chatgpt")
            .unwrap();
        let model = profile.models[0].request_model.clone();
        profile.profile_name = format!("account.{id}");
        profile.credential = llm_runtime::CredentialConfig::Static {
            id: reference.secret_ref,
        };
        let client = ModelRuntime::from_config(llm_runtime::ClientConfig {
            providers: vec![profile],
        })
        .unwrap()
        .with_credential_provider(Arc::new(
            MultiCredentialProvider::new(cm).with_accounts(vec![account]),
        ));
        let prepared = client
            .prepare(&LlmRequest::new(model).with_user_text("hello"))
            .await
            .unwrap();
        assert_eq!(
            prepared
                .provider_request
                .headers
                .get("ChatGPT-Account-ID")
                .map(String::as_str),
            Some("workspace-shared")
        );
        assert_ne!(
            prepared
                .provider_request
                .headers
                .get("ChatGPT-Account-ID")
                .map(String::as_str),
            Some("oidc-user-A")
        );
    }

    fn manager() -> Arc<secret::CredentialManager> {
        Arc::new(secret::CredentialManager::new(
            Arc::new(MemStorage::default()),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ))
    }

    fn api_account(id: &str) -> lingxi_core::host::provider_accounts::AccountMetadata {
        use lingxi_core::host::provider_accounts::{
            AccountAuthKind, AccountMetadata, CredentialRef,
        };
        AccountMetadata {
            account_id: id.into(),
            provider_id: "openai".into(),
            display_name: id.into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "openai".into(),
            credential_ref: CredentialRef {
                store_scope: "credential_test".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: 1,
        }
    }

    fn account_scope(
        account: &lingxi_core::host::provider_accounts::AccountMetadata,
    ) -> CredentialScope {
        let mut scope = CredentialScope::new(
            ProviderId::OpenAICompatible {
                name: "openai".into(),
            },
            format!("account.{}", account.account_id),
        )
        .with_credential_id(account.credential_ref.secret_ref.clone());
        scope.expected_identity_generation = Some(account.identity_generation);
        scope
    }

    #[tokio::test]
    async fn exact_accounts_keep_keys_isolated_and_never_read_provider_slots() {
        let storage = Arc::new(MemStorage::default());
        let cm = Arc::new(secret::CredentialManager::new(
            storage.clone(),
            Arc::new(FixedClock),
            Arc::new(NoHttp),
        ));
        let a = api_account("11111111-1111-4111-8111-111111111111");
        let b = api_account("22222222-2222-4222-8222-222222222222");
        for (account, key) in [(&a, "key-A"), (&b, "key-B")] {
            cm.store_account_credential(
                &account.credential_ref,
                &secret::AccountCredential {
                    credential_revision: 3,
                    identity_generation: 1,
                    kind: secret::AccountCredentialKind::ApiKey {
                        key: lingxi_core::types::Secret::new(key.into()),
                    },
                },
            )
            .await
            .unwrap();
        }
        storage.map.lock().unwrap().insert(
            ("lingxi".into(), "provider-key-openai".into()),
            lingxi_core::types::SecureStorageData::new(
                b"obsolete-provider-key".to_vec(),
                lingxi_core::types::SecureStorageMetadata {
                    created_at: std::time::SystemTime::UNIX_EPOCH,
                    last_accessed: None,
                    kind: lingxi_core::types::SecretKindDto("generic_api_key".into()),
                },
            ),
        );
        let provider = MultiCredentialProvider::new(cm).with_accounts(vec![a.clone(), b.clone()]);
        for (account, key) in [(&a, "key-A"), (&b, "key-B")] {
            let loaded = provider
                .load_versioned(&account_scope(account))
                .await
                .unwrap();
            assert_eq!(loaded.credential, Credential::ApiKey(key.into()));
            assert_eq!(loaded.credential_revision, Some(3));
        }
        let old_slot = CredentialScope::new(
            ProviderId::OpenAICompatible {
                name: "openai".into(),
            },
            "openai",
        )
        .with_credential_id("openai");
        assert!(matches!(
            provider.load(&old_slot).await,
            Err(LlmError::Authentication { .. })
        ));
        assert!(matches!(
            provider.source(&old_slot).await,
            Err(LlmError::Authentication { .. })
        ));
    }

    #[tokio::test]
    async fn disabled_or_replaced_identity_cannot_load_a_saved_key() {
        let cm = manager();
        let mut account = api_account("11111111-1111-4111-8111-111111111111");
        cm.store_account_credential(
            &account.credential_ref,
            &secret::AccountCredential {
                credential_revision: 1,
                identity_generation: 1,
                kind: secret::AccountCredentialKind::ApiKey {
                    key: lingxi_core::types::Secret::new("key-A".into()),
                },
            },
        )
        .await
        .unwrap();
        let original_scope = account_scope(&account);
        account.enabled = false;
        let disabled =
            MultiCredentialProvider::new(cm.clone()).with_accounts(vec![account.clone()]);
        assert!(matches!(
            disabled.load(&original_scope).await,
            Err(LlmError::Authentication { .. })
        ));
        account.enabled = true;
        account.identity_generation = 2;
        let replaced = MultiCredentialProvider::new(cm).with_accounts(vec![account]);
        assert!(matches!(
            replaced.load(&original_scope).await,
            Err(LlmError::InvalidRequest { .. })
        ));
        assert!(matches!(
            replaced.source(&original_scope).await,
            Err(LlmError::InvalidRequest { .. })
        ));
    }
}

fn clone_record(record: &secret::AccountCredential) -> Result<secret::AccountCredential, LlmError> {
    let bytes = lingxi_core::types::Secret::new(
        serde_json::to_vec(record).map_err(|_| LlmError::AccountStorageUnavailable)?,
    );
    serde_json::from_slice(bytes.expose_secret()).map_err(|_| LlmError::AccountStorageUnavailable)
}
fn account_credential_to_runtime(
    account: &lingxi_core::host::provider_accounts::AccountMetadata,
    record: secret::AccountCredential,
) -> Result<Credential, LlmError> {
    if record.identity_generation != account.identity_generation {
        return Err(LlmError::InvalidRequest {
            message: "stale account credential identity".into(),
        });
    }
    match record.kind {
        secret::AccountCredentialKind::ApiKey { key } => {
            Ok(Credential::ApiKey(key.expose_secret().clone()))
        }
        secret::AccountCredentialKind::OAuth {
            access_token,
            organization_id,
            fedramp,
            scopes,
            ..
        } => {
            if account.execution_binding_id == "openai-chatgpt" {
                Ok(Credential::ChatGptOAuth {
                    access_token: access_token.expose_secret().clone(),
                    account_id: organization_id,
                    fedramp,
                })
            } else {
                Ok(Credential::AnthropicOAuth {
                    access_token: access_token.expose_secret().clone(),
                    scopes,
                })
            }
        }
    }
}

fn account_refresh_error(error: secret::CredentialError) -> LlmError {
    match error {
        secret::CredentialError::RefreshFailed(_) => LlmError::Authentication {
            message: "account requires reauthorization".into(),
        },
        _ => LlmError::AccountStorageUnavailable,
    }
}
