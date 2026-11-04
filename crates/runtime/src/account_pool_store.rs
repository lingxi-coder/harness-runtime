//! Cross-process account health transactions. Rooted no-follow I/O, locking and
//! fsync run only on blocking workers; the lock never spans provider traffic.
use lingxi_core::host::rooted_fs;
use llm_runtime::accounts::{AccountPoolStore, HealthDocument, PoolOperation, PoolResult};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct FileAccountPoolStore {
    data_root: PathBuf,
    authority: Option<llm_runtime::accounts::SharedAccountDirectory>,
}
impl FileAccountPoolStore {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            data_root,
            authority: None,
        }
    }
    pub fn with_directory_authority(
        mut self,
        directory: llm_runtime::accounts::SharedAccountDirectory,
    ) -> Self {
        self.authority = Some(directory);
        self
    }
    fn transact_blocking(
        root: &Path,
        authority: Option<llm_runtime::accounts::SharedAccountDirectory>,
        operation: PoolOperation,
    ) -> Result<PoolResult, String> {
        let account = match &operation {
            PoolOperation::Claim { account, .. } | PoolOperation::Finish { account, .. } => account,
        };
        let directory_lock = PathBuf::from(format!(
            "provider-accounts/{}.lock",
            account.credential_ref.store_scope
        ));
        // Match catalog mutation lock ordering: directory before health. An
        // injected Broker snapshot is an authority only for its captured scope.
        let _directory_lock = rooted_fs::lock_exclusive(root, &directory_lock, 0o700, 0o600)
            .map_err(|_| "account directory lock unavailable")?;
        let persisted = crate::provider_accounts::load_account_snapshot(
            root,
            &account.credential_ref.store_scope,
        )?;
        if persisted.health.iter().any(|health| {
            health.account_id == account.account_id
                && health.identity_generation == account.identity_generation
                && health.reason.as_deref() == Some("credential_commit_pending")
        }) {
            return Ok(PoolResult::Unavailable(
                llm_runtime::accounts::AccountUnavailable {
                    account_id: account.account_id.clone(),
                    reason: "credential_commit_pending".into(),
                    scope: llm_runtime::accounts::FailureScope::Account,
                    retryable: true,
                    recovery_at_ms: None,
                },
            ));
        }
        let snapshot = authority
            .map(|directory| directory.read().unwrap_or_else(|e| e.into_inner()).clone())
            .filter(|snapshot| {
                snapshot.store_scope == account.credential_ref.store_scope
                    && snapshot.config_revision >= persisted.config_revision
            })
            .unwrap_or(persisted);
        // Re-read authority every transaction. Old pools cannot restore deleted
        // accounts or blacklist a newly installed identity/credential revision.
        if !snapshot.accounts.iter().any(|current| {
            current.account_id == account.account_id
                && current.identity_generation == account.identity_generation
                && current.config_revision == account.config_revision
                && current.enabled
        }) {
            return Ok(PoolResult::Stale);
        }
        let scope_hash = format!(
            "{:x}",
            Sha256::digest(account.credential_ref.store_scope.as_bytes())
        );
        let lock_path = PathBuf::from(format!("provider-accounts/health/{scope_hash}.lock"));
        let state_path = PathBuf::from(format!("provider-accounts/health/{scope_hash}.json"));
        let _lock = rooted_fs::lock_exclusive(root, &lock_path, 0o700, 0o600)
            .map_err(|_| "account health lock unavailable")?;
        let mut document =
            match rooted_fs::read_to_string_limited(root, &state_path, 4 * 1024 * 1024) {
                Ok(text) => serde_json::from_str::<HealthDocument>(&text)
                    .map_err(|_| "account health state is invalid")?,
                Err(rooted_fs::FsError::NotFound(_)) => HealthDocument::default(),
                Err(_) => return Err("account health storage unavailable".into()),
            };
        let current_credential_revision =
            provider_config::read_account_credential_version(root, account)?;
        document.invalidate_auth_for_credential(account, current_credential_revision);
        if let PoolOperation::Finish {
            credential_revision,
            outcome: Some(failure),
            ..
        } = &operation
        {
            if (failure.status == llm_runtime::accounts::HealthStatus::NeedsAuth
                || failure.reason == "derived_token_rejected")
                && *credential_revision != current_credential_revision
            {
                return Ok(PoolResult::Stale);
            }
        }
        let result = document.apply(operation);
        if !matches!(result, PoolResult::Unavailable(_) | PoolResult::Stale) {
            let bytes =
                serde_json::to_vec(&document).map_err(|_| "account health encoding failed")?;
            rooted_fs::atomic_write(
                root,
                &state_path,
                &bytes,
                rooted_fs::AtomicWriteOptions::default(),
            )
            .map_err(|_| "account health persistence failed")?;
        }
        Ok(result)
    }
}
#[async_trait::async_trait]
impl AccountPoolStore for FileAccountPoolStore {
    async fn latest_snapshot(
        &self,
    ) -> Result<Option<lingxi_core::host::provider_accounts::AccountSnapshot>, String> {
        let Some(authority) = self.authority.clone() else {
            return Ok(None);
        };
        let root = self.data_root.clone();
        let current = authority
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        tokio::task::spawn_blocking(move || {
            let persisted =
                crate::provider_accounts::load_account_snapshot(&root, &current.store_scope)?;
            Ok(Some(
                if persisted.config_revision > current.config_revision {
                    persisted
                } else {
                    current
                },
            ))
        })
        .await
        .map_err(|_| "account directory worker failed")?
    }
    async fn transact(&self, operation: PoolOperation) -> Result<PoolResult, String> {
        let root = self.data_root.clone();
        let authority = self.authority.clone();
        tokio::task::spawn_blocking(move || Self::transact_blocking(&root, authority, operation))
            .await
            .map_err(|_| "account health worker failed")?
    }
}

