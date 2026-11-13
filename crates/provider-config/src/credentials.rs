//! Exact-account credential provider shared by native and Desktop hosts.
//! Every request resolves one account identity and its published credential
//! revision. Missing accounts never fall back to provider-wide credentials.

use std::collections::BTreeMap;
use std::sync::Arc;

use lingxi_core::host::provider_accounts::AccountMetadata;

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
    #[cfg(test)]
    after_account_resolved: Option<Arc<dyn Fn() + Send + Sync>>,
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
            #[cfg(test)]
            after_account_resolved: None,
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
    fn account_for_scope(
        &self,
        scope: &CredentialScope,
    ) -> Result<Option<AccountMetadata>, LlmError> {
        // Resolve and fence the same owned metadata. A second live-directory
        // lookup could select a new principal after checking the old scope.
        let account = scope
            .credential_id
            .as_deref()
            .and_then(|id| self.account(id));
        if let Some(expected) = scope.expected_identity_generation {
            if account
                .as_ref()
                .is_none_or(|a| a.identity_generation != expected)
            {
                return Err(LlmError::InvalidRequest {
                    message: "stale account route identity".into(),
                });
            }
        }
        #[cfg(test)]
        if let Some(hook) = &self.after_account_resolved {
            hook();
        }
        Ok(account)
    }

    fn required_account(&self, scope: &CredentialScope) -> Result<AccountMetadata, LlmError> {
        let account = self.account_for_scope(scope)?;
        if scope.credential_id.is_none() {
            return Err(LlmError::Authentication {
                message: "account credential required".into(),
            });
        }
        account.ok_or_else(|| LlmError::Authentication {
            message: "unknown account".into(),
        })
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
    async fn reread_account(
        &self,
        account: &AccountMetadata,
    ) -> Result<secret::AccountCredential, LlmError> {
        self.credentials
            .get_account_credential_uncached(&account.credential_ref)
            .await
            .map_err(|_| LlmError::AccountStorageUnavailable)?
            .ok_or(LlmError::AccountStorageUnavailable)
    }
    async fn load_account_versioned(
        &self,
        account: AccountMetadata,
    ) -> Result<llm_runtime::VersionedCredential, llm_runtime::VersionedCredentialError> {
        let mut rejected_revision = None;
        let result: Result<_, LlmError> = async {
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
            let mut authoritative = false;
            let mut record = match stored {
                Some(record) => record,
                None => {
                    authoritative = true;
                    self.credentials
                        .refresh_account_credential(account.clone(), 0)
                        .await
                        .map_err(account_refresh_error)?
                        .ok_or_else(|| LlmError::Authentication {
                            message: "account requires authentication".into(),
                        })?
                }
            };
            rejected_revision = Some(record.credential_revision);
            if record.identity_generation != account.identity_generation {
                // Another host can publish a replacement directory before
                // this host's positive native cache expires. Adopt the exact
                // durable slot before delegating any OAuth protocol refresh.
                record = self.reread_account(&account).await?;
                rejected_revision = Some(record.credential_revision);
                authoritative = true;
            }
            if record.identity_generation < account.identity_generation
                && account.auth_kind
                    == lingxi_core::host::provider_accounts::AccountAuthKind::OAuth
            {
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
            if self.account_version_root.is_some() {
                let published = self.published_revision(&account).await?;
                if published != Some(record.credential_revision) {
                    if !authoritative {
                        record = self.reread_account(&account).await?;
                        rejected_revision = Some(record.credential_revision);
                    }
                    if record.identity_generation != account.identity_generation {
                        return Err(LlmError::AccountStorageUnavailable);
                    }
                    let published = self.published_revision(&account).await?;
                    if published != Some(record.credential_revision) {
                        if account.auth_kind
                            != lingxi_core::host::provider_accounts::AccountAuthKind::OAuth
                        {
                            return Err(LlmError::AccountStorageUnavailable);
                        }
                        let rejected = if published
                            .is_some_and(|revision| revision > record.credential_revision)
                        {
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
                        if self.published_revision(&account).await?
                            != Some(record.credential_revision)
                        {
                            return Err(LlmError::AccountStorageUnavailable);
                        }
                    }
                }
            }
            rejected_revision = Some(record.credential_revision);
            let expires_soon = matches!(
                &record.kind,
                secret::AccountCredentialKind::OAuth { expires_at, .. }
                    if *expires_at <= std::time::SystemTime::now() + std::time::Duration::from_secs(60)
            );
            if expires_soon {
                record = self
                    .credentials
                    .refresh_account_credential(account.clone(), record.credential_revision)
                    .await
                    .map_err(account_refresh_error)?
                    .ok_or_else(|| LlmError::Authentication {
                        message: "account token expired".into(),
                    })?;
                if record.identity_generation != account.identity_generation
                    || (self.account_version_root.is_some()
                        && self.published_revision(&account).await?
                            != Some(record.credential_revision))
                {
                    return Err(LlmError::AccountStorageUnavailable);
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
    async fn load_account(&self, account: AccountMetadata) -> Result<Credential, LlmError> {
        self.load_account_versioned(account)
            .await
            .map(|value| value.credential)
            .map_err(|error| error.error)
    }
    async fn refresh_account_versioned(
        &self,
        account: AccountMetadata,
        rejected: &Credential,
    ) -> Result<Option<llm_runtime::VersionedCredential>, llm_runtime::VersionedCredentialError>
    {
        let mut rejected_revision = None;
        let result: Result<_, LlmError> = async {
            if account.auth_kind != lingxi_core::host::provider_accounts::AccountAuthKind::OAuth {
                return Ok(None);
            }
            let mut current = self
                .credentials
                .get_account_credential(&account.credential_ref)
                .await
                .map_err(|_| LlmError::AccountStorageUnavailable)?
                .ok_or_else(|| LlmError::Authentication {
                    message: "account requires authentication".into(),
                })?;
            rejected_revision = Some(current.credential_revision);
            if current.identity_generation != account.identity_generation
                || (self.account_version_root.is_some()
                    && self.published_revision(&account).await?
                        != Some(current.credential_revision))
            {
                current = self.reread_account(&account).await?;
                rejected_revision = Some(current.credential_revision);
            }
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
        account: AccountMetadata,
        rejected: &Credential,
    ) -> Result<Option<Credential>, LlmError> {
        self.refresh_account_versioned(account, rejected)
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
            let account = self.required_account(scope)?;
            self.load_account_versioned(account).await
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
            match self.account_for_scope(scope)? {
                Some(account) => self.refresh_account_versioned(account, rejected).await,
                None => Ok(None),
            }
        })
    }

    fn current_credential_revision<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Option<u64>, LlmError>> {
        Box::pin(async move {
            match self.account_for_scope(scope)? {
                Some(account) => self.published_revision(&account).await,
                None => Ok(None),
            }
        })
    }

    fn source<'a>(&'a self, scope: &'a CredentialScope) -> BoxFuture<'a, Result<Source, LlmError>> {
        Box::pin(async move {
            let account = self.account_for_scope(scope)?;
            if scope.credential_id.is_none() {
                return Ok(Source::None);
            }
            let account = account.ok_or_else(|| LlmError::Authentication {
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
            match self.account_for_scope(scope)? {
                Some(account) => self.refresh_account(account, rejected).await,
                None => Ok(None),
            }
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a CredentialScope,
    ) -> BoxFuture<'a, Result<Credential, LlmError>> {
        Box::pin(async move {
            let account = self.required_account(scope)?;
            self.load_account(account).await
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

    #[derive(Default)]
    struct CapturingWire {
        requests: std::sync::Mutex<Vec<lingxi_llm_client::HttpRequest>>,
    }

    #[async_trait::async_trait]
    impl lingxi_llm_client::Transport for CapturingWire {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            self.requests.lock().unwrap().push(request);
            Err(lingxi_llm_client::protocol::LlmError::Transport {
                message: "synthetic wire captured after authentication".into(),
            })
        }
    }

    #[tokio::test]
    async fn directory_replacement_during_resolution_never_authenticates_an_old_request_as_g2() {
        use lingxi_core::host::provider_accounts::AccountSnapshot;
        for streaming in [false, true] {
            let cm = manager();
            let old = api_account("11111111-1111-4111-8111-111111111111");
            let mut new = old.clone();
            new.identity_generation = 2;
            new.config_revision = 2;
            // Secure storage is committed before metadata is published. Pause
            // resolution across exactly that directory publication boundary.
            cm.store_account_credential(
                &new.credential_ref,
                &secret::AccountCredential {
                    credential_revision: 2,
                    identity_generation: 2,
                    kind: secret::AccountCredentialKind::ApiKey {
                        key: lingxi_core::types::Secret::new("synthetic-new-principal-key".into()),
                    },
                },
            )
            .await
            .unwrap();
            let snapshot = AccountSnapshot {
                store_scope: old.credential_ref.store_scope.clone(),
                config_revision: 1,
                accounts: vec![old.clone()],
                provider_policies: Default::default(),
                health: vec![],
                model_entitlements: vec![],
                model_availability: vec![],
                scoped_health: vec![],
            };
            let mut replacement = snapshot.clone();
            replacement.config_revision = 2;
            replacement.accounts = vec![new];
            let directory = Arc::new(std::sync::RwLock::new(snapshot.clone()));
            let mut provider =
                MultiCredentialProvider::new(cm.clone()).with_account_directory(directory.clone());
            let published = directory.clone();
            let next = replacement.clone();
            provider.after_account_resolved = Some(Arc::new(move || {
                *published.write().unwrap() = next.clone();
            }));
            let mut profile = llm_runtime::builtin_presets()
                .providers
                .into_iter()
                .find(|profile| profile.profile_name == "openai")
                .unwrap();
            let model = profile.models[0].request_model.clone();
            profile.profile_name = format!("account.{}", old.account_id);
            profile.credential = llm_runtime::CredentialConfig::Static {
                id: old.credential_ref.secret_ref.clone(),
            };
            let config = llm_runtime::ClientConfig {
                providers: vec![profile],
            };
            let client = ModelRuntime::from_config(config.clone())
                .unwrap()
                .with_credential_provider(Arc::new(provider))
                .reconfigured_with_account_snapshot(config.clone(), &snapshot)
                .unwrap();
            let mut request = LlmRequest::new(model).with_user_text("synthetic generation fence");
            request.stream = streaming;
            let wire = Arc::new(CapturingWire::default());
            let result = if streaming {
                client
                    .execute_stream(&request, wire.clone())
                    .await
                    .map(|_| ())
            } else {
                client.execute(&request, wire.clone()).await.map(|_| ())
            };
            assert!(matches!(result, Err(LlmError::AccountStorageUnavailable)));
            assert_eq!(directory.read().unwrap().accounts[0].identity_generation, 2);
            assert!(
                wire.requests.lock().unwrap().is_empty(),
                "an old route must never send the replacement principal's credential"
            );

            // A fresh ordinary request still reaches the physical stream/non-
            // stream envelope with the newly published principal's key.
            let fresh = ModelRuntime::from_config(config.clone())
                .unwrap()
                .with_credential_provider(Arc::new(
                    MultiCredentialProvider::new(cm).with_account_directory(directory),
                ))
                .reconfigured_with_account_snapshot(config, &replacement)
                .unwrap();
            let result = if streaming {
                fresh
                    .execute_stream(&request, wire.clone())
                    .await
                    .map(|_| ())
            } else {
                fresh.execute(&request, wire.clone()).await.map(|_| ())
            };
            assert!(matches!(result, Err(LlmError::Transport { .. })));
            let requests = wire.requests.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert!(requests[0]
                .headers
                .iter()
                .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                    && value == "Bearer synthetic-new-principal-key"));
        }
    }

    mod cache_regression_tests {
        use super::*;
        use lingxi_core::host::SecureStorage as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::{Duration, SystemTime};

        type Key = (String, String);
        struct PositiveCachedStorage {
            durable: Arc<MemStorage>,
            cache: std::sync::Mutex<BTreeMap<Key, lingxi_core::types::SecureStorageData>>,
            uncached_reads: AtomicUsize,
            backend_reads: AtomicUsize,
        }
        impl PositiveCachedStorage {
            fn new(durable: Arc<MemStorage>) -> Self {
                Self {
                    durable,
                    cache: Default::default(),
                    uncached_reads: AtomicUsize::new(0),
                    backend_reads: AtomicUsize::new(0),
                }
            }
            async fn authoritative(
                &self,
                service: &str,
                account: &str,
            ) -> Result<
                Option<lingxi_core::types::SecureStorageData>,
                lingxi_core::host::SecureStorageError,
            > {
                let key = (service.to_owned(), account.to_owned());
                self.cache.lock().unwrap().remove(&key);
                self.backend_reads.fetch_add(1, Ordering::SeqCst);
                let data = self.durable.retrieve(service, account).await?;
                if let Some(value) = &data {
                    self.cache.lock().unwrap().insert(key, value.clone());
                }
                Ok(data)
            }
        }
        #[async_trait::async_trait]
        impl lingxi_core::host::SecureStorage for PositiveCachedStorage {
            async fn store(
                &self,
                service: &str,
                account: &str,
                data: lingxi_core::types::SecureStorageData,
            ) -> Result<(), lingxi_core::host::SecureStorageError> {
                self.durable.store(service, account, data).await?;
                self.cache
                    .lock()
                    .unwrap()
                    .remove(&(service.to_owned(), account.to_owned()));
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
                let cached = self
                    .cache
                    .lock()
                    .unwrap()
                    .get(&(service.to_owned(), account.to_owned()))
                    .cloned();
                if cached.is_some() {
                    return Ok(cached);
                }
                self.authoritative(service, account).await
            }
            async fn retrieve_uncached(
                &self,
                service: &str,
                account: &str,
            ) -> Result<
                Option<lingxi_core::types::SecureStorageData>,
                lingxi_core::host::SecureStorageError,
            > {
                self.uncached_reads.fetch_add(1, Ordering::SeqCst);
                self.authoritative(service, account).await
            }
            async fn delete(
                &self,
                service: &str,
                account: &str,
            ) -> Result<(), lingxi_core::host::SecureStorageError> {
                self.durable.delete(service, account).await?;
                self.cache
                    .lock()
                    .unwrap()
                    .remove(&(service.to_owned(), account.to_owned()));
                Ok(())
            }
            async fn list(
                &self,
                service: &str,
            ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
                self.durable.list(service).await
            }
            fn is_encrypted(&self) -> bool {
                true
            }
            fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
                lingxi_core::host::SecureStorageBackend::MacOsKeychain
            }
        }
        fn manager_on(
            storage: Arc<dyn lingxi_core::host::SecureStorage>,
        ) -> Arc<secret::CredentialManager> {
            Arc::new(secret::CredentialManager::new(
                storage,
                Arc::new(FixedClock),
                Arc::new(NoHttp),
            ))
        }
        fn api_record(generation: u64, revision: u64) -> secret::AccountCredential {
            secret::AccountCredential {
                identity_generation: generation,
                credential_revision: revision,
                kind: secret::AccountCredentialKind::ApiKey {
                    key: lingxi_core::types::Secret::new(format!("synthetic-key-r{revision}")),
                },
            }
        }
        fn oauth_record(
            generation: u64,
            revision: u64,
            expires_at: SystemTime,
        ) -> secret::AccountCredential {
            secret::AccountCredential {
                identity_generation: generation,
                credential_revision: revision,
                kind: secret::AccountCredentialKind::OAuth {
                    access_token: lingxi_core::types::Secret::new(format!(
                        "synthetic-oauth-r{revision}"
                    )),
                    refresh_token: Some(lingxi_core::types::Secret::new(
                        "synthetic-refresh".into(),
                    )),
                    expires_at,
                    scopes: vec![],
                    issuer: "https://auth.openai.com".into(),
                    subject: Some("fixture-user".into()),
                    organization_id: Some("fixture-workspace".into()),
                    email: None,
                    fedramp: false,
                },
            }
        }
        fn publish(root: &std::path::Path, account: &AccountMetadata, revision: u64) {
            let _lock = lingxi_core::host::rooted_fs::lock_exclusive(
                root,
                &std::path::PathBuf::from(format!(
                    "provider-accounts/{}.lock",
                    account.credential_ref.store_scope
                )),
                0o700,
                0o600,
            )
            .unwrap();
            crate::write_account_credential_version_locked(root, account, revision).unwrap();
        }
        fn directory(
            account: &AccountMetadata,
        ) -> lingxi_core::host::provider_accounts::SharedAccountDirectory {
            let mut registry =
                crate::AccountRegistry::new(account.credential_ref.store_scope.clone());
            registry.add(account.clone(), 0).unwrap();
            Arc::new(std::sync::RwLock::new(registry.snapshot()))
        }
        struct Owner {
            writer: Arc<secret::CredentialManager>,
            root: std::path::PathBuf,
            calls: std::sync::Mutex<Vec<u64>>,
            rotations: AtomicUsize,
            fail_rotation: bool,
        }
        #[async_trait::async_trait]
        impl secret::AccountCredentialRefresher for Owner {
            async fn refresh(
                &self,
                account: AccountMetadata,
                rejected: u64,
            ) -> Result<Option<secret::AccountCredential>, secret::CredentialError> {
                self.calls.lock().unwrap().push(rejected);
                if account.auth_kind != lingxi_core::host::provider_accounts::AccountAuthKind::OAuth
                {
                    return Ok(None);
                }
                let mut current = self
                    .writer
                    .get_account_credential_uncached(&account.credential_ref)
                    .await?
                    .unwrap();
                if current.credential_revision <= rejected {
                    if self.fail_rotation {
                        return Err(secret::CredentialError::RefreshFailed(
                            "synthetic refresh rejected".into(),
                        ));
                    }
                    self.rotations.fetch_add(1, Ordering::SeqCst);
                    current = oauth_record(
                        account.identity_generation,
                        rejected + 1,
                        SystemTime::now() + Duration::from_secs(3600),
                    );
                    self.writer
                        .store_account_credential(&account.credential_ref, &current)
                        .await?;
                }
                publish(&self.root, &account, current.credential_revision);
                Ok(Some(current))
            }
        }
        fn owner(writer: Arc<secret::CredentialManager>, root: &std::path::Path) -> Arc<Owner> {
            Arc::new(Owner {
                writer,
                root: root.to_owned(),
                calls: Default::default(),
                rotations: AtomicUsize::new(0),
                fail_rotation: false,
            })
        }

        #[tokio::test]
        async fn api_key_rotation_adopts_independent_hot_generation_and_revision_without_refresh() {
            for change_generation in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let durable = Arc::new(MemStorage::default());
                let writer = manager_on(durable.clone());
                let storage = Arc::new(PositiveCachedStorage::new(durable));
                let reader = manager_on(storage.clone());
                let account = api_account("11111111-1111-4111-8111-111111111111");
                writer
                    .store_account_credential(&account.credential_ref, &api_record(1, 1))
                    .await
                    .unwrap();
                publish(root.path(), &account, 1);
                let directory = directory(&account);
                let provider = MultiCredentialProvider::new(reader.clone())
                    .with_account_directory(directory.clone())
                    .with_account_version_root(root.path().to_owned());
                let owner = owner(writer.clone(), root.path());
                reader.set_account_credential_refresher(owner.clone());
                assert_eq!(
                    provider
                        .load_versioned(&account_scope(&account))
                        .await
                        .unwrap()
                        .credential,
                    Credential::ApiKey("synthetic-key-r1".into())
                );
                assert_eq!(storage.backend_reads.load(Ordering::SeqCst), 1);
                assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 0);

                let mut live = account.clone();
                live.config_revision = 2;
                if change_generation {
                    live.identity_generation = 2;
                }
                writer
                    .store_account_credential(
                        &live.credential_ref,
                        &api_record(live.identity_generation, 2),
                    )
                    .await
                    .unwrap();
                publish(root.path(), &live, 2);
                directory.write().unwrap().config_revision = 2;
                directory.write().unwrap().accounts = vec![live.clone()];
                let adopted = provider
                    .load_versioned(&account_scope(&live))
                    .await
                    .unwrap();
                assert_eq!(
                    adopted.credential,
                    Credential::ApiKey("synthetic-key-r2".into())
                );
                assert_eq!(adopted.credential_revision, Some(2));
                assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
                assert!(
                    owner.calls.lock().unwrap().is_empty(),
                    "API-key adoption does not invoke OAuth authority"
                );
                provider
                    .load_versioned(&account_scope(&live))
                    .await
                    .unwrap();
                assert_eq!(
                    storage.backend_reads.load(Ordering::SeqCst),
                    2,
                    "unchanged credentials remain positively cached"
                );
                assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
            }
        }

        #[tokio::test]
        async fn authoritative_api_key_rereads_keep_identity_publication_and_io_fences() {
            for failure in [
                "future_identity",
                "stale_identity",
                "unpublished",
                "unavailable",
            ] {
                let root = tempfile::tempdir().unwrap();
                let durable = Arc::new(MemStorage::default());
                let writer = manager_on(durable.clone());
                let storage = Arc::new(PositiveCachedStorage::new(durable.clone()));
                let reader = manager_on(storage.clone());
                let account = api_account("11111111-1111-4111-8111-111111111111");
                writer
                    .store_account_credential(&account.credential_ref, &api_record(1, 1))
                    .await
                    .unwrap();
                publish(root.path(), &account, 1);
                let directory = directory(&account);
                let provider = MultiCredentialProvider::new(reader.clone())
                    .with_account_directory(directory.clone())
                    .with_account_version_root(root.path().to_owned());
                let owner = owner(writer.clone(), root.path());
                reader.set_account_credential_refresher(owner.clone());
                provider
                    .load_versioned(&account_scope(&account))
                    .await
                    .unwrap();
                let mut live = account.clone();
                live.identity_generation = 2;
                live.config_revision = 2;
                writer
                    .store_account_credential(
                        &live.credential_ref,
                        &api_record(
                            match failure {
                                "future_identity" => 3,
                                "stale_identity" => 1,
                                _ => 2,
                            },
                            2,
                        ),
                    )
                    .await
                    .unwrap();
                if failure != "unpublished" {
                    publish(root.path(), &live, 2);
                }
                directory.write().unwrap().config_revision = 2;
                directory.write().unwrap().accounts = vec![live.clone()];
                let old = provider.load_versioned(&account_scope(&account)).await;
                assert!(
                    matches!(old, Err(error) if matches!(error.error, LlmError::InvalidRequest { .. }))
                );
                assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 0);
                durable
                    .fail_reads
                    .store(failure == "unavailable", Ordering::SeqCst);
                let result = provider.load_versioned(&account_scope(&live)).await;
                assert!(
                    matches!(result, Err(error) if matches!(error.error, LlmError::AccountStorageUnavailable))
                );
                assert_eq!(
                    storage.uncached_reads.load(Ordering::SeqCst),
                    1,
                    "only one authoritative reread per failed adoption"
                );
                assert!(owner.calls.lock().unwrap().is_empty());
            }
        }

        #[tokio::test]
        async fn oauth_cached_rotation_is_adopted_before_expiry_refresh() {
            let root = tempfile::tempdir().unwrap();
            let durable = Arc::new(MemStorage::default());
            let writer = manager_on(durable.clone());
            let storage = Arc::new(PositiveCachedStorage::new(durable));
            let reader = manager_on(storage.clone());
            let mut account = api_account("11111111-1111-4111-8111-111111111111");
            account.auth_kind = lingxi_core::host::provider_accounts::AccountAuthKind::OAuth;
            account.execution_binding_id = "openai-chatgpt".into();
            writer
                .store_account_credential(
                    &account.credential_ref,
                    &oauth_record(1, 1, SystemTime::now() + Duration::from_secs(3600)),
                )
                .await
                .unwrap();
            publish(root.path(), &account, 1);
            let provider = MultiCredentialProvider::new(reader.clone())
                .with_accounts(vec![account.clone()])
                .with_account_version_root(root.path().to_owned());
            provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap();
            writer
                .store_account_credential(
                    &account.credential_ref,
                    &oauth_record(1, 2, SystemTime::UNIX_EPOCH),
                )
                .await
                .unwrap();
            publish(root.path(), &account, 2);
            let owner = owner(writer, root.path());
            reader.set_account_credential_refresher(owner.clone());
            let loaded = provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap();
            assert_eq!(loaded.credential_revision, Some(3));
            assert!(
                matches!(loaded.credential, Credential::ChatGptOAuth { access_token, .. } if access_token == "synthetic-oauth-r3")
            );
            assert_eq!(*owner.calls.lock().unwrap(), vec![2]);
            assert_eq!(owner.rotations.load(Ordering::SeqCst), 1);
            assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn oauth_rejection_adopts_the_current_revision_without_another_rotation() {
            let root = tempfile::tempdir().unwrap();
            let durable = Arc::new(MemStorage::default());
            let writer = manager_on(durable.clone());
            let storage = Arc::new(PositiveCachedStorage::new(durable));
            let reader = manager_on(storage.clone());
            let mut account = api_account("11111111-1111-4111-8111-111111111111");
            account.auth_kind = lingxi_core::host::provider_accounts::AccountAuthKind::OAuth;
            account.execution_binding_id = "openai-chatgpt".into();
            let expires_at = SystemTime::now() + Duration::from_secs(3600);
            writer
                .store_account_credential(&account.credential_ref, &oauth_record(1, 1, expires_at))
                .await
                .unwrap();
            publish(root.path(), &account, 1);
            let provider = MultiCredentialProvider::new(reader.clone())
                .with_accounts(vec![account.clone()])
                .with_account_version_root(root.path().to_owned());
            let rejected = provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap();
            writer
                .store_account_credential(&account.credential_ref, &oauth_record(1, 2, expires_at))
                .await
                .unwrap();
            publish(root.path(), &account, 2);
            let owner = owner(writer, root.path());
            reader.set_account_credential_refresher(owner.clone());
            let adopted = provider
                .refresh_versioned(&account_scope(&account), &rejected.credential)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(adopted.credential_revision, Some(2));
            assert!(
                matches!(adopted.credential, Credential::ChatGptOAuth { access_token, .. } if access_token == "synthetic-oauth-r2")
            );
            assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
            assert!(owner.calls.lock().unwrap().is_empty());
        }

        #[tokio::test]
        async fn adopted_oauth_refresh_failure_reports_the_current_credential_revision() {
            let root = tempfile::tempdir().unwrap();
            let durable = Arc::new(MemStorage::default());
            let writer = manager_on(durable.clone());
            let storage = Arc::new(PositiveCachedStorage::new(durable));
            let reader = manager_on(storage.clone());
            let mut account = api_account("11111111-1111-4111-8111-111111111111");
            account.auth_kind = lingxi_core::host::provider_accounts::AccountAuthKind::OAuth;
            account.execution_binding_id = "openai-chatgpt".into();
            writer
                .store_account_credential(
                    &account.credential_ref,
                    &oauth_record(1, 1, SystemTime::now() + Duration::from_secs(3600)),
                )
                .await
                .unwrap();
            publish(root.path(), &account, 1);
            let provider = MultiCredentialProvider::new(reader.clone())
                .with_accounts(vec![account.clone()])
                .with_account_version_root(root.path().to_owned());
            provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap();
            writer
                .store_account_credential(
                    &account.credential_ref,
                    &oauth_record(1, 2, SystemTime::UNIX_EPOCH),
                )
                .await
                .unwrap();
            publish(root.path(), &account, 2);
            reader.set_account_credential_refresher(Arc::new(Owner {
                writer,
                root: root.path().to_owned(),
                calls: Default::default(),
                rotations: AtomicUsize::new(0),
                fail_rotation: true,
            }));
            let failed = provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap_err();
            assert!(matches!(failed.error, LlmError::Authentication { .. }));
            assert_eq!(failed.credential_revision, Some(2));
            assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
        }

        #[tokio::test]
        async fn oauth_unpublished_rotation_can_still_repair_its_ledger() {
            let root = tempfile::tempdir().unwrap();
            let durable = Arc::new(MemStorage::default());
            let writer = manager_on(durable.clone());
            let storage = Arc::new(PositiveCachedStorage::new(durable));
            let reader = manager_on(storage.clone());
            let mut account = api_account("11111111-1111-4111-8111-111111111111");
            account.auth_kind = lingxi_core::host::provider_accounts::AccountAuthKind::OAuth;
            account.execution_binding_id = "openai-chatgpt".into();
            writer
                .store_account_credential(
                    &account.credential_ref,
                    &oauth_record(1, 2, SystemTime::now() + Duration::from_secs(3600)),
                )
                .await
                .unwrap();
            publish(root.path(), &account, 1);
            let owner = owner(writer, root.path());
            reader.set_account_credential_refresher(owner.clone());
            let provider = MultiCredentialProvider::new(reader)
                .with_accounts(vec![account.clone()])
                .with_account_version_root(root.path().to_owned());
            let loaded = provider
                .load_versioned(&account_scope(&account))
                .await
                .unwrap();
            assert_eq!(loaded.credential_revision, Some(2));
            assert_eq!(
                crate::read_account_credential_version(root.path(), &account).unwrap(),
                Some(2)
            );
            assert_eq!(*owner.calls.lock().unwrap(), vec![1]);
            assert_eq!(owner.rotations.load(Ordering::SeqCst), 0);
            assert_eq!(storage.uncached_reads.load(Ordering::SeqCst), 1);
        }
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