/// Assemble account profiles before freezing the routing catalog. The same
/// metadata snapshot drives credential resolution, selection, and fencing.
pub fn assemble_pool(
    assembled: &mut provider_config::Assembled,
    snapshot: lingxi_core::host::provider_accounts::AccountSnapshot,
    data_root: PathBuf,
) -> Result<std::sync::Arc<llm_runtime::accounts::AccountPool>, String> {
    provider_config::assemble_account_profiles(assembled, &snapshot.accounts)?;
    let directory = std::sync::Arc::new(std::sync::RwLock::new(snapshot));
    let store = FileAccountPoolStore::new(data_root).with_directory_authority(directory.clone());
    Ok(std::sync::Arc::new(
        llm_runtime::accounts::AccountPool::from_directory(directory, std::sync::Arc::new(store)),
    ))
}

/// Re-applies the current account directory to an upstream live catalog. Each
/// logical call gets a frozen configuration; active prepared calls are untouched.
#[derive(Debug)]
pub(crate) struct AccountRuntimeSource {
    base: std::sync::Arc<llm_runtime::ModelRuntime>,
    pool: std::sync::Arc<llm_runtime::accounts::AccountPool>,
}
impl AccountRuntimeSource {
    pub(crate) fn configuration(&self) -> llm_runtime::ClientConfig {
        let mut config = self.base.current_without_task_scope().configuration();
        if provider_config::assemble_account_client_config(
            &mut config,
            &self.pool.snapshot().accounts,
        )
        .is_err()
        {
            config.providers.clear();
        }
        config
    }
    pub(crate) fn refresh(
        &self,
        snapshot: lingxi_core::host::provider_accounts::AccountSnapshot,
    ) -> Result<(), String> {
        let mut config = self.base.current_without_task_scope().configuration();
        provider_config::assemble_account_client_config(&mut config, &snapshot.accounts)?;
        llm_runtime::ModelRuntime::from_config(config)
            .map_err(|_| "account routing configuration is invalid")?;
        self.pool.refresh(snapshot)
    }
}
impl llm_runtime::client::ModelRuntimeSource for AccountRuntimeSource {
    fn rebind_accounts(
        &self,
        captured: &llm_runtime::ModelRuntime,
    ) -> std::sync::Arc<llm_runtime::ModelRuntime> {
        let mut config = captured.configuration();
        if provider_config::assemble_account_client_config(
            &mut config,
            &self.pool.snapshot().accounts,
        )
        .is_err()
        {
            config.providers.clear();
        }
        captured
            .reconfigured(config)
            .expect("validated account routing configuration")
            .with_owner_state(captured)
    }
    fn current(
        &self,
        template: &llm_runtime::ModelRuntime,
    ) -> std::sync::Arc<llm_runtime::ModelRuntime> {
        let base = self.base.current_without_task_scope();
        let mut config = base.configuration();
        let snapshot = self.pool.snapshot();
        if provider_config::assemble_account_client_config(&mut config, &snapshot.accounts).is_err()
        {
            // Fail closed if a binding disappeared from the live catalog.
            config.providers.clear();
        }
        match base.reconfigured(config) {
            Ok(runtime) => runtime.with_owner_state(template),
            Err(_) => {
                let mut config = base.configuration();
                config.providers.clear();
                base.reconfigured(config)
                    .expect("empty routing catalog")
                    .with_owner_state(template)
            }
        }
    }
}
/// Attach the live directory layer after installing credentials and the upstream
/// catalog source. The registry is the host's dynamic account update surface.
#[cfg(feature = "desktop")]
pub(crate) fn attach_account_runtime(
    runtime: llm_runtime::ModelRuntime,
    pool: std::sync::Arc<llm_runtime::accounts::AccountPool>,
    registry: &crate::desktop::FusionCatalogRegistry,
) -> llm_runtime::ModelRuntime {
    let source = std::sync::Arc::new(AccountRuntimeSource {
        base: std::sync::Arc::new(runtime.clone()),
        pool: pool.clone(),
    });
    registry
        .account_sources
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(std::sync::Arc::downgrade(&source));
    runtime.with_account_pool(pool).with_catalog_source(source)
}

pub(crate) fn attach_mobile_account_runtime(
    runtime: llm_runtime::ModelRuntime,
    pool: std::sync::Arc<llm_runtime::accounts::AccountPool>,
) -> (
    llm_runtime::ModelRuntime,
    std::sync::Arc<AccountRuntimeSource>,
) {
    let source = std::sync::Arc::new(AccountRuntimeSource {
        base: std::sync::Arc::new(runtime.clone()),
        pool: pool.clone(),
    });
    (
        runtime
            .with_account_pool(pool)
            .with_catalog_source(source.clone()),
        source,
    )
}

pub(crate) async fn load_native_account_snapshot(
    root: PathBuf,
    credentials: std::sync::Arc<secret::CredentialManager>,
) -> Result<lingxi_core::host::provider_accounts::AccountSnapshot, String> {
    let scope = crate::provider_accounts::account_store_scope(
        &root,
        &format!("{:?}", credentials.account_storage_backend()),
    );
    tokio::task::spawn_blocking(move || {
        crate::provider_accounts::load_account_snapshot(&root, &scope)
    })
    .await
    .map_err(|_| "account directory worker failed")?
}

/// Read health for presentation. Atomic files need no lock on this read path;
/// absence is ready and corrupt/unreadable state is a storage error, not auth.
pub async fn read_health(
    data_root: PathBuf,
    store_scope: String,
) -> Result<HealthDocument, String> {
    tokio::task::spawn_blocking(move || {
        let scope_hash = format!("{:x}", Sha256::digest(store_scope.as_bytes()));
        let path = PathBuf::from(format!("provider-accounts/health/{scope_hash}.json"));
        match rooted_fs::read_to_string_limited(&data_root, &path, 4 * 1024 * 1024) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|_| "account health state is invalid".into())
            }
            Err(rooted_fs::FsError::NotFound(_)) => Ok(HealthDocument::default()),
            Err(_) => Err("account health storage unavailable".into()),
        }
    })
    .await
    .map_err(|_| "account health worker failed")?
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, AccountMetadata, AccountSnapshot, CredentialRef,
    };
    use llm_runtime::accounts::{classify, FailureScope};
    use std::sync::{Arc, RwLock};
    fn setup() -> (
        tempfile::TempDir,
        llm_runtime::accounts::SharedAccountDirectory,
        AccountMetadata,
    ) {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let account = AccountMetadata {
            account_id: id.clone(),
            provider_id: "test".into(),
            display_name: "Test".into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "test".into(),
            credential_ref: CredentialRef {
                store_scope: "test-domain".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: 1,
        };
        let directory = Arc::new(RwLock::new(AccountSnapshot {
            store_scope: "test-domain".into(),
            config_revision: 1,
            accounts: vec![account.clone()],
            health: vec![],
            model_entitlements: vec![],
            scoped_health: vec![],
            model_availability: vec![],
            provider_policies: Default::default(),
        }));
        (root, directory, account)
    }
    fn claim(account: AccountMetadata, owner: String, now_ms: u64) -> PoolOperation {
        PoolOperation::Claim {
            account,
            owner,
            model: "m".into(),
            endpoint: "fingerprint".into(),
            now_ms,
        }
    }
    fn publish_version(root: &Path, account: &AccountMetadata, revision: u64) {
        let path = PathBuf::from(format!(
            "provider-accounts/{}.lock",
            account.credential_ref.store_scope
        ));
        let _lock = rooted_fs::lock_exclusive(root, &path, 0o700, 0o600).unwrap();
        provider_config::write_account_credential_version_locked(root, account, revision).unwrap();
    }
    #[tokio::test]
    async fn credential_publication_racing_auth_failure_cannot_poison_new_revision() {
        let (root, directory, account) = setup();
        rooted_fs::atomic_write(
            root.path(),
            Path::new("provider-accounts/test-domain.json"),
            &serde_json::to_vec(&*directory.read().unwrap()).unwrap(),
            Default::default(),
        )
        .unwrap();
        publish_version(root.path(), &account, 1);
        let store =
            FileAccountPoolStore::new(root.path().to_owned()).with_directory_authority(directory);
        let revision = match store
            .transact(claim(account.clone(), "rev1".into(), 0))
            .await
            .unwrap()
        {
            PoolResult::Claimed { revision } => revision,
            _ => panic!("claim"),
        };
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let finish = {
            let store = store.clone();
            let account = account.clone();
            let barrier = barrier.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                store
                    .transact(PoolOperation::Finish {
                        account,
                        model: "m".into(),
                        endpoint: "fingerprint".into(),
                        owner: "rev1".into(),
                        claimed_revision: revision,
                        credential_revision: Some(1),
                        outcome: classify(
                            &llm_runtime::LlmError::Authentication {
                                message: "old token".into(),
                            },
                            "m",
                            "fingerprint",
                            0,
                        ),
                        now_ms: 0,
                    })
                    .await
                    .unwrap()
            })
        };
        let publish = {
            let root = root.path().to_owned();
            let account = account.clone();
            tokio::spawn(async move {
                barrier.wait().await;
                tokio::task::spawn_blocking(move || publish_version(&root, &account, 2))
                    .await
                    .unwrap();
            })
        };
        finish.await.unwrap();
        publish.await.unwrap();
        let snapshot =
            crate::provider_accounts::load_account_snapshot(root.path(), "test-domain").unwrap();
        assert!(snapshot.health.iter().all(|health| health.status
            != lingxi_core::host::provider_accounts::AccountHealthStatus::NeedsAuth));
        let revision = match store
            .transact(claim(account.clone(), "rev2".into(), 1))
            .await
            .unwrap()
        {
            PoolResult::Claimed { revision } => revision,
            _ => panic!("new credential must ignore old auth fault"),
        };
        assert!(matches!(
            store
                .transact(PoolOperation::Finish {
                    account: account.clone(),
                    model: "m".into(),
                    endpoint: "fingerprint".into(),
                    owner: "rev2".into(),
                    claimed_revision: revision,
                    credential_revision: Some(2),
                    outcome: classify(
                        &llm_runtime::LlmError::Authentication {
                            message: "current token".into()
                        },
                        "m",
                        "fingerprint",
                        1
                    ),
                    now_ms: 1
                })
                .await
                .unwrap(),
            PoolResult::Recorded { .. }
        ));
        publish_version(root.path(), &account, 3);
        let snapshot =
            crate::provider_accounts::load_account_snapshot(root.path(), "test-domain").unwrap();
        assert!(snapshot.health.iter().all(|health| health.status
            != lingxi_core::host::provider_accounts::AccountHealthStatus::NeedsAuth));
        assert!(matches!(
            store
                .transact(claim(account.clone(), "rev3".into(), 2))
                .await
                .unwrap(),
            PoolResult::Claimed { .. }
        ));
        assert!(matches!(
            store
                .transact(PoolOperation::Finish {
                    account,
                    model: "m".into(),
                    endpoint: "fingerprint".into(),
                    owner: "rev2".into(),
                    claimed_revision: revision,
                    credential_revision: Some(2),
                    outcome: classify(
                        &llm_runtime::LlmError::Authentication {
                            message: "late token".into()
                        },
                        "m",
                        "fingerprint",
                        2
                    ),
                    now_ms: 2
                })
                .await
                .unwrap(),
            PoolResult::Stale
        ));
    }
    #[tokio::test]
    async fn separate_file_stores_share_one_probe_and_fence_deleted_identity() {
        let (root, directory, account) = setup();
        let store = FileAccountPoolStore::new(root.path().to_owned())
            .with_directory_authority(directory.clone());
        store
            .transact(claim(account.clone(), "initial".into(), 0))
            .await
            .unwrap();
        store
            .transact(PoolOperation::Finish {
                account: account.clone(),
                model: "m".into(),
                endpoint: "fingerprint".into(),
                owner: "initial".into(),
                claimed_revision: 0,
                credential_revision: None,
                outcome: classify(
                    &llm_runtime::LlmError::RateLimited {
                        retry_after: None,
                        scope: None,
                    },
                    "m",
                    "fingerprint",
                    0,
                ),
                now_ms: 0,
            })
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for index in 0..50 {
            let store = FileAccountPoolStore::new(root.path().to_owned())
                .with_directory_authority(directory.clone());
            let account = account.clone();
            tasks.push(tokio::spawn(async move {
                matches!(
                    store
                        .transact(claim(account, index.to_string(), 30_000))
                        .await
                        .unwrap(),
                    PoolResult::Claimed { .. }
                )
            }));
        }
        let mut probes = 0;
        for task in tasks {
            probes += usize::from(task.await.unwrap());
        }
        assert_eq!(probes, 1);
        directory.write().unwrap().accounts.clear();
        directory.write().unwrap().config_revision = 2;
        assert!(matches!(
            store
                .transact(claim(account, "deleted".into(), 60_001))
                .await
                .unwrap(),
            PoolResult::Stale
        ));
        let health = read_health(root.path().to_owned(), "test-domain".into())
            .await
            .unwrap();
        assert_eq!(health.records.len(), 1);
        assert_eq!(
            health.records.values().next().unwrap().scope,
            FailureScope::Model("m".into())
        );
    }
}

/// Awaited observation bridge keeps selection/failure before final turn output.
pub(crate) struct OutputAccountRoutingReporter(
    pub std::sync::Arc<dyn lingxi_core::host::OutputStream>,
);
#[async_trait::async_trait]
impl llm_runtime::accounts::AccountRoutingReporter for OutputAccountRoutingReporter {
    async fn report(
        &self,
        event: lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent,
    ) {
        self.0.emit_provider_account_routing(&event).await;
    }
}

#[cfg(all(test, feature = "desktop"))]
mod logical_call_directory_tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{
        AccountAuthKind, AccountMetadata, AccountSnapshot, CredentialRef, ProviderAccountPolicy,
        ProviderAccountRoutingEvent,
    };
    use std::sync::{Arc, Mutex, RwLock};
    struct AccountWire(std::sync::atomic::AtomicUsize);
    impl llm_runtime::test_support::FixtureTransport for AccountWire {
        fn execute<'a>(
            &'a self,
            _: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::ProviderResponse, llm_runtime::LlmError>>
        {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Ok(llm_runtime::ProviderResponse {status:200,headers:Default::default(),body_json:lingxi_llm_client::protocol::JsonPayload::from_generated_value(&serde_json::json!({"id":"chat","model":"wire","choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})).unwrap(),request_id:None})
            })
        }
        fn open_stream<'a>(
            &'a self,
            _: &'a llm_runtime::ProviderRequest,
        ) -> llm_runtime::BoxFuture<'a, Result<llm_runtime::StreamingResponse, llm_runtime::LlmError>>
        {
            Box::pin(async {
                Err(llm_runtime::LlmError::UnsupportedCapability {
                    capability: "test stream".into(),
                })
            })
        }
    }
    llm_runtime::impl_fixture_transport!(AccountWire);
    #[derive(Default)]
    struct Routing(Mutex<Vec<ProviderAccountRoutingEvent>>);
    #[async_trait::async_trait]
    impl llm_runtime::accounts::AccountRoutingReporter for Routing {
        async fn report(&self, event: ProviderAccountRoutingEvent) {
            self.0.lock().unwrap().push(event);
        }
    }
    #[tokio::test]
    async fn existing_process_reloads_external_reorder_and_removal_at_logical_boundary() {
        let root = tempfile::tempdir().unwrap();
        let mut template = llm_runtime::builtin_presets()
            .providers
            .into_iter()
            .find(|profile| profile.profile_name == "deepseek")
            .unwrap();
        template.profile_name = "test".into();
        template.base_url = "https://unit.invalid/v1".into();
        template.protocol = llm_runtime::ProtocolFamily::OpenAiChat;
        template.connection.group = Some("test".into());
        template.models.truncate(1);
        let model = template.models[0].request_model.clone();
        let mut assembled = provider_config::Assembled {
            client_config: llm_runtime::ClientConfig {
                providers: vec![template],
            },
            pricing: cost::PricingCatalog::empty(),
            chains: Default::default(),
            credential_sources: vec![],
            warnings: vec![],
        };
        let accounts: Vec<_> = (0..2)
            .map(|priority| {
                let id = uuid::Uuid::new_v4().to_string();
                AccountMetadata {
                    account_id: id.clone(),
                    provider_id: "test".into(),
                    display_name: format!("Account {priority}"),
                    auth_kind: AccountAuthKind::ApiKey,
                    execution_binding_id: "test".into(),
                    credential_ref: CredentialRef {
                        store_scope: "test-domain".into(),
                        secret_ref: format!("acct.{id}"),
                    },
                    enabled: true,
                    user_priority: priority,
                    config_revision: 1,
                    identity_generation: 1,
                }
            })
            .collect();
        let first = accounts[0].account_id.clone();
        let second = accounts[1].account_id.clone();
        let mut policies = std::collections::BTreeMap::new();
        policies.insert("test".into(), ProviderAccountPolicy::default());
        let snapshot = AccountSnapshot {
            store_scope: "test-domain".into(),
            config_revision: 1,
            accounts,
            health: vec![],
            model_entitlements: vec![],
            scoped_health: vec![],
            model_availability: vec![],
            provider_policies: policies,
        };
        let directory = Arc::new(RwLock::new(snapshot.clone()));
        let pool = Arc::new(llm_runtime::accounts::AccountPool::from_directory(
            directory.clone(),
            Arc::new(
                FileAccountPoolStore::new(root.path().to_owned())
                    .with_directory_authority(directory),
            ),
        ));
        provider_config::assemble_account_profiles(&mut assembled, &snapshot.accounts).unwrap();
        let runtime = llm_runtime::ModelRuntime::from_config(assembled.client_config)
            .unwrap()
            .with_credential_provider(Arc::new(llm_runtime::StaticCredentialProvider::new(
                llm_runtime::Credential::ApiKey("fixture".into()),
            )));
        let (runtime, _source) = attach_mobile_account_runtime(runtime, pool);
        let wire = Arc::new(AccountWire(std::sync::atomic::AtomicUsize::new(0)));
        let service = llm_runtime::ApiService::new(
            Arc::new(runtime),
            wire.clone(),
            Default::default(),
            Default::default(),
            "test",
            None,
            None,
        );
        let routing = Arc::new(Routing::default());
        service.set_account_routing_reporter(routing.clone());
        let request = || {
            let mut request = llm_runtime::LlmRequest::new(&model)
                .with_profile("test")
                .with_user_text("hello");
            request.input.max_tokens = Some(128);
            request
        };
        llm_runtime::catalog_task::scope(async {
            service.execute_side_query_request(request()).await.unwrap();
            assert_eq!(
                routing
                    .0
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .selected_account_id
                    .as_deref(),
                Some(first.as_str())
            );
            // A different operation has durably staged A's credential commit.
            // B remains a usable inference candidate while that intent exists.
            let pending=serde_json::json!({"snapshot":snapshot,"credential_ref":snapshot.accounts[0].credential_ref,"identity_generation":2,"credential_revision":2});
            rooted_fs::atomic_write(root.path(),Path::new("provider-accounts/test-domain.json"),&serde_json::to_vec(&snapshot).unwrap(),Default::default()).unwrap();
            rooted_fs::atomic_write(root.path(),Path::new("provider-accounts/test-domain.pending"),&serde_json::to_vec(&pending).unwrap(),Default::default()).unwrap();
            service.execute_side_query_request(request()).await.unwrap();
            assert_eq!(routing.0.lock().unwrap().last().unwrap().selected_account_id.as_deref(),Some(second.as_str()));
            rooted_fs::remove_file(root.path(),Path::new("provider-accounts/test-domain.pending")).unwrap();
            let mut external = snapshot.clone();
            external.config_revision = 2;
            for account in &mut external.accounts {
                account.config_revision = 2;
                account.user_priority = 1 - account.user_priority;
            }
            rooted_fs::atomic_write(
                root.path(),
                Path::new("provider-accounts/test-domain.json"),
                &serde_json::to_vec(&external).unwrap(),
                Default::default(),
            )
            .unwrap();
            service.execute_side_query_request(request()).await.unwrap();
            assert_eq!(
                routing
                    .0
                    .lock()
                    .unwrap()
                    .last()
                    .unwrap()
                    .selected_account_id
                    .as_deref(),
                Some(second.as_str())
            );
            external.config_revision = 3;
            external.accounts.clear();
            rooted_fs::atomic_write(
                root.path(),
                Path::new("provider-accounts/test-domain.json"),
                &serde_json::to_vec(&external).unwrap(),
                Default::default(),
            )
            .unwrap();
            assert!(matches!(
                service.execute_side_query_request(request()).await,
                Err(llm_runtime::LlmError::AllAccountsUnavailable { .. })
            ));
            assert_eq!(wire.0.load(std::sync::atomic::Ordering::SeqCst), 3);
        })
        .await;
    }
}
