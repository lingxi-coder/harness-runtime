//! Host-local account directory and credential commit authority.
//! Metadata writes use an exclusive cross-process lock plus atomic replacement.
use lingxi_core::host::provider_accounts::{
    AccountAuthKind, AccountHealth, AccountHealthStatus, AccountMetadata, AccountSnapshot,
    CredentialRef, ProviderAccountAction, ProviderAccountCommand,
};
use provider_config::AccountRegistry;
use secret::{AccountCredential, AccountCredentialKind, CredentialManager};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub use provider_config::AccountCredentialVersion;
/// Explicit host credential publication; metadata must already name the exact live principal.
pub async fn publish_account_credential_version(
    data_dir: PathBuf,
    store_scope: String,
    version: AccountCredentialVersion,
) -> Result<(), String> {
    validate_scope(&store_scope)?;
    tokio::task::spawn_blocking(move || {
        let _lock = lingxi_core::host::rooted_fs::lock_exclusive(
            &data_dir,
            &PathBuf::from("provider-accounts").join(format!("{store_scope}.lock")),
            0o700,
            0o600,
        )
        .map_err(|_| "account directory lock unavailable")?;
        let snapshot = load_account_snapshot(&data_dir, &store_scope)?;
        let account = snapshot
            .accounts
            .iter()
            .find(|a| {
                a.account_id == version.account_id
                    && a.identity_generation == version.identity_generation
                    && a.credential_ref.store_scope == store_scope
            })
            .ok_or("account credential version identity mismatch")?;
        provider_config::write_account_credential_version_locked(
            &data_dir,
            account,
            version.credential_revision,
        )
    })
    .await
    .map_err(|_| "account credential version worker failed")?
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AccountCommandFingerprint {
    nonce: String,
    digest: String,
}
impl AccountCommandFingerprint {
    fn digest(
        command: &ProviderAccountCommand,
        credential: Option<&str>,
        nonce: &str,
    ) -> Result<String, String> {
        let bytes = serde_json::to_vec(command).map_err(|_| "account command encoding failed")?;
        let mut hash = Sha256::new();
        hash.update((nonce.len() as u64).to_be_bytes());
        hash.update(nonce.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(bytes);
        match credential {
            Some(value) => {
                hash.update((value.len() as u64).to_be_bytes());
                hash.update(value.as_bytes());
            }
            None => hash.update(u64::MAX.to_be_bytes()),
        };
        Ok(format!("{:x}", hash.finalize()))
    }
    fn matches(
        &self,
        command: &ProviderAccountCommand,
        credential: Option<&str>,
    ) -> Result<bool, String> {
        if uuid::Uuid::parse_str(&self.nonce).is_err() {
            return Err("account transaction fingerprint invalid".into());
        }
        Ok(self.digest == Self::digest(command, credential, &self.nonce)?)
    }
}
fn account_command_fingerprint(
    command: &ProviderAccountCommand,
    credential: Option<&str>,
) -> Result<AccountCommandFingerprint, String> {
    let nonce = uuid::Uuid::new_v4().to_string();
    let digest = AccountCommandFingerprint::digest(command, credential, &nonce)?;
    Ok(AccountCommandFingerprint { nonce, digest })
}

/// The configured data root already carries brand/channel identity. The backend
/// identifies the actual credential ownership domain, including Desktop Broker.
pub fn account_store_scope(data_dir: &Path, backend: &str) -> String {
    let mut hash = Sha256::new();
    let absolute = if data_dir.is_absolute() {
        data_dir.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(data_dir)
    };
    let components: Vec<_> = absolute
        .components()
        .map(|part| part.as_os_str().to_owned())
        .collect();
    let mut normalized = absolute.clone();
    for count in (0..=components.len()).rev() {
        let ancestor: PathBuf = components[..count].iter().collect();
        if let Ok(mut canonical) = std::fs::canonicalize(&ancestor) {
            for component in &components[count..] {
                if component == ".." {
                    canonical.pop();
                } else if component != "." {
                    canonical.push(component);
                }
            }
            normalized = canonical;
            break;
        }
    }
    hash.update(normalized.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(backend.as_bytes());
    format!("acct_{:x}", hash.finalize())
}
fn directory_path(root: &Path, scope: &str) -> PathBuf {
    root.join("provider-accounts").join(format!("{scope}.json"))
}
fn validate_scope(scope: &str) -> Result<(), String> {
    if scope.is_empty()
        || scope.len() > 128
        || !scope
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err("invalid account storage scope".into());
    }
    Ok(())
}
/// Read-only bootstrap loader. Never creates directories, registers or repairs credentials.
pub fn load_account_snapshot(
    data_dir: &Path,
    store_scope: &str,
) -> Result<AccountSnapshot, String> {
    validate_scope(store_scope)?;
    let pending = PathBuf::from("provider-accounts").join(format!("{store_scope}.pending"));
    match lingxi_core::host::rooted_fs::read_to_string_limited(data_dir, &pending, 4 * 1024 * 1024)
    {
        Ok(text) => {
            let intent: AccountCommitIntent =
                serde_json::from_str(&text).map_err(|_| "account transaction invalid")?;
            let mut snapshot = load_account_snapshot_raw(data_dir, store_scope)?;
            snapshot
                .provider_policies
                .extend(intent.snapshot.provider_policies);
            let affected = snapshot
                .accounts
                .iter()
                .find(|a| a.credential_ref == intent.credential_ref)
                .cloned()
                .or_else(|| {
                    intent
                        .snapshot
                        .accounts
                        .into_iter()
                        .find(|a| a.credential_ref == intent.credential_ref)
                });
            if let Some(account) = affected {
                snapshot
                    .health
                    .retain(|h| h.account_id != account.account_id);
                snapshot.health.push(AccountHealth {
                    account_id: account.account_id.clone(),
                    identity_generation: account.identity_generation,
                    status: AccountHealthStatus::StorageUnavailable,
                    reason: Some("credential_commit_pending".into()),
                    cooldown_until_ms: None,
                    health_revision: 0,
                });
            }
            return Ok(snapshot);
        }
        Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => {}
        Err(_) => return Err("account transaction state unavailable".into()),
    }
    load_account_snapshot_raw(data_dir, store_scope)
}
fn load_account_snapshot_raw(
    data_dir: &Path,
    store_scope: &str,
) -> Result<AccountSnapshot, String> {
    let path = PathBuf::from("provider-accounts").join(format!("{store_scope}.json"));
    match lingxi_core::host::rooted_fs::read_to_string_limited(data_dir, &path, 4 * 1024 * 1024) {
        Ok(bytes) => {
            let snapshot: AccountSnapshot =
                serde_json::from_str(&bytes).map_err(|_| "account directory is invalid")?;
            if snapshot.store_scope != store_scope {
                return Err("account directory scope mismatch".into());
            }
            {
                let mut snapshot = AccountRegistry::from_snapshot(snapshot)?.snapshot();
                snapshot.health.clear();
                snapshot.model_entitlements.clear();
                snapshot.model_availability.clear();
                snapshot.scoped_health.clear();
                read_account_health(data_dir, &mut snapshot)?;
                Ok(snapshot)
            }
        }
        Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => {
            Ok(AccountRegistry::new(store_scope.into()).snapshot())
        }
        Err(_) => Err("account directory unavailable".into()),
    }
}

/// Live catalog inputs owned by the host, separate from account credentials.
#[derive(Clone)]
pub struct ProviderAccountCatalog {
    inputs: provider_config::AssembleInputs,
    region: llm_runtime::Region,
    source: Option<provider_config::startup_catalog::StartupCatalog>,
    enabled_profiles: Option<std::collections::BTreeSet<String>>,
}
impl ProviderAccountCatalog {
    /// Catalog reads use only the currently published snapshot and never fetch models.
    pub fn new(
        inputs: provider_config::AssembleInputs,
        region: llm_runtime::Region,
        source: Option<provider_config::startup_catalog::StartupCatalog>,
        enabled_profiles: Option<std::collections::BTreeSet<String>>,
    ) -> Self {
        Self {
            inputs,
            region,
            source,
            enabled_profiles,
        }
    }
    fn profiles(
        &self,
    ) -> (
        Vec<llm_runtime::ProviderProfile>,
        Vec<llm_runtime::ProviderProfile>,
    ) {
        let mut inputs = self.inputs.clone();
        if let Some(source) = &self.source {
            inputs.official_catalog = Some((*source.snapshot()).clone());
        }
        let management =
            provider_config::assemble_management_for_region(inputs.clone(), self.region)
                .client_config
                .providers;
        let mut execution = provider_config::assemble_for_region(inputs, self.region)
            .client_config
            .providers;
        if let Some(enabled) = &self.enabled_profiles {
            execution.retain(|profile| provider_config::mobile_profile_allowed(profile, enabled));
        }
        (management, execution)
    }
}

#[derive(Clone)]
pub struct ProviderAccountStore {
    catalog: Option<ProviderAccountCatalog>,
    profiles: Vec<llm_runtime::ProviderProfile>,
    execution_profiles: Option<Vec<llm_runtime::ProviderProfile>>,
    data_dir: PathBuf,
    store_scope: String,
    credentials: Arc<CredentialManager>,
}
impl ProviderAccountStore {
    pub fn new(
        data_dir: PathBuf,
        store_scope: String,
        credentials: Arc<CredentialManager>,
    ) -> Self {
        Self {
            catalog: None,
            profiles: llm_runtime::builtin_presets().providers,
            execution_profiles: None,
            data_dir,
            store_scope,
            credentials,
        }
    }
    /// Use host-owned settings with the latest published catalog for each operation.
    pub fn with_catalog(mut self, catalog: ProviderAccountCatalog) -> Self {
        self.catalog = Some(catalog);
        self
    }
    fn frozen_catalog(&self) -> Self {
        let mut frozen = self.clone();
        if let Some(catalog) = frozen.catalog.take() {
            let (management, execution) = catalog.profiles();
            frozen.profiles = management;
            frozen.execution_profiles = Some(execution);
        }
        frozen
    }
    pub fn with_profiles(mut self, profiles: Vec<llm_runtime::ProviderProfile>) -> Self {
        self.profiles = profiles;
        self
    }
    /// Actual enabled runtime bindings, kept separate from disabled definitions used for management.
    pub fn with_execution_profiles(mut self, profiles: Vec<llm_runtime::ProviderProfile>) -> Self {
        self.execution_profiles = Some(profiles);
        self
    }
    fn execution_profiles(&self) -> &[llm_runtime::ProviderProfile] {
        self.execution_profiles.as_deref().unwrap_or(&self.profiles)
    }
    fn validate_execution_binding(
        &self,
        provider: &str,
        binding: &str,
        auth: AccountAuthKind,
    ) -> Result<(), String> {
        let profile = self
            .profiles
            .iter()
            .find(|p| p.profile_name == binding)
            .ok_or("unknown execution binding")?;
        let logical = if binding == "openai-chatgpt" {
            "openai"
        } else {
            profile.group()
        };
        if provider != logical {
            return Err("execution binding belongs to a different logical provider".into());
        }
        validate_binding(provider, binding, auth)?;
        if auth == AccountAuthKind::OAuth {
            let endpoint = url::Url::parse(&profile.base_url)
                .map_err(|_| "invalid OAuth execution endpoint")?;
            if endpoint.scheme() != "https"
                || endpoint.host_str() != Some("chatgpt.com")
                || endpoint.port().is_some_and(|port| port != 443)
            {
                return Err("first-party OAuth cannot authenticate this endpoint".into());
            }
        }
        if auth == AccountAuthKind::ApiKey
            && matches!(
                profile.auth,
                llm_runtime::AuthStrategy::AwsSigV4
                    | llm_runtime::AuthStrategy::GcpToken
                    | llm_runtime::AuthStrategy::None
            )
        {
            return Err("API key authentication is not supported by this execution binding".into());
        }
        Ok(())
    }
    pub fn credentials(&self) -> Arc<CredentialManager> {
        self.credentials.clone()
    }
    pub fn store_scope(&self) -> &str {
        &self.store_scope
    }
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }
    pub async fn snapshot(&self) -> Result<AccountSnapshot, String> {
        self.frozen_catalog().snapshot_inner().await
    }
    async fn snapshot_inner(&self) -> Result<AccountSnapshot, String> {
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        let mut snapshot =
            tokio::task::spawn_blocking(move || load_account_snapshot(&root, &scope))
                .await
                .map_err(|_| "account directory worker failed")??;
        for account in &snapshot.accounts {
            if account.enabled
                && snapshot.health.iter().any(|h| {
                    h.account_id == account.account_id
                        && h.identity_generation == account.identity_generation
                })
            {
                continue;
            }
            snapshot
                .health
                .retain(|h| h.account_id != account.account_id);
            let (status, reason) = if !self
                .profiles
                .iter()
                .any(|p| p.profile_name == account.execution_binding_id)
            {
                (
                    AccountHealthStatus::Disabled,
                    Some("execution_binding_unavailable".into()),
                )
            } else if !account.enabled {
                (AccountHealthStatus::Disabled, None)
            } else {
                match self
                    .credentials
                    .has_account_credential(&account.credential_ref)
                    .await
                {
                    Ok(true) => match provider_config::read_account_credential_version(
                        &self.data_dir,
                        account,
                    ) {
                        Ok(Some(_)) => (AccountHealthStatus::Ready, None),
                        Ok(None) => (
                            AccountHealthStatus::StorageUnavailable,
                            Some("credential_version_unpublished".into()),
                        ),
                        Err(_) => (
                            AccountHealthStatus::StorageUnavailable,
                            Some("credential_version_unavailable".into()),
                        ),
                    },
                    Ok(false) => (
                        AccountHealthStatus::NeedsAuth,
                        Some("credential_missing".into()),
                    ),
                    Err(_) => (
                        AccountHealthStatus::StorageUnavailable,
                        Some("storage_unavailable".into()),
                    ),
                }
            };
            snapshot.health.push(AccountHealth {
                account_id: account.account_id.clone(),
                identity_generation: account.identity_generation,
                status,
                reason,
                cooldown_until_ms: None,
                health_revision: 0,
            });
        }
        let mut config = llm_runtime::ClientConfig {
            providers: self.execution_profiles().to_vec(),
        };
        provider_config::assemble_account_client_config(&mut config, &snapshot.accounts)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        snapshot.model_availability =
            llm_runtime::ModelRuntime::project_account_model_availability(
                &config, &snapshot, now_ms,
            )
            .map_err(|_| "account model availability unavailable")?;
        Ok(snapshot)
    }
    /// Project a metadata snapshot with the caller's attribute-only credential presence.
    /// This performs no credential reads, refreshes, requests, claims or persistence.
    pub fn project_model_availability_with_presence(
        &self,
        snapshot: AccountSnapshot,
        presence: std::collections::BTreeSet<String>,
    ) -> Result<AccountSnapshot, String> {
        self.frozen_catalog()
            .project_model_availability_with_presence_inner(snapshot, presence)
    }
    fn project_model_availability_with_presence_inner(
        &self,
        mut snapshot: AccountSnapshot,
        presence: std::collections::BTreeSet<String>,
    ) -> Result<AccountSnapshot, String> {
        if snapshot.store_scope != self.store_scope
            || presence
                .iter()
                .any(|id| !snapshot.accounts.iter().any(|a| &a.account_id == id))
        {
            return Err("account presence snapshot mismatch".into());
        }
        for account in &snapshot.accounts {
            let has_fault = snapshot.health.iter().any(|h| {
                h.account_id == account.account_id
                    && h.identity_generation == account.identity_generation
                    && h.status != AccountHealthStatus::Ready
            });
            if !account.enabled || !presence.contains(&account.account_id) || !has_fault {
                snapshot
                    .health
                    .retain(|h| h.account_id != account.account_id);
                let status = if !account.enabled {
                    AccountHealthStatus::Disabled
                } else if presence.contains(&account.account_id) {
                    AccountHealthStatus::Ready
                } else {
                    AccountHealthStatus::NeedsAuth
                };
                snapshot.health.push(AccountHealth {
                    account_id: account.account_id.clone(),
                    identity_generation: account.identity_generation,
                    status,
                    reason: (!presence.contains(&account.account_id))
                        .then(|| "credential_missing".into()),
                    cooldown_until_ms: None,
                    health_revision: 0,
                });
            }
        }
        let mut config = llm_runtime::ClientConfig {
            providers: self.execution_profiles().to_vec(),
        };
        provider_config::assemble_account_client_config(&mut config, &snapshot.accounts)?;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        snapshot.model_availability =
            llm_runtime::ModelRuntime::project_account_model_availability(
                &config, &snapshot, now_ms,
            )
            .map_err(|_| "account model availability unavailable")?;
        Ok(snapshot)
    }
    /// Broker metadata reads omit native presence checks; persisted scheduler health is retained.
    pub async fn snapshot_metadata(&self) -> Result<AccountSnapshot, String> {
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        tokio::task::spawn_blocking(move || load_account_snapshot(&root, &scope))
            .await
            .map_err(|_| "account directory worker failed")?
    }
    pub async fn execute(
        &self,
        command: ProviderAccountCommand,
        credential: Option<String>,
    ) -> Result<AccountSnapshot, String> {
        self.frozen_catalog()
            .execute_inner(command, credential, false)
            .await
    }
    /// Desktop Broker owns all secrets. Metadata operations never touch native storage.
    pub async fn execute_metadata(
        &self,
        command: ProviderAccountCommand,
    ) -> Result<AccountSnapshot, String> {
        self.frozen_catalog()
            .execute_inner(command, None, true)
            .await
    }
    async fn execute_inner(
        &self,
        command: ProviderAccountCommand,
        credential: Option<String>,
        external: bool,
    ) -> Result<AccountSnapshot, String> {
        if matches!(&command.action, ProviderAccountAction::List) {
            return if external {
                self.snapshot_metadata().await
            } else {
                self.snapshot().await
            };
        }
        if let ProviderAccountAction::Test { account_id } = &command.action {
            return self.test_account(account_id).await;
        }
        validate_scope(&self.store_scope)?;
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        let (lock, mut registry) = tokio::task::spawn_blocking(move || {
            let path = directory_path(&root, &scope);
            std::fs::create_dir_all(path.parent().ok_or("account directory unavailable")?)
                .map_err(|_| "account directory unavailable")?;
            let lock_path = path.with_extension("lock");
            let lock = lingxi_core::host::rooted_fs::lock_exclusive(
                &root,
                &lock_path
                    .strip_prefix(&root)
                    .map_err(|_| "account directory unavailable")?,
                0o700,
                0o600,
            )
            .map_err(|_| "account directory lock unavailable")?;
            Ok::<_, String>((
                lock,
                AccountRegistry::from_snapshot(load_account_snapshot_raw(&root, &scope)?)?,
            ))
        })
        .await
        .map_err(|_| "account directory worker failed")??;
        let (lock, recovered) = self
            .recover_pending(lock, &command, credential.as_deref(), &registry)
            .await?;
        if let Some((recovered, same_operation)) = recovered {
            let result = self.commit_transaction(lock, recovered, None, None).await?;
            return if same_operation {
                Ok(result)
            } else {
                Err(
                    "account transaction recovered; refresh configuration and retry operation"
                        .into(),
                )
            };
        }
        let fingerprint = account_command_fingerprint(&command, credential.as_deref())?;
        registry.check_revision(command.expected_revision)?;
        let mut mutation: Option<CredentialMutation> = None;
        let find = |id: &str| {
            registry
                .snapshot()
                .accounts
                .into_iter()
                .find(|a| a.account_id == id)
                .ok_or_else(|| "unknown account".to_string())
        };
        match command.action {
            ProviderAccountAction::SetPolicy {
                provider_id,
                allow_subscription_to_metered,
            } => registry.set_policy(
                provider_id,
                allow_subscription_to_metered,
                command.expected_revision,
            )?,
            ProviderAccountAction::List => {
                return if external {
                    self.snapshot_metadata().await
                } else {
                    self.snapshot().await
                }
            }
            ProviderAccountAction::Add {
                provider_id,
                display_name,
                execution_binding_id,
                auth_kind,
                account_id,
            } => {
                self.validate_execution_binding(&provider_id, &execution_binding_id, auth_kind)?;
                let id = account_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                if uuid::Uuid::parse_str(&id).is_err() {
                    return Err("invalid account identifier".into());
                }
                let root = self.data_dir.clone();
                let scope = self.store_scope.clone();
                let retired_id = id.clone();
                tokio::task::spawn_blocking(move || {
                    match lingxi_core::host::rooted_fs::read_to_string_limited(
                        &root,
                        &PathBuf::from("provider-accounts/retired")
                            .join(scope)
                            .join(format!("{retired_id}.json")),
                        4096,
                    ) {
                        Ok(_) => Err("account identifier was retired; create a new account"),
                        Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => Ok(()),
                        Err(_) => Err("account identifier history unavailable"),
                    }
                })
                .await
                .map_err(|_| "account history worker failed")??;
                let reference = CredentialRef {
                    store_scope: self.store_scope.clone(),
                    secret_ref: format!("acct.{id}"),
                };
                let account = AccountMetadata {
                    account_id: id,
                    provider_id,
                    display_name,
                    execution_binding_id,
                    auth_kind,
                    credential_ref: reference.clone(),
                    enabled: true,
                    user_priority: 0,
                    config_revision: 0,
                    identity_generation: 1,
                };
                registry.add(account, command.expected_revision)?;
                if let Some(key) = credential {
                    if auth_kind != AccountAuthKind::ApiKey {
                        return Err("OAuth material must use the authorization flow".into());
                    }
                    mutation = Some(CredentialMutation::Store(
                        reference,
                        self.key_record(key, 1, 0)?,
                    ));
                }
            }
            ProviderAccountAction::Reorder {
                provider_id,
                account_ids,
            } => registry.reorder(&provider_id, &account_ids, command.expected_revision)?,
            ProviderAccountAction::Remove { account_id } => {
                let account = find(&account_id)?;
                if registry.snapshot().config_revision != command.expected_revision {
                    return Err("account configuration conflict".into());
                }
                let root = self.data_dir.clone();
                let scope = self.store_scope.clone();
                let retired = account_id.clone();
                tokio::task::spawn_blocking(move || {
                    lingxi_core::host::rooted_fs::atomic_write(
                        &root,
                        &PathBuf::from("provider-accounts/retired")
                            .join(scope)
                            .join(format!("{retired}.json")),
                        b"{}",
                        Default::default(),
                    )
                    .map_err(|_| "account identifier retirement failed")
                })
                .await
                .map_err(|_| "account history worker failed")??;
                if !external {
                    mutation = Some(CredentialMutation::Delete(account.credential_ref.clone()));
                }
                registry.remove(&account_id, command.expected_revision)?;
            }
            ProviderAccountAction::Update {
                account_id,
                display_name,
            } => {
                let mut account = find(&account_id)?;
                account.display_name = display_name;
                registry.update(account, command.expected_revision)?;
            }
            ProviderAccountAction::SetEnabled {
                account_id,
                enabled,
            } => {
                let mut account = find(&account_id)?;
                account.enabled = enabled;
                registry.update(account, command.expected_revision)?;
            }
            ProviderAccountAction::SetCredential { account_id } => {
                let mut account = find(&account_id)?;
                if account.auth_kind != AccountAuthKind::ApiKey && !external {
                    return Err("OAuth account requires authorization".into());
                }
                account.identity_generation = account
                    .identity_generation
                    .checked_add(1)
                    .ok_or("account generation exhausted")?;
                if !external {
                    let revision = self
                        .credentials
                        .get_account_credential(&account.credential_ref)
                        .await
                        .map_err(|_| "credential storage unavailable")?
                        .map_or(0, |c| c.credential_revision);
                    mutation = Some(CredentialMutation::Store(
                        account.credential_ref.clone(),
                        self.key_record(
                            credential.ok_or("credential required")?,
                            account.identity_generation,
                            revision,
                        )?,
                    ));
                }
                registry.update(account, command.expected_revision)?;
            }
            ProviderAccountAction::Logout { account_id }
            | ProviderAccountAction::ClearCredential { account_id } => {
                let mut account = find(&account_id)?;
                if !external {
                    mutation = Some(CredentialMutation::Delete(account.credential_ref.clone()));
                }
                account.identity_generation = account
                    .identity_generation
                    .checked_add(1)
                    .ok_or("account generation exhausted")?;
                registry.update(account, command.expected_revision)?;
            }
            ProviderAccountAction::Test { .. } => {
                return Err("account tests require a host transport".into())
            }
            ProviderAccountAction::BeginOauth { .. }
            | ProviderAccountAction::PollOauth { .. }
            | ProviderAccountAction::CompleteOauth { .. }
            | ProviderAccountAction::CancelOauth { .. } => {
                return Err("OAuth requires the host authorization coordinator".into())
            }
        }
        self.commit_transaction(lock, registry.snapshot(), mutation, Some(fingerprint))
            .await
    }
    pub async fn verify_account(
        &self,
        expected: &AccountMetadata,
    ) -> Result<AccountMetadata, String> {
        let snapshot = self.snapshot().await?;
        if snapshot.health.iter().any(|h| {
            h.account_id == expected.account_id
                && h.reason.as_deref() == Some("credential_commit_pending")
        }) {
            return Err("account credential transaction pending".into());
        }
        let account = snapshot
            .accounts
            .into_iter()
            .find(|a| a.account_id == expected.account_id)
            .ok_or("account was removed")?;
        if account.identity_generation != expected.identity_generation
            || account.config_revision != expected.config_revision
        {
            return Err("account changed during authorization".into());
        }
        Ok(account)
    }
    /// Principal fencing tolerates non-identity metadata edits during an upstream grant rotation.
    async fn verify_identity(&self, expected: &AccountMetadata) -> Result<AccountMetadata, String> {
        let (_lock, registry) = self.locked_registry_for(Some(&expected.account_id)).await?;
        let account = registry
            .snapshot()
            .accounts
            .into_iter()
            .find(|a| a.account_id == expected.account_id)
            .ok_or("account was removed")?;
        if account.identity_generation != expected.identity_generation
            || account.credential_ref != expected.credential_ref
            || account.auth_kind != expected.auth_kind
        {
            return Err("account identity changed during authorization".into());
        }
        Ok(account)
    }
    async fn locked_registry(
        &self,
    ) -> Result<(lingxi_core::host::RootedFileLock, AccountRegistry), String> {
        self.locked_registry_for(None).await
    }
    async fn locked_registry_for(
        &self,
        account_id: Option<&str>,
    ) -> Result<(lingxi_core::host::RootedFileLock, AccountRegistry), String> {
        let account_id = account_id.map(str::to_owned);
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        tokio::task::spawn_blocking(move || {
            let lock = lingxi_core::host::rooted_fs::lock_exclusive(
                &root,
                &PathBuf::from("provider-accounts").join(format!("{scope}.lock")),
                0o700,
                0o600,
            )
            .map_err(|_| "account directory lock unavailable")?;
            let snapshot = load_account_snapshot(&root, &scope)?;
            if snapshot.health.iter().any(|h| {
                h.reason.as_deref() == Some("credential_commit_pending")
                    && account_id.as_ref().is_none_or(|id| &h.account_id == id)
            }) {
                return Err("account credential transaction pending".into());
            }
            Ok((lock, AccountRegistry::from_snapshot(snapshot)?))
        })
        .await
        .map_err(|_| "account directory worker failed")?
    }
    pub async fn commit_oauth(
        &self,
        expected: &AccountMetadata,
        mut credential: AccountCredential,
    ) -> Result<AccountSnapshot, String> {
        let (lock, mut registry) = self.locked_registry().await?;
        let mut account = registry
            .snapshot()
            .accounts
            .into_iter()
            .find(|a| a.account_id == expected.account_id)
            .ok_or("account was removed")?;
        if account.identity_generation != expected.identity_generation
            || account.credential_ref != expected.credential_ref
            || account.auth_kind != expected.auth_kind
        {
            return Err("account identity changed during authorization".into());
        }
        if let AccountCredentialKind::OAuth {
            subject: Some(subject),
            organization_id: workspace,
            issuer,
            ..
        } = &credential.kind
        {
            for other in registry.snapshot().accounts.iter().filter(|a| {
                a.account_id != account.account_id
                    && a.provider_id == account.provider_id
                    && a.auth_kind == AccountAuthKind::OAuth
            }) {
                if let Some(AccountCredential {
                    kind:
                        AccountCredentialKind::OAuth {
                            subject: Some(other_subject),
                            issuer: other_issuer,
                            organization_id: other_workspace,
                            ..
                        },
                    ..
                }) = self
                    .credentials
                    .get_account_credential(&other.credential_ref)
                    .await
                    .map_err(|_| "credential storage unavailable")?
                {
                    if subject == &other_subject
                        && issuer == &other_issuer
                        && workspace == &other_workspace
                    {
                        return Err("this upstream account is already registered".into());
                    }
                }
            }
        }
        if account.auth_kind != AccountAuthKind::OAuth
            || !matches!(&credential.kind, AccountCredentialKind::OAuth { .. })
        {
            return Err("account authentication kind mismatch".into());
        }
        let old = self
            .credentials
            .get_account_credential(&account.credential_ref)
            .await
            .map_err(|_| "credential storage unavailable")?;
        if let Some(AccountCredential {
            kind:
                AccountCredentialKind::OAuth {
                    subject: Some(old_subject),
                    ..
                },
            ..
        }) = &old
        {
            if let AccountCredentialKind::OAuth {
                subject: new_subject,
                ..
            } = &credential.kind
            {
                let new_subject = new_subject
                    .as_ref()
                    .ok_or("authorization did not identify the upstream account")?;
                if old_subject != new_subject {
                    return Err(
                        "authorization returned a different upstream account; add a new account"
                            .into(),
                    );
                }
            }
        }
        if let (
            Some(AccountCredential {
                kind:
                    AccountCredentialKind::OAuth {
                        organization_id: Some(old_workspace),
                        ..
                    },
                ..
            }),
            AccountCredentialKind::OAuth {
                organization_id: new_workspace,
                ..
            },
        ) = (&old, &credential.kind)
        {
            if new_workspace.as_ref() != Some(old_workspace) {
                return Err(
                    "authorization returned a different workspace; add a new account".into(),
                );
            }
        }
        account.identity_generation = account
            .identity_generation
            .checked_add(1)
            .ok_or("account generation exhausted")?;
        credential.identity_generation = account.identity_generation;
        credential.credential_revision = old
            .as_ref()
            .map_or(0, |c| c.credential_revision)
            .checked_add(1)
            .ok_or("credential revision exhausted")?;
        let revision = registry.snapshot().config_revision;
        account.config_revision = revision;
        registry.update(account.clone(), revision)?;
        self.commit_transaction(
            lock,
            registry.snapshot(),
            Some(CredentialMutation::Store(
                account.credential_ref.clone(),
                credential,
            )),
            None,
        )
        .await
    }
    /// Execute an exact-account, read-only model-directory/authentication probe.
    pub async fn test_account(&self, account_id: &str) -> Result<AccountSnapshot, String> {
        self.frozen_catalog().test_account_inner(account_id).await
    }
    async fn test_account_inner(&self, account_id: &str) -> Result<AccountSnapshot, String> {
        use llm_runtime::services::sdk::directory::probe::{ProbeCredential, ProbeProtocol};
        let snapshot = self.snapshot().await?;
        let account = snapshot
            .accounts
            .iter()
            .find(|a| a.account_id == account_id)
            .ok_or("unknown account")?;
        let profile = self
            .profiles
            .iter()
            .find(|p| p.profile_name == account.execution_binding_id)
            .ok_or("account execution binding unavailable for test")?;
        let mut record = self
            .credentials
            .get_account_credential(&account.credential_ref)
            .await
            .map_err(|_| "credential storage unavailable")?
            .ok_or("account requires authentication")?;
        if record.identity_generation != account.identity_generation {
            return Err("account credential generation mismatch".into());
        }
        if matches!(&record.kind,AccountCredentialKind::OAuth{expires_at,..} if *expires_at<=std::time::SystemTime::now()+std::time::Duration::from_secs(60))
        {
            record = self
                .credentials
                .refresh_account_credential(account.clone(), record.credential_revision)
                .await
                .map_err(|_| "provider account refresh failed")?
                .ok_or("account requires authentication")?;
        }
        if provider_config::read_account_credential_version(&self.data_dir, account)?
            != Some(record.credential_revision)
        {
            return Err("account credential version unpublished".into());
        }
        let credential = match record.kind {
            AccountCredentialKind::ApiKey { key } => ProbeCredential {
                token: llm_runtime::services::sdk::protocol::Secret::new(
                    key.expose_secret().clone(),
                ),
                bearer: false,
                account_id: None,
                fedramp: false,
            },
            AccountCredentialKind::OAuth {
                access_token,
                organization_id,
                fedramp,
                ..
            } => ProbeCredential {
                token: llm_runtime::services::sdk::protocol::Secret::new(
                    access_token.expose_secret().clone(),
                ),
                bearer: true,
                account_id: organization_id,
                fedramp,
            },
        };
        let protocol = if account.execution_binding_id == "openai-chatgpt" {
            ProbeProtocol::ChatGpt
        } else {
            match profile.protocol {
                llm_runtime::ProtocolFamily::AnthropicMessages => ProbeProtocol::Anthropic,
                llm_runtime::ProtocolFamily::GeminiGenerateContent => ProbeProtocol::Google,
                _ => ProbeProtocol::OpenAi,
            }
        };
        let transport = platform_common::provider_transport()
            .map_err(|_| "provider test transport unavailable")?;
        let result = llm_runtime::services::sdk::directory::probe::probe(
            &transport,
            &profile.base_url,
            protocol,
            &credential,
            std::time::Duration::from_secs(15),
        )
        .await
        .map_err(|_| "provider account test failed")?;
        self.verify_account(account).await?;
        match result.status {
            200..=299 => {
                self.clear_verified_auth_health(account, result.model_ids.unwrap_or_default())
                    .await?;
                self.snapshot().await
            }
            401 | 403 => Err("provider account authentication rejected".into()),
            429 => Err("provider account rate limited".into()),
            _ => Err("provider account endpoint rejected the test".into()),
        }
    }

    async fn clear_verified_auth_health(
        &self,
        account: &AccountMetadata,
        discovered: Vec<String>,
    ) -> Result<(), String> {
        use llm_runtime::accounts::{FailureScope, HealthDocument, HealthStatus};
        let (directory_lock, registry) =
            self.locked_registry_for(Some(&account.account_id)).await?;
        if !registry.snapshot().accounts.iter().any(|a| {
            a.account_id == account.account_id
                && a.identity_generation == account.identity_generation
                && a.config_revision == account.config_revision
        }) {
            return Err("account changed during connection test".into());
        }
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        let account = account.clone();
        tokio::task::spawn_blocking(move || {
            let _directory_lock = directory_lock;
            let hash = format!("{:x}", Sha256::digest(scope.as_bytes()));
            let directory = PathBuf::from("provider-accounts/health");
            let path = directory.join(format!("{hash}.json"));
            let _health_lock = lingxi_core::host::rooted_fs::lock_exclusive(
                &root,
                &directory.join(format!("{hash}.lock")),
                0o700,
                0o600,
            )
            .map_err(|_| "account health lock unavailable")?;
            let mut document = match lingxi_core::host::rooted_fs::read_to_string_limited(
                &root,
                &path,
                4 * 1024 * 1024,
            ) {
                Ok(text) => serde_json::from_str::<HealthDocument>(&text)
                    .map_err(|_| "account health state invalid")?,
                Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => HealthDocument::default(),
                Err(_) => return Err("account health storage unavailable"),
            };
            let identity = serde_json::to_string(&(scope.as_str(), account.account_id.as_str()))
                .map_err(|_| "account health identity invalid")?;
            let before = document.records.len();
            document.records.retain(|key, row| {
                let Ok((stored, generation, failure_scope)) =
                    serde_json::from_str::<(String, u64, FailureScope)>(key)
                else {
                    return true;
                };
                !(stored == identity
                    && generation == account.identity_generation
                    && failure_scope == FailureScope::Account
                    && matches!(
                        row.status,
                        HealthStatus::NeedsAuth | HealthStatus::StorageUnavailable
                    ))
            });
            let mut discovered_changed = false;
            let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().min(u64::MAX as u128) as u64;
            for model in discovered.into_iter().filter(|m| !m.is_empty() && m.len() <= 512 && !m.chars().any(char::is_control)).take(10000) {
                let key = serde_json::to_string(&(identity.as_str(), account.identity_generation, FailureScope::Model(model.clone()))).map_err(|_| "account model identity invalid")?;
                if !document.model_entitlements.contains_key(&key) {
                    document.model_entitlements.insert(key, lingxi_core::host::provider_accounts::AccountModelEntitlement {
                        account_id: account.account_id.clone(), identity_generation: account.identity_generation,
                        model_id: model, status: lingxi_core::host::provider_accounts::AccountModelEntitlementStatus::Unverified,
                        observed_at_ms: now_ms, retry_at_ms: None, reason: Some("discovered".into()),
                    });
                    discovered_changed = true;
                }
            }
            if document.records.len() != before || discovered_changed {
                document.revision = document
                    .revision
                    .checked_add(1)
                    .ok_or("account health revision exhausted")?;
                let bytes =
                    serde_json::to_vec(&document).map_err(|_| "account health encoding failed")?;
                lingxi_core::host::rooted_fs::atomic_write(
                    &root,
                    &path,
                    &bytes,
                    Default::default(),
                )
                .map_err(|_| "account health persistence failed")?;
            }
            Ok(())
        })
        .await
        .map_err(|_| "account health worker failed")?
        .map_err(str::to_owned)
    }

    fn key_record(
        &self,
        key: String,
        generation: u64,
        revision: u64,
    ) -> Result<AccountCredential, String> {
        if key.trim().is_empty() {
            return Err("credential is empty".into());
        }
        Ok(AccountCredential {
            credential_revision: revision
                .checked_add(1)
                .ok_or("credential revision exhausted")?,
            identity_generation: generation,
            kind: AccountCredentialKind::ApiKey {
                key: lingxi_core::types::Secret::new(key),
            },
        })
    }
}
fn validate_binding(provider: &str, binding: &str, auth: AccountAuthKind) -> Result<(), String> {
    if provider.trim().is_empty() || binding.trim().is_empty() {
        return Err("provider and execution binding required".into());
    }
    if auth == AccountAuthKind::OAuth && (provider != "openai" || binding != "openai-chatgpt") {
        return Err("OAuth login is not enabled for this execution binding".into());
    }
    if auth == AccountAuthKind::ApiKey && binding == "openai-chatgpt" {
        return Err("ChatGPT execution binding requires OAuth".into());
    }
    Ok(())
}

/// Account-bound OAuth flow data exposed to native authentication sessions.
#[derive(serde::Serialize)]
pub struct AccountOAuthSession {
    pub mode: AccountOAuthMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    pub interval_seconds: u64,
    pub expires_in_seconds: u64,
    pub provider: String,
    pub flow_id: String,
    pub authorization_url: String,
    pub callback_url_scheme: String,
    pub account_id: String,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountOAuthMode {
    Browser,
    DeviceCode,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountOAuthPollStatus {
    Pending,
    Completed,
}
#[derive(serde::Serialize)]
pub struct AccountOAuthPollResult {
    pub status: AccountOAuthPollStatus,
    pub retry_after_seconds: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<AccountSnapshot>,
}
const FLOW_ACTIVE: u8 = 0;
const FLOW_EXCHANGING: u8 = 1;
const FLOW_COMMITTING: u8 = 2;
const FLOW_COMPLETED: u8 = 3;
const FLOW_CANCELLED: u8 = 4;
const FLOW_FAILED: u8 = 5;
fn flow_live(phase: &std::sync::atomic::AtomicU8) -> bool {
    phase.load(std::sync::atomic::Ordering::Acquire) <= FLOW_COMMITTING
}
fn request_flow_cancel(phase: &std::sync::atomic::AtomicU8) {
    use std::sync::atomic::Ordering;
    loop {
        let current = phase.load(Ordering::Acquire);
        if current != FLOW_ACTIVE && current != FLOW_EXCHANGING {
            return;
        }
        if phase
            .compare_exchange(current, FLOW_CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}
fn cancelled_flow_result(phase: &std::sync::atomic::AtomicU8) -> Result<(), String> {
    match phase.load(std::sync::atomic::Ordering::Acquire) {
        FLOW_CANCELLED => Ok(()),
        FLOW_COMPLETED => Err("authorization completed before cancellation".into()),
        _ => Err("authorization flow ended before cancellation".into()),
    }
}
struct AccountDeviceFlow {
    account: AccountMetadata,
    device_auth_id: lingxi_core::types::Secret<String>,
    user_code: lingxi_core::types::Secret<String>,
    interval: std::time::Duration,
    expires_at: std::time::Instant,
    gate: tokio::sync::Mutex<std::time::Instant>,
    phase: std::sync::atomic::AtomicU8,
}
struct AccountOAuthFlow {
    account: AccountMetadata,
    verifier: lingxi_core::types::Secret<String>,
    state: String,
    redirect_uri: String,
    expires_at: std::time::Instant,
    gate: tokio::sync::Mutex<()>,
    phase: std::sync::atomic::AtomicU8,
}
/// Shared authorization authority. Every flow binds its original account and revision.
pub struct AccountOAuthCoordinator {
    data_dir: PathBuf,
    store_scope: String,
    profiles: Vec<llm_runtime::ProviderProfile>,
    execution_profiles: Option<Vec<llm_runtime::ProviderProfile>>,
    catalog: Option<ProviderAccountCatalog>,
    credentials: std::sync::Weak<CredentialManager>,
    openai: Arc<llm_runtime::auth::openai::AccountOAuthClient>,
    pending_grants:
        Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<AccountCredential>>>>,
    flows: tokio::sync::Mutex<std::collections::HashMap<String, Arc<AccountOAuthFlow>>>,
    device_flows: tokio::sync::Mutex<std::collections::HashMap<String, Arc<AccountDeviceFlow>>>,
    starts: tokio::sync::Mutex<()>,
}
impl AccountOAuthCoordinator {
    fn store(&self) -> Result<ProviderAccountStore, String> {
        let mut store = ProviderAccountStore::new(
            self.data_dir.clone(),
            self.store_scope.clone(),
            self.credentials
                .upgrade()
                .ok_or("account credential owner ended")?,
        )
        .with_profiles(self.profiles.clone());
        store.execution_profiles = self.execution_profiles.clone();
        store.catalog = self.catalog.clone();
        Ok(store.frozen_catalog())
    }

    pub fn new(
        store: Arc<ProviderAccountStore>,
        openai: Arc<llm_runtime::auth::openai::AccountOAuthClient>,
    ) -> Self {
        Self {
            data_dir: store.data_dir.clone(),
            store_scope: store.store_scope.clone(),
            profiles: store.profiles.clone(),
            execution_profiles: store.execution_profiles.clone(),
            catalog: store.catalog.clone(),
            credentials: Arc::downgrade(&store.credentials),
            openai,
            pending_grants: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            flows: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            device_flows: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            starts: tokio::sync::Mutex::new(()),
        }
    }
    pub async fn begin(
        &self,
        account_id: String,
        expected_revision: u64,
        redirect_uri: String,
    ) -> Result<AccountOAuthSession, String> {
        let _start = self.starts.lock().await;
        let snapshot = self.store()?.snapshot().await?;
        if snapshot.config_revision != expected_revision {
            return Err("account configuration conflict".into());
        }
        let account = snapshot
            .accounts
            .into_iter()
            .find(|a| a.account_id == account_id)
            .ok_or("unknown account")?;
        if account.auth_kind != AccountAuthKind::OAuth
            || account.execution_binding_id != "openai-chatgpt"
        {
            return Err("OAuth not enabled for account".into());
        }
        let redirect = url::Url::parse(&redirect_uri).map_err(|_| "invalid OAuth redirect")?;
        let loopback = redirect.scheme() == "http"
            && redirect.host_str() == Some("localhost")
            && matches!(redirect.port(), Some(1455 | 1457))
            && redirect.path() == "/auth/callback";
        if !loopback {
            return Err("invalid OAuth redirect".into());
        }
        let (authorization_url, verifier, state) = self.openai.begin_browser_login(&redirect_uri);
        let flow_id = uuid::Uuid::new_v4().to_string();
        if self.device_flows.lock().await.values().any(|f| {
            f.account.account_id == account_id
                && flow_live(&f.phase)
                && f.expires_at > std::time::Instant::now()
        }) {
            return Err("account already has an authorization flow".into());
        }
        let mut flows = self.flows.lock().await;
        flows.retain(|_, f| f.expires_at > std::time::Instant::now());
        if flows
            .values()
            .any(|f| f.account.account_id == account_id && flow_live(&f.phase))
        {
            return Err("account already has an authorization flow".into());
        }
        flows.insert(
            flow_id.clone(),
            Arc::new(AccountOAuthFlow {
                account,
                verifier: lingxi_core::types::Secret::new(verifier),
                state,
                redirect_uri,
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(600),
                gate: tokio::sync::Mutex::new(()),
                phase: std::sync::atomic::AtomicU8::new(FLOW_ACTIVE),
            }),
        );
        Ok(AccountOAuthSession {
            mode: AccountOAuthMode::Browser,
            user_code: None,
            verification_url: None,
            interval_seconds: 0,
            expires_in_seconds: 600,
            provider: "openai-chatgpt".into(),
            flow_id,
            authorization_url,
            callback_url_scheme: redirect.scheme().into(),
            account_id,
        })
    }
    /// Device authorization uses the provider's registered device callback, never an application scheme.
    pub async fn begin_device(
        &self,
        account_id: String,
        expected_revision: u64,
    ) -> Result<AccountOAuthSession, String> {
        let _start = self.starts.lock().await;
        let snapshot = self.store()?.snapshot().await?;
        if snapshot.config_revision != expected_revision {
            return Err("account configuration conflict".into());
        }
        let account = snapshot
            .accounts
            .into_iter()
            .find(|a| a.account_id == account_id)
            .ok_or("unknown account")?;
        if account.auth_kind != AccountAuthKind::OAuth
            || account.execution_binding_id != "openai-chatgpt"
        {
            return Err("OAuth not enabled for account".into());
        }
        let now = std::time::Instant::now();
        {
            let mut flows = self.flows.lock().await;
            flows.retain(|_, f| f.expires_at > now);
            if flows
                .values()
                .any(|f| f.account.account_id == account_id && flow_live(&f.phase))
            {
                return Err("account already has an authorization flow".into());
            }
        }
        {
            let mut flows = self.device_flows.lock().await;
            flows.retain(|_, f| f.expires_at > now);
            if flows
                .values()
                .any(|f| f.account.account_id == account_id && flow_live(&f.phase))
            {
                return Err("account already has an authorization flow".into());
            }
        }
        let code=tokio::time::timeout(std::time::Duration::from_secs(20),self.openai.request_account_device_code()).await.map_err(|_|"device authorization request timed out")?.map_err(|_|"device code sign-in unavailable; enable device code authorization in ChatGPT security settings or contact your workspace administrator")?;
        self.store()?.verify_identity(&account).await?;
        if code.device_auth_id.is_empty()
            || code.device_auth_id.len() > 16384
            || code.user_code.is_empty()
            || code.user_code.len() > 128
            || code.user_code.chars().any(char::is_control)
        {
            return Err("device authorization response invalid".into());
        }
        let interval = std::time::Duration::from_secs(if code.interval == 0 {
            5
        } else {
            code.interval.clamp(1, 60)
        });
        let flow_id = uuid::Uuid::new_v4().to_string();
        let verification_url = self.openai.device_verification_url();
        self.device_flows.lock().await.insert(
            flow_id.clone(),
            Arc::new(AccountDeviceFlow {
                account,
                device_auth_id: lingxi_core::types::Secret::new(code.device_auth_id),
                user_code: lingxi_core::types::Secret::new(code.user_code.clone()),
                interval,
                expires_at: std::time::Instant::now() + std::time::Duration::from_secs(900),
                gate: tokio::sync::Mutex::new(std::time::Instant::now() + interval),
                phase: std::sync::atomic::AtomicU8::new(FLOW_ACTIVE),
            }),
        );
        Ok(AccountOAuthSession {
            mode: AccountOAuthMode::DeviceCode,
            provider: "openai-chatgpt".into(),
            flow_id,
            authorization_url: verification_url.clone(),
            callback_url_scheme: String::new(),
            account_id,
            user_code: Some(code.user_code),
            verification_url: Some(verification_url),
            interval_seconds: interval.as_secs(),
            expires_in_seconds: 900,
        })
    }
    pub async fn poll_device(
        &self,
        account_id: &str,
        flow_id: &str,
    ) -> Result<AccountOAuthPollResult, String> {
        use std::sync::atomic::Ordering;
        let flow = self
            .device_flows
            .lock()
            .await
            .get(flow_id)
            .cloned()
            .ok_or("device authorization flow missing")?;
        if flow.account.account_id != account_id {
            return Err("OAuth flow account mismatch".into());
        }
        let mut next = flow.gate.lock().await;
        if flow.phase.load(Ordering::Acquire) != FLOW_ACTIVE {
            return Err("device authorization cancelled".into());
        }
        if flow.expires_at <= std::time::Instant::now() {
            self.device_flows.lock().await.remove(flow_id);
            return Err("device authorization expired".into());
        }
        self.store()?.verify_identity(&flow.account).await?;
        let pending = |seconds| AccountOAuthPollResult {
            status: AccountOAuthPollStatus::Pending,
            retry_after_seconds: seconds,
            snapshot: None,
        };
        if *next > std::time::Instant::now() {
            return Ok(pending(
                next.saturating_duration_since(std::time::Instant::now())
                    .as_secs()
                    .saturating_add(1),
            ));
        }
        *next = std::time::Instant::now() + flow.interval;
        let grant = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.openai.poll_account_device_code(
                flow.device_auth_id.expose_secret(),
                flow.user_code.expose_secret(),
            ),
        )
        .await
        .map_err(|_| "device authorization poll timed out")?
        .map_err(|_| "device authorization failed")?;
        if flow.phase.load(Ordering::Acquire) != FLOW_ACTIVE {
            return Err("device authorization cancelled".into());
        }
        let Some(grant) = grant else {
            return Ok(pending(flow.interval.as_secs()));
        };
        flow.phase
            .compare_exchange(
                FLOW_ACTIVE,
                FLOW_COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| "device authorization cancelled")?;
        let result = async {
            self.store()?.verify_identity(&flow.account).await?;
            self.store()?.commit_oauth(&flow.account, grant).await
        }
        .await;
        flow.phase.store(
            if result.is_ok() {
                FLOW_COMPLETED
            } else {
                FLOW_FAILED
            },
            Ordering::Release,
        );
        let snapshot = result?;
        Ok(AccountOAuthPollResult {
            status: AccountOAuthPollStatus::Completed,
            retry_after_seconds: 0,
            snapshot: Some(snapshot),
        })
    }
    pub async fn verify_flow_account(&self, flow_id: &str, account_id: &str) -> Result<(), String> {
        if self
            .flows
            .lock()
            .await
            .get(flow_id)
            .is_some_and(|f| f.account.account_id == account_id)
        {
            Ok(())
        } else {
            Err("OAuth flow account mismatch".into())
        }
    }
    pub async fn cancel(&self, account_id: &str, flow_id: &str) -> Result<(), String> {
        let device = self.device_flows.lock().await.get(flow_id).cloned();
        if let Some(flow) = device {
            if flow.account.account_id != account_id {
                return Err("OAuth account mismatch".into());
            }
            request_flow_cancel(&flow.phase);
            let _gate = flow.gate.lock().await;
            return cancelled_flow_result(&flow.phase);
        }
        let flow = self
            .flows
            .lock()
            .await
            .get(flow_id)
            .cloned()
            .ok_or("OAuth flow missing")?;
        if flow.account.account_id != account_id {
            return Err("OAuth account mismatch".into());
        }
        request_flow_cancel(&flow.phase);
        let _gate = flow.gate.lock().await;
        cancelled_flow_result(&flow.phase)
    }
    pub async fn complete(
        &self,
        flow_id: String,
        callback_url: String,
    ) -> Result<AccountSnapshot, String> {
        use std::sync::atomic::Ordering;
        let callback = url::Url::parse(&callback_url).map_err(|_| "invalid OAuth callback")?;
        let pairs = callback.query_pairs().collect::<Vec<_>>();
        let single = |name: &str| -> Result<String, String> {
            let values = pairs
                .iter()
                .filter(|(key, _)| key == name)
                .collect::<Vec<_>>();
            if values.len() != 1 || values[0].1.is_empty() {
                return Err("invalid OAuth callback".into());
            }
            Ok(values[0].1.to_string())
        };
        let code = lingxi_core::types::Secret::new(single("code")?);
        let state = single("state")?;
        let flow = self
            .flows
            .lock()
            .await
            .get(&flow_id)
            .cloned()
            .ok_or("OAuth flow missing or already consumed")?;
        let _gate = flow.gate.lock().await;
        let redirect = url::Url::parse(&flow.redirect_uri).map_err(|_| "invalid OAuth redirect")?;
        if flow.expires_at <= std::time::Instant::now() {
            return Err("OAuth flow expired".into());
        }
        if flow.state != state
            || callback.scheme() != redirect.scheme()
            || callback.host_str() != redirect.host_str()
            || callback.port() != redirect.port()
            || callback.path() != redirect.path()
        {
            return Err("OAuth callback state or destination mismatch".into());
        }
        flow.phase
            .compare_exchange(
                FLOW_ACTIVE,
                FLOW_EXCHANGING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| "OAuth flow missing or already consumed")?;
        let credential = async {
            self.store()?.verify_identity(&flow.account).await?;
            self.openai
                .exchange_account_code(
                    code.expose_secret(),
                    flow.verifier.expose_secret(),
                    &flow.redirect_uri,
                )
                .await
                .map_err(|_| "OAuth account exchange failed".to_owned())
        }
        .await;
        let credential = match credential {
            Ok(value) => value,
            Err(error) => {
                let _ = flow.phase.compare_exchange(
                    FLOW_EXCHANGING,
                    FLOW_FAILED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
                return Err(error);
            }
        };
        flow.phase
            .compare_exchange(
                FLOW_EXCHANGING,
                FLOW_COMMITTING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| "OAuth authorization cancelled")?;
        let result = self.store()?.commit_oauth(&flow.account, credential).await;
        flow.phase.store(
            if result.is_ok() {
                FLOW_COMPLETED
            } else {
                FLOW_FAILED
            },
            Ordering::Release,
        );
        result
    }
}

/// Read persisted health without acquiring leases or writing the secure store.
fn read_account_health(root: &Path, snapshot: &mut AccountSnapshot) -> Result<(), String> {
    use llm_runtime::accounts::{HealthDocument, HealthStatus};
    let hash = format!("{:x}", Sha256::digest(snapshot.store_scope.as_bytes()));
    let relative = PathBuf::from("provider-accounts/health").join(format!("{hash}.json"));
    let document = match lingxi_core::host::rooted_fs::read_to_string_limited(
        root,
        &relative,
        4 * 1024 * 1024,
    ) {
        Ok(text) => serde_json::from_str::<HealthDocument>(&text)
            .map_err(|_| "account health state invalid")?,
        Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => return Ok(()),
        Err(_) => return Err("account health storage unavailable".into()),
    };
    snapshot.model_entitlements.clear();
    snapshot.scoped_health.clear();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    for account in &snapshot.accounts {
        snapshot
            .model_entitlements
            .extend(document.model_entitlements(account, now_ms));
        for row in document.scoped_records(account) {
            use lingxi_core::host::provider_accounts::{
                AccountHealthScopeKind, AccountScopedHealth,
            };
            use llm_runtime::accounts::FailureScope;
            let (scope_kind, scope_id) = match row.scope {
                FailureScope::Model(id) => (AccountHealthScopeKind::Model, id),
                FailureScope::Endpoint(id) => (AccountHealthScopeKind::Endpoint, id),
                FailureScope::Account => continue,
            };
            snapshot.scoped_health.push(AccountScopedHealth {
                account_id: account.account_id.clone(),
                identity_generation: account.identity_generation,
                scope_kind,
                scope_id,
                unavailable_until_ms: row
                    .recovery_at_ms
                    .map(|recovery| recovery.max(row.probe_until_ms.unwrap_or(0))),
                reason: row.reason,
            });
        }
        let current_revision = match provider_config::read_account_credential_version(root, account)
        {
            Ok(version) => version,
            Err(_) => {
                snapshot
                    .health
                    .retain(|h| h.account_id != account.account_id);
                snapshot.health.push(AccountHealth {
                    account_id: account.account_id.clone(),
                    identity_generation: account.identity_generation,
                    status: AccountHealthStatus::StorageUnavailable,
                    reason: Some("credential_version_unavailable".into()),
                    cooldown_until_ms: None,
                    health_revision: 0,
                });
                continue;
            }
        };
        let record = document
            .account_records(account)
            .into_iter()
            .filter(|row| {
                (row.status != HealthStatus::NeedsAuth && row.reason != "derived_token_rejected")
                    || row.credential_revision == current_revision
            })
            .max_by_key(|row| row.revision);
        if let Some(row) = record {
            snapshot
                .health
                .retain(|h| h.account_id != account.account_id);
            snapshot.health.push(AccountHealth {
                account_id: account.account_id.clone(),
                identity_generation: account.identity_generation,
                status: match row.status {
                    HealthStatus::CoolingDown => AccountHealthStatus::CoolingDown,
                    HealthStatus::NeedsAuth => AccountHealthStatus::NeedsAuth,
                    HealthStatus::QuotaExhausted => AccountHealthStatus::QuotaExhausted,
                    HealthStatus::StorageUnavailable => AccountHealthStatus::StorageUnavailable,
                },
                reason: Some(row.reason.clone()),
                cooldown_until_ms: row
                    .recovery_at_ms
                    .map(|recovery| recovery.max(row.probe_until_ms.unwrap_or(0))),
                health_revision: row.revision,
            });
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl secret::AccountCredentialRefresher for AccountOAuthCoordinator {
    async fn refresh(
        &self,
        account: AccountMetadata,
        rejected_revision: u64,
    ) -> Result<Option<AccountCredential>, secret::CredentialError> {
        use secret::CredentialError;
        let store = self.store().map_err(|_| CredentialError::Unavailable)?;
        if account.auth_kind != AccountAuthKind::OAuth
            || account.execution_binding_id != "openai-chatgpt"
        {
            return Ok(None);
        }
        let root = store.data_dir.clone();
        let id = account.account_id.clone();
        let scope = account.credential_ref.store_scope.clone();
        let mut lock = tokio::task::spawn_blocking(move || {
            lingxi_core::host::rooted_fs::lock_exclusive(
                &root,
                &PathBuf::from("provider-accounts/refresh")
                    .join(scope)
                    .join(format!("{id}.lock")),
                0o700,
                0o600,
            )
            .map_err(|_| CredentialError::Unavailable)
        })
        .await
        .map_err(|_| CredentialError::Unavailable)??;
        store
            .verify_identity(&account)
            .await
            .map_err(|_| CredentialError::Unavailable)?;
        let Some(current) = store
            .credentials
            .get_account_credential_uncached(&account.credential_ref)
            .await?
        else {
            return Ok(None);
        };
        if current.identity_generation != account.identity_generation {
            return Err(CredentialError::Unavailable);
        }
        let has_newer_pending = self
            .pending_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account.account_id)
            .is_some_and(|pending| {
                pending.identity_generation == current.identity_generation
                    && pending.credential_revision > current.credential_revision
            });
        if current.credential_revision > rejected_revision && !has_newer_pending {
            let (_directory_lock, _registry) = store
                .locked_registry_for(Some(&account.account_id))
                .await
                .map_err(|_| CredentialError::Unavailable)?;
            if !_registry.snapshot().accounts.iter().any(|a| {
                a.account_id == account.account_id
                    && a.identity_generation == account.identity_generation
                    && a.credential_ref == account.credential_ref
            }) {
                return Err(CredentialError::Unavailable);
            }
            let root = store.data_dir.clone();
            let version_account = account.clone();
            let revision = current.credential_revision;
            tokio::task::spawn_blocking(move || {
                let _refresh_lock = lock;
                let _directory_lock = _directory_lock;
                provider_config::write_account_credential_version_locked(
                    &root,
                    &version_account,
                    revision,
                )
            })
            .await
            .map_err(|_| CredentialError::Unavailable)?
            .map_err(|_| CredentialError::Unavailable)?;
            self.pending_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|id, pending| {
                    id != &account.account_id
                        || pending.identity_generation != current.identity_generation
                        || pending.credential_revision > current.credential_revision
                });
            return Ok(Some(current));
        }
        let relative = PathBuf::from("provider-accounts/refresh")
            .join(&account.credential_ref.store_scope)
            .join(format!("{}.intent", account.account_id));
        let root = store.data_dir.clone();
        let check_path = relative.clone();
        let intent = tokio::task::spawn_blocking(move || {
            match lingxi_core::host::rooted_fs::read_to_string_limited(&root, &check_path, 4096) {
                Ok(text) => serde_json::from_str::<(String, u64, u64)>(&text)
                    .map(Some)
                    .map_err(|_| CredentialError::Unavailable),
                Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => Ok(None),
                Err(_) => Err(CredentialError::Unavailable),
            }
        })
        .await
        .map_err(|_| CredentialError::Unavailable)??;
        let mut has_intent = intent.is_some();
        if intent.is_some_and(|(_, revision, generation)| {
            current.credential_revision > revision || current.identity_generation != generation
        }) {
            let root = store.data_dir.clone();
            let path = relative.clone();
            lock = tokio::task::spawn_blocking(move || {
                let lock = lock;
                lingxi_core::host::rooted_fs::remove_file(&root, &path)
                    .map_err(|_| CredentialError::Unavailable)?;
                Ok::<_, CredentialError>(lock)
            })
            .await
            .map_err(|_| CredentialError::Unavailable)??;
            has_intent = false;
        }
        let pending = self
            .pending_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&account.account_id)
            .cloned();
        let pending = pending.filter(|pending| {
            if pending.identity_generation < current.identity_generation
                || (pending.identity_generation == current.identity_generation
                    && pending.credential_revision <= current.credential_revision)
            {
                self.pending_grants
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&account.account_id);
                false
            } else {
                true
            }
        });
        let grant = if let Some(pending) = pending {
            if pending.identity_generation != account.identity_generation {
                return Err(CredentialError::Unavailable);
            }
            pending
        } else {
            if has_intent {
                return Err(CredentialError::RefreshFailed(
                    "previous refresh outcome uncertain; reconnect this account".into(),
                ));
            }
            let root = store.data_dir.clone();
            let intent_path = relative.clone();
            let intent = serde_json::to_vec(&(
                uuid::Uuid::new_v4().to_string(),
                current.credential_revision,
                account.identity_generation,
            ))
            .map_err(|_| CredentialError::Unavailable)?;
            lock = tokio::task::spawn_blocking(move || {
                let lock = lock;
                lingxi_core::host::rooted_fs::atomic_write(
                    &root,
                    &intent_path,
                    &intent,
                    lingxi_core::host::AtomicWriteOptions::default(),
                )
                .map_err(|_| CredentialError::Unavailable)?;
                Ok::<_, CredentialError>(lock)
            })
            .await
            .map_err(|_| CredentialError::Unavailable)??;
            Arc::new(
                self.openai
                    .refresh_account_grant(&current)
                    .await
                    .map_err(|_| {
                        CredentialError::RefreshFailed(
                            "account refresh outcome uncertain; reconnect this account".into(),
                        )
                    })?,
            )
        };
        // Preserve a successfully rotated grant before any metadata/storage await.
        self.pending_grants
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(account.account_id.clone(), grant.clone());
        // Directory lock fences removal/replacement while committing the rotated grant.
        let (directory_lock, registry) = store
            .locked_registry_for(Some(&account.account_id))
            .await
            .map_err(|_| CredentialError::Unavailable)?;
        if !registry.snapshot().accounts.iter().any(|a| {
            a.account_id == account.account_id
                && a.identity_generation == account.identity_generation
                && a.credential_ref == account.credential_ref
                && a.auth_kind == account.auth_kind
        }) {
            self.pending_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&account.account_id);
            return Err(CredentialError::Unavailable);
        }
        // A backend may keep a started mutation alive after its waiter is
        // cancelled. The account transaction must retain both cross-process
        // locks through that real completion and all publication afterwards.
        let pending_grants = self.pending_grants.clone();
        tokio::spawn(async move {
            let _refresh_lock = lock;
            store
                .credentials
                .store_account_credential(&account.credential_ref, &grant)
                .await?;
            let version_root = store.data_dir.clone();
            let version_account = account.clone();
            let version_revision = grant.credential_revision;
            let directory_lock = tokio::task::spawn_blocking(move || {
                provider_config::write_account_credential_version_locked(
                    &version_root,
                    &version_account,
                    version_revision,
                )
                .map_err(|_| CredentialError::Unavailable)?;
                Ok::<_, CredentialError>(directory_lock)
            })
            .await
            .map_err(|_| CredentialError::Unavailable)??;
            pending_grants
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&account.account_id);
            let root = store.data_dir.clone();
            tokio::task::spawn_blocking(move || {
                let _directory_lock = directory_lock;
                lingxi_core::host::rooted_fs::remove_file(&root, &relative)
                    .map_err(|_| CredentialError::Unavailable)
            })
            .await
            .map_err(|_| CredentialError::Unavailable)??;
            Arc::try_unwrap(grant)
                .map(Some)
                .map_err(|_| CredentialError::Unavailable)
        })
        .await
        .map_err(|_| CredentialError::Unavailable)?
    }
}

#[cfg(all(test, feature = "desktop"))]
mod tests {
    use super::*;
    use lingxi_core::host::InMemorySecureStorage;
    fn fixture() -> (
        tempfile::TempDir,
        Arc<CredentialManager>,
        Arc<ProviderAccountStore>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Arc::new(CredentialManager::new(
            Arc::new(InMemorySecureStorage::new()),
            Arc::new(platform_posix::PosixClock::new()),
            Arc::new(platform_posix::PosixHttp::new()),
        ));
        let store = Arc::new(ProviderAccountStore::new(
            dir.path().to_path_buf(),
            "test_scope".into(),
            credentials.clone(),
        ));
        (dir, credentials, store)
    }
    #[cfg(unix)]
    #[test]
    fn account_scope_is_stable_before_home_creation_through_symlink_parent() {
        let directory = tempfile::tempdir().unwrap();
        let physical = directory.path().join("physical");
        std::fs::create_dir(&physical).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&physical, &alias).unwrap();
        let missing = alias.join("new-home");
        let before = account_store_scope(&missing, "fixture-native");
        assert!(!missing.exists());
        std::fs::create_dir(&missing).unwrap();
        assert_eq!(before, account_store_scope(&missing, "fixture-native"));
        assert_eq!(
            before,
            account_store_scope(&physical.join("new-home"), "fixture-native")
        );
    }
    fn add(kind: AccountAuthKind, expected: u64) -> ProviderAccountCommand {
        ProviderAccountCommand {
            expected_revision: expected,
            operation_id: None,
            action: ProviderAccountAction::Add {
                provider_id: "openai".into(),
                display_name: "Test".into(),
                execution_binding_id: if kind == AccountAuthKind::OAuth {
                    "openai-chatgpt".into()
                } else {
                    "openai".into()
                },
                auth_kind: kind,
                account_id: None,
            },
        }
    }
    #[tokio::test]
    async fn scoped_health_projection_keeps_active_probe_unavailable_after_cooldown_expiry() {
        use lingxi_core::host::provider_accounts::AccountHealthScopeKind;
        use llm_runtime::accounts::{
            AccountFailure, AccountPoolStore, FailureScope, HealthStatus, PoolOperation, PoolResult,
        };
        let (dir, _credentials, store) = fixture();
        let account = store
            .execute(
                add(AccountAuthKind::ApiKey, 0),
                Some("synthetic-probe-key".into()),
            )
            .await
            .unwrap()
            .accounts[0]
            .clone();
        let model = store
            .snapshot()
            .await
            .unwrap()
            .model_availability
            .into_iter()
            .find(|row| {
                row.execution_binding_id == "openai" && !row.eligible_account_ids.is_empty()
            })
            .unwrap()
            .model_id;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let backend = crate::account_pool_store::FileAccountPoolStore::new(dir.path().to_owned());
        let PoolResult::Claimed { revision } = backend
            .transact(PoolOperation::Claim {
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                now_ms: now,
                owner: "probe-first".into(),
            })
            .await
            .unwrap()
        else {
            panic!("initial claim")
        };
        backend
            .transact(PoolOperation::Finish {
                credential_revision: None,
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                owner: "probe-first".into(),
                claimed_revision: revision,
                outcome: Some(AccountFailure {
                    status: HealthStatus::CoolingDown,
                    scope: FailureScope::Model(model.clone()),
                    reason: "rate_limited".into(),
                    recovery_at_ms: Some(now + 1),
                }),
                now_ms: now,
            })
            .await
            .unwrap();
        let result = backend
            .transact(PoolOperation::Claim {
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                now_ms: now + 2,
                owner: "probe-owner".into(),
            })
            .await
            .unwrap();
        assert!(matches!(result, PoolResult::Claimed { .. }));
        let snapshot = store.snapshot().await.unwrap();
        let health = snapshot
            .scoped_health
            .iter()
            .find(|row| {
                row.account_id == account.account_id
                    && row.scope_kind == AccountHealthScopeKind::Model
                    && row.scope_id == model
            })
            .unwrap();
        assert!(
            health.unavailable_until_ms.unwrap() > now + 2,
            "active probe extends the expired model cooldown"
        );
        assert!(snapshot
            .model_availability
            .iter()
            .find(|row| row.execution_binding_id == "openai" && row.model_id == model)
            .unwrap()
            .eligible_account_ids
            .is_empty());
    }
    #[tokio::test]
    async fn fresh_model_permission_projection_changes_without_config_revision_change() {
        use lingxi_core::host::provider_accounts::AccountModelEntitlementStatus;
        use llm_runtime::accounts::{
            AccountFailure, AccountPoolStore, FailureScope, HealthStatus, PoolOperation, PoolResult,
        };
        let (dir, _credentials, store) = fixture();
        let added = store
            .execute(
                add(AccountAuthKind::ApiKey, 0),
                Some("synthetic-permission-key".into()),
            )
            .await
            .unwrap();
        let account = added.accounts[0].clone();
        let before = store.snapshot().await.unwrap();
        let model = before
            .model_availability
            .iter()
            .find(|row| {
                row.execution_binding_id == "openai"
                    && row.eligible_account_ids.contains(&account.account_id)
            })
            .unwrap()
            .model_id
            .clone();
        let config_revision = before.config_revision;
        let backend = crate::account_pool_store::FileAccountPoolStore::new(dir.path().to_owned());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let owner = "permission-projection".to_string();
        let PoolResult::Claimed { revision } = backend
            .transact(PoolOperation::Claim {
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                now_ms: now,
                owner: owner.clone(),
            })
            .await
            .unwrap()
        else {
            panic!("initial account claim must succeed")
        };
        backend
            .transact(PoolOperation::Finish {
                credential_revision: None,
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                owner: owner.clone(),
                claimed_revision: revision,
                outcome: Some(AccountFailure {
                    status: HealthStatus::CoolingDown,
                    scope: FailureScope::Model(model.clone()),
                    reason: "model_permission_denied".into(),
                    recovery_at_ms: Some(now + 300_000),
                }),
                now_ms: now,
            })
            .await
            .unwrap();
        let denied = store.snapshot().await.unwrap();
        assert_eq!(denied.config_revision, config_revision);
        let denied_row = denied
            .model_availability
            .iter()
            .find(|row| row.execution_binding_id == "openai" && row.model_id == model)
            .unwrap();
        assert!(!denied_row
            .eligible_account_ids
            .contains(&account.account_id));
        assert!(denied_row
            .forbidden_account_ids
            .contains(&account.account_id));
        assert!(denied
            .model_entitlements
            .iter()
            .any(|row| row.account_id == account.account_id
                && row.model_id == model
                && row.status == AccountModelEntitlementStatus::Forbidden));
        let PoolResult::Claimed { revision } = backend
            .transact(PoolOperation::Claim {
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                now_ms: now + 300_001,
                owner: owner.clone(),
            })
            .await
            .unwrap()
        else {
            panic!("expired denial admits one exact recovery probe")
        };
        backend
            .transact(PoolOperation::Finish {
                credential_revision: None,
                account: account.clone(),
                model: model.clone(),
                endpoint: "fixture".into(),
                owner,
                claimed_revision: revision,
                outcome: None,
                now_ms: now + 300_001,
            })
            .await
            .unwrap();
        let recovered = store.snapshot().await.unwrap();
        assert_eq!(recovered.config_revision, config_revision);
        assert!(recovered
            .model_availability
            .iter()
            .any(|row| row.execution_binding_id == "openai"
                && row.model_id == model
                && row.confirmed_account_ids.contains(&account.account_id)));
    }
    #[tokio::test]
    async fn removed_oauth_draft_identifier_cannot_reanimate_an_old_principal() {
        let (_directory, _credentials, store) = fixture();
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        let removed = store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: 1,
                operation_id: None,
                action: ProviderAccountAction::Remove {
                    account_id: account.account_id.clone(),
                },
            })
            .await
            .unwrap();
        let replacement = ProviderAccountCommand {
            expected_revision: removed.config_revision,
            operation_id: None,
            action: ProviderAccountAction::Add {
                provider_id: account.provider_id.clone(),
                display_name: "new principal".into(),
                execution_binding_id: account.execution_binding_id.clone(),
                auth_kind: AccountAuthKind::OAuth,
                account_id: Some(account.account_id.clone()),
            },
        };
        assert!(store
            .execute_metadata(replacement)
            .await
            .unwrap_err()
            .contains("retired"));
        assert!(store.verify_identity(&account).await.is_err());
        assert!(store.snapshot_metadata().await.unwrap().accounts.is_empty());
    }

    #[tokio::test]
    async fn logical_openai_enable_projects_oauth_accounts_without_enabling_unrelated_bindings() {
        let (directory, credentials, store) = fixture();
        let added = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap();
        let committed = store
            .commit_oauth(&added.accounts[0], grant("oauth-only", "workspace"))
            .await
            .unwrap();
        let id = committed.accounts[0].account_id.clone();
        let all = llm_runtime::builtin_presets().providers;
        let enabled = std::collections::BTreeSet::from(["openai".to_string()]);
        let profiles: Vec<_> = all
            .iter()
            .filter(|profile| provider_config::mobile_profile_allowed(profile, &enabled))
            .cloned()
            .collect();
        assert!(profiles
            .iter()
            .any(|profile| profile.profile_name == "openai-chatgpt"));
        let view =
            ProviderAccountStore::new(directory.path().into(), "test_scope".into(), credentials)
                .with_profiles(all.clone())
                .with_execution_profiles(profiles)
                .snapshot()
                .await
                .unwrap();
        assert!(view
            .model_availability
            .iter()
            .any(|row| row.execution_binding_id == "openai-chatgpt"
                && row.eligible_account_ids.contains(&id)
                && row.unverified_account_ids.contains(&id)
                && row.confirmed_account_ids.is_empty()));
        let subscription_only = std::collections::BTreeSet::from(["openai-chatgpt".to_string()]);
        assert!(!all
            .iter()
            .filter(|p| provider_config::mobile_profile_allowed(p, &subscription_only))
            .any(|p| p.profile_name == "openai"));
        let mut unrelated = all
            .iter()
            .find(|p| p.profile_name == "openai")
            .unwrap()
            .clone();
        unrelated.profile_name = "custom-proxy".into();
        unrelated.base_url = "https://proxy.invalid/v1".into();
        assert!(!provider_config::mobile_profile_allowed(
            &unrelated, &enabled
        ));
        let mut wrong_endpoint = all
            .iter()
            .find(|p| p.profile_name == "openai-chatgpt")
            .unwrap()
            .clone();
        wrong_endpoint.base_url = "https://proxy.invalid/v1".into();
        assert!(!provider_config::mobile_profile_allowed(
            &wrong_endpoint,
            &enabled
        ));
    }

    #[tokio::test]
    async fn published_models_refresh_account_availability_without_granting_entitlements() {
        use lingxi_core::host::provider_accounts::{
            AccountModelEntitlement, AccountModelEntitlementStatus,
        };
        use provider_config::startup_catalog::{
            CatalogPlatform, CatalogTrigger, StartupCatalog, PUBLIC_ENDPOINT, RUNTIME_CAPABILITIES,
        };
        let (directory, credentials, _) = fixture();
        let source = StartupCatalog::new(CatalogPlatform::Desktop, "0.1.0", directory.path());
        let inputs = provider_config::AssembleInputs {
            official_catalog: None,
            anthropic_api_base: "https://api.anthropic.com".into(),
            anthropic_models: Vec::new(),
            anthropic_has_api_key: false,
            anthropic_has_oauth: false,
            user_providers: Default::default(),
            routing: None,
        };
        let store =
            ProviderAccountStore::new(directory.path().into(), "test_scope".into(), credentials)
                .with_catalog(ProviderAccountCatalog::new(
                    inputs,
                    llm_runtime::Region::International,
                    Some(source.clone()),
                    Some(std::collections::BTreeSet::from(["deepseek".into()])),
                ));
        let before = store
            .execute(
                ProviderAccountCommand {
                    expected_revision: 0,
                    operation_id: None,
                    action: ProviderAccountAction::Add {
                        provider_id: "deepseek".into(),
                        display_name: "fixture".into(),
                        execution_binding_id: "deepseek".into(),
                        auth_kind: AccountAuthKind::ApiKey,
                        account_id: None,
                    },
                },
                Some("synthetic-key".into()),
            )
            .await
            .unwrap();
        let account = before.accounts[0].clone();
        let model = "deepseek-published-account-test";
        assert!(!before
            .model_availability
            .iter()
            .any(|row| row.model_id == model));
        // Publish through the same validated sibling-process cache path. No HTTP is needed.
        let mut document: serde_json::Value = serde_json::from_str(include_str!(
            "../../provider-config/tests/fixtures/startup-catalog.json"
        ))
        .unwrap();
        let definition = &mut document["providers"][0]["executionDefinition"];
        let updated = format!("{}\n[[model]]\nid = \"{model}\"\nname = \"New model\"\ncontext_window = 1000000\nmax_output = 8192\ntool_call = true\n", definition["source"].as_str().unwrap());
        definition["source"] = serde_json::json!(updated);
        definition["sha256"] =
            serde_json::json!(format!("{:x}", Sha256::digest(updated.as_bytes())));
        let mut published_model = document["models"][0].clone();
        published_model["id"] = serde_json::json!(model);
        document["models"]
            .as_array_mut()
            .unwrap()
            .push(published_model);
        let body = document.to_string();
        let key = format!(
            "{}\0desktop\0{}\02\0{}",
            PUBLIC_ENDPOINT,
            "0.1.0",
            RUNTIME_CAPABILITIES.join(",")
        );
        let cache = directory.path().join("cache/provider-catalog-v2");
        std::fs::create_dir_all(&cache).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let cached = serde_json::json!({
            "endpoint": PUBLIC_ENDPOINT, "platform":"desktop", "appVersion":"0.1.0",
            "etag":null, "body":body,
            "bodySha256":format!("{:x}", Sha256::digest(body.as_bytes())), "checkedAt":now,
        });
        std::fs::write(
            cache.join(format!("{:x}.json", Sha256::digest(key.as_bytes()))),
            cached.to_string(),
        )
        .unwrap();
        assert!(source.trigger(CatalogTrigger::Settings).await.unwrap());
        let after = store.snapshot().await.unwrap();
        let row = after
            .model_availability
            .iter()
            .find(|row| row.model_id == model)
            .unwrap();
        assert_eq!(row.eligible_account_ids, vec![account.account_id.clone()]);
        assert_eq!(row.unverified_account_ids, vec![account.account_id.clone()]);
        assert!(row.confirmed_account_ids.is_empty());
        assert_eq!(before.config_revision, after.config_revision);
        assert!(before.model_entitlements.is_empty() && after.model_entitlements.is_empty());
        // A model-directory publication cannot clear an account's actual model denial.
        let mut denied = after;
        denied.model_entitlements.push(AccountModelEntitlement {
            account_id: account.account_id.clone(),
            identity_generation: account.identity_generation,
            model_id: model.into(),
            status: AccountModelEntitlementStatus::Forbidden,
            observed_at_ms: now,
            retry_at_ms: Some(now + 300_000),
            reason: Some("model_forbidden".into()),
        });
        let projected = store
            .project_model_availability_with_presence(
                denied,
                std::collections::BTreeSet::from([account.account_id.clone()]),
            )
            .unwrap();
        let row = projected
            .model_availability
            .iter()
            .find(|row| row.model_id == model)
            .unwrap();
        assert!(row.eligible_account_ids.is_empty());
        assert_eq!(row.forbidden_account_ids, vec![account.account_id.clone()]);
        assert!(projected
            .model_availability
            .iter()
            .any(|row| row.model_id != model
                && row.eligible_account_ids.contains(&account.account_id)));
    }

    #[tokio::test]
    async fn management_binding_does_not_enable_disabled_execution_models() {
        let (dir, credentials, store) = fixture();
        let snapshot = store
            .execute(
                add(AccountAuthKind::ApiKey, 0),
                Some("synthetic-model-key".into()),
            )
            .await
            .unwrap();
        let account_id = snapshot.accounts[0].account_id.clone();
        let disabled = ProviderAccountStore::new(
            dir.path().to_owned(),
            "test_scope".into(),
            credentials.clone(),
        )
        .with_profiles(llm_runtime::builtin_presets().providers)
        .with_execution_profiles(Vec::new());
        let unavailable = disabled.snapshot().await.unwrap();
        assert!(unavailable
            .model_availability
            .iter()
            .all(|row| row.eligible_account_ids.is_empty()));
        let enabled = store.snapshot().await.unwrap();
        assert!(enabled
            .model_availability
            .iter()
            .any(|row| row.eligible_account_ids.contains(&account_id)));
        let absent = store
            .project_model_availability_with_presence(enabled, std::collections::BTreeSet::new())
            .unwrap();
        assert!(absent
            .model_availability
            .iter()
            .all(|row| row.eligible_account_ids.is_empty()));
    }

    #[tokio::test]
    async fn snapshot_is_read_only_and_revision_races_have_one_winner() {
        let (dir, _credentials, store) = fixture();
        assert!(store.snapshot().await.unwrap().accounts.is_empty());
        assert!(!dir.path().join("provider-accounts").exists());
        let (a, b) = tokio::join!(
            store.execute_metadata(add(AccountAuthKind::ApiKey, 0)),
            store.execute_metadata(add(AccountAuthKind::ApiKey, 0))
        );
        assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
        assert_eq!(store.snapshot_metadata().await.unwrap().accounts.len(), 1);
    }
    #[tokio::test]
    async fn removing_one_account_preserves_other_slot_and_priority() {
        let (_dir, credentials, store) = fixture();
        let a = store
            .execute(add(AccountAuthKind::ApiKey, 0), Some("key-a".into()))
            .await
            .unwrap();
        let b = store
            .execute(add(AccountAuthKind::ApiKey, 1), Some("key-b".into()))
            .await
            .unwrap();
        let removed = a.accounts[0].clone();
        let survivor = b.accounts[1].clone();
        store
            .execute(
                ProviderAccountCommand {
                    expected_revision: 2,
                    operation_id: None,
                    action: ProviderAccountAction::Remove {
                        account_id: removed.account_id,
                    },
                },
                None,
            )
            .await
            .unwrap();
        assert!(credentials
            .get_account_credential(&removed.credential_ref)
            .await
            .unwrap()
            .is_none());
        assert!(credentials
            .get_account_credential(&survivor.credential_ref)
            .await
            .unwrap()
            .is_some());
        assert_eq!(
            store.snapshot().await.unwrap().accounts[0].user_priority,
            survivor.user_priority
        );
    }
    fn coordinator(store: Arc<ProviderAccountStore>) -> Arc<AccountOAuthCoordinator> {
        let handle = Arc::new(llm_runtime::auth::openai::AccountOAuthClient::new(
            Default::default(),
            Arc::new(platform_common::provider_transport().unwrap()),
        ));
        Arc::new(AccountOAuthCoordinator::new(store, handle))
    }
    struct DeviceFixtureHttp {
        ready: std::sync::atomic::AtomicBool,
        requests: std::sync::Mutex<Vec<lingxi_llm_client::HttpRequest>>,
        block_token: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl llm_runtime::Transport for DeviceFixtureHttp {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            use base64::Engine as _;
            if request.url.ends_with("/oauth/token")
                && self
                    .block_token
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            let (status, body) = if request.url.ends_with("/deviceauth/usercode") {
                (
                    200,
                    serde_json::json!({"device_auth_id":"synthetic-device-id","user_code":"ABCD-EFGH","interval":5}),
                )
            } else if request.url.ends_with("/deviceauth/token") {
                if self.ready.load(std::sync::atomic::Ordering::SeqCst) {
                    (
                        200,
                        serde_json::json!({"authorization_code":"synthetic-code","code_verifier":"synthetic-verifier"}),
                    )
                } else {
                    (403, serde_json::json!({}))
                }
            } else {
                assert!(request.url.ends_with("/oauth/token"));
                let redirect = url::form_urlencoded::parse(&request.body)
                    .find(|(name, _)| name == "redirect_uri")
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_else(|| "https://auth.openai.com/deviceauth/callback".into());
                assert!(matches!(
                    redirect.as_str(),
                    "https://auth.openai.com/deviceauth/callback"
                        | "http://localhost:1455/auth/callback"
                ));
                let config = lingxi_llm_client::auth::oauth::openai::OpenAiOAuthConfig::default();
                let exp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600;
                let claims = serde_json::json!({"iss":config.issuer,"aud":config.client_id,"exp":exp,"sub":"device-principal","https://api.openai.com/auth":{"chatgpt_account_id":"device-workspace"}});
                let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(serde_json::to_vec(&claims).unwrap());
                (
                    200,
                    serde_json::json!({"access_token":"synthetic-device-access","refresh_token":"synthetic-device-refresh","expires_in":3600,"id_token":format!("e30.{payload}.synthetic-signature")}),
                )
            };
            self.requests.lock().unwrap().push(request);
            let body = bytes::Bytes::from(serde_json::to_vec(&body).unwrap());
            Ok(lingxi_llm_client::StreamResponse {
                status,
                headers: vec![],
                body: Box::pin(futures::stream::once(async move { Ok(body) })),
            })
        }
    }
    fn device_coordinator(
        store: Arc<ProviderAccountStore>,
    ) -> (Arc<AccountOAuthCoordinator>, Arc<DeviceFixtureHttp>) {
        let http = Arc::new(DeviceFixtureHttp {
            ready: std::sync::atomic::AtomicBool::new(false),
            requests: Default::default(),
            block_token: std::sync::atomic::AtomicBool::new(false),
            entered: Default::default(),
            release: Default::default(),
        });
        let handle = Arc::new(llm_runtime::auth::openai::AccountOAuthClient::new(
            Default::default(),
            http.clone(),
        ));
        (Arc::new(AccountOAuthCoordinator::new(store, handle)), http)
    }
    #[tokio::test]
    async fn device_flow_enforces_interval_account_cancel_and_removal_before_network() {
        let (_dir, credentials, store) = fixture();
        let snapshot = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap();
        let account = snapshot.accounts[0].clone();
        let (authority, http) = device_coordinator(store.clone());
        let session = authority
            .begin_device(account.account_id.clone(), 1)
            .await
            .unwrap();
        assert!(matches!(session.mode, AccountOAuthMode::DeviceCode));
        assert!(session.callback_url_scheme.is_empty());
        assert_eq!(session.user_code.as_deref(), Some("ABCD-EFGH"));
        assert!(authority
            .poll_device("different-account", &session.flow_id)
            .await
            .is_err());
        assert!(matches!(
            authority
                .poll_device(&account.account_id, &session.flow_id)
                .await
                .unwrap()
                .status,
            AccountOAuthPollStatus::Pending
        ));
        assert_eq!(
            http.requests.lock().unwrap().len(),
            1,
            "early poll must not issue a request"
        );
        let flow = authority
            .device_flows
            .lock()
            .await
            .get(&session.flow_id)
            .unwrap()
            .clone();
        *flow.gate.lock().await = std::time::Instant::now();
        assert!(matches!(
            authority
                .poll_device(&account.account_id, &session.flow_id)
                .await
                .unwrap()
                .status,
            AccountOAuthPollStatus::Pending
        ));
        assert_eq!(http.requests.lock().unwrap().len(), 2);
        authority
            .cancel(&account.account_id, &session.flow_id)
            .await
            .unwrap();
        assert!(authority
            .poll_device(&account.account_id, &session.flow_id)
            .await
            .is_err());
        assert!(!credentials
            .has_account_credential(&account.credential_ref)
            .await
            .unwrap());
        let second = authority
            .begin_device(account.account_id.clone(), 1)
            .await
            .unwrap();
        store
            .execute(
                ProviderAccountCommand {
                    expected_revision: 1,
                    operation_id: None,
                    action: ProviderAccountAction::Remove {
                        account_id: account.account_id.clone(),
                    },
                },
                None,
            )
            .await
            .unwrap();
        assert!(authority
            .poll_device(&account.account_id, &second.flow_id)
            .await
            .is_err());
        assert_eq!(
            http.requests.lock().unwrap().len(),
            3,
            "removed account must be fenced before poll/exchange"
        );
    }
    #[tokio::test]
    async fn device_flow_commits_exact_account_with_validated_principal_and_workspace() {
        let (_dir, credentials, store) = fixture();
        let snapshot = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap();
        let account = snapshot.accounts[0].clone();
        let (authority, http) = device_coordinator(store.clone());
        let session = authority
            .begin_device(account.account_id.clone(), 1)
            .await
            .unwrap();
        http.ready.store(true, std::sync::atomic::Ordering::SeqCst);
        let flow = authority
            .device_flows
            .lock()
            .await
            .get(&session.flow_id)
            .unwrap()
            .clone();
        *flow.gate.lock().await = std::time::Instant::now();
        let result = authority
            .poll_device(&account.account_id, &session.flow_id)
            .await
            .unwrap();
        assert!(matches!(result.status, AccountOAuthPollStatus::Completed));
        assert_eq!(result.snapshot.unwrap().accounts[0].identity_generation, 2);
        assert!(authority
            .poll_device(&account.account_id, &session.flow_id)
            .await
            .is_err());
        let stored = credentials
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .unwrap();
        let AccountCredentialKind::OAuth {
            subject,
            organization_id,
            ..
        } = stored.kind
        else {
            panic!("expected exact OAuth account grant")
        };
        assert_eq!(subject.as_deref(), Some("device-principal"));
        assert_eq!(organization_id.as_deref(), Some("device-workspace"));
    }
    async fn entered(signal: &tokio::sync::Notify) {
        tokio::time::timeout(std::time::Duration::from_secs(5), signal.notified())
            .await
            .expect("fixture operation must reach its bounded gate");
    }
    fn initial_device_grant() -> AccountCredential {
        AccountCredential {
            credential_revision: 1,
            identity_generation: 1,
            kind: AccountCredentialKind::OAuth {
                access_token: lingxi_core::types::Secret::new("synthetic-old-access".into()),
                refresh_token: Some(lingxi_core::types::Secret::new(
                    "synthetic-old-refresh".into(),
                )),
                expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                scopes: vec![],
                issuer: "https://auth.openai.com".into(),
                subject: Some("device-principal".into()),
                organization_id: Some("device-workspace".into()),
                email: None,
                fedramp: false,
            },
        }
    }

    #[tokio::test]
    async fn cancelled_refresh_ledger_writers_hold_the_directory_lock_through_publication() {
        use secret::AccountCredentialRefresher;
        for repair_only in [false, true] {
            let (directory, credentials, store) = fixture();
            let account = store
                .execute_metadata(add(AccountAuthKind::OAuth, 0))
                .await
                .unwrap()
                .accounts[0]
                .clone();
            let mut grant = initial_device_grant();
            if repair_only {
                grant.credential_revision = 2;
            }
            credentials
                .store_account_credential(&account.credential_ref, &grant)
                .await
                .unwrap();
            publish_account_credential_version(
                directory.path().to_owned(),
                "test_scope".into(),
                AccountCredentialVersion {
                    account_id: account.account_id.clone(),
                    identity_generation: 1,
                    credential_revision: 1,
                },
            )
            .await
            .unwrap();
            let pause = provider_config::account_versions::pause_next_account_version_write(
                directory.path(),
            );
            let (authority, http) = device_coordinator(store.clone());
            let refreshing = {
                let authority = authority.clone();
                let account = account.clone();
                tokio::spawn(async move { authority.refresh(account, 1).await })
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), pause.entered())
                .await
                .unwrap();
            refreshing.abort();
            assert!(refreshing.await.unwrap_err().is_cancelled());
            assert!(
                matches!(
                    lingxi_core::host::rooted_fs::try_lock_exclusive(
                        directory.path(),
                        Path::new("provider-accounts/test_scope.lock"),
                        0o700,
                        0o600
                    ),
                    Err(lingxi_core::host::FsError::AlreadyExists(_))
                ),
                "the cancelled waiter must leave the directory lock with its blocking writer"
            );

            let mut reconnecting = Box::pin(store.commit_oauth(&account, initial_device_grant()));
            std::future::poll_fn(|cx| {
                assert!(std::future::Future::poll(reconnecting.as_mut(), cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            // This guard also releases on assertion failure, so no blocking
            // worker is stranded if the regression fails before this point.
            pause.release();
            let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), reconnecting)
                .await
                .unwrap()
                .unwrap();
            let current = &snapshot.accounts[0];
            assert_eq!(current.identity_generation, 2);
            assert_eq!(
                provider_config::read_account_credential_version(directory.path(), current)
                    .unwrap(),
                Some(3)
            );
            assert_eq!(
                http.requests.lock().unwrap().len(),
                usize::from(!repair_only)
            );
            assert_eq!(
                credentials
                    .get_account_credential(&account.credential_ref)
                    .await
                    .unwrap()
                    .unwrap()
                    .identity_generation,
                2
            );
        }
    }

    #[cfg(unix)]
    fn cached_broker_pair(root: &Path) -> [Arc<dyn lingxi_core::host::SecureStorage>; 2] {
        use std::os::unix::fs::PermissionsExt;
        let script = root.join("synthetic-credential-broker");
        let program = r#"#!/usr/bin/env python3
import json, pathlib, sys, time
root = pathlib.Path(ROOT)
request = json.load(sys.stdin)
state = root / request.get('account', 'unused')
reply = {'ok': True, 'protocol_version': 1}
pause = root / ('pause-next-' + request['op'])
if request['op'] in ('store', 'delete') and pause.exists():
    pause.rename(root / 'mutation-entered')
    while not (root / 'release-mutation').exists(): time.sleep(0.01)
if request['op'] == 'store': state.write_text(request['payload'])
elif request['op'] == 'delete': state.unlink(missing_ok=True)
elif request['op'] == 'retrieve' and state.exists(): reply['payload'] = state.read_text()
elif request['op'] == 'contains': reply['present'] = state.exists()
json.dump(reply, sys.stdout)
"#
        .replace(
            "ROOT",
            &serde_json::to_string(root.to_str().unwrap()).unwrap(),
        );
        std::fs::write(&script, program).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::array::from_fn(|_| {
            Arc::new(
                platform_posix::secure_storage::MacOsKeychainStorage::with_broker_for_test(
                    root.to_owned(),
                    root.to_owned(),
                    script.clone(),
                ),
            ) as Arc<dyn lingxi_core::host::SecureStorage>
        })
    }

    #[cfg(unix)]
    struct BrokerMutationPause(PathBuf);

    #[cfg(unix)]
    impl BrokerMutationPause {
        fn arm(root: &Path, operation: &str) -> Self {
            std::fs::write(root.join(format!("pause-next-{operation}")), b"").unwrap();
            Self(root.to_owned())
        }

        async fn entered(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !self.0.join("mutation-entered").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("synthetic broker mutation did not start");
        }

        fn release(&self) {
            std::fs::write(self.0.join("release-mutation"), b"").unwrap();
        }
    }

    #[cfg(unix)]
    impl Drop for BrokerMutationPause {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0.join("release-mutation"), b"");
        }
    }

    #[cfg(unix)]
    fn independent_broker_stores(
        root: &Path,
    ) -> [(Arc<CredentialManager>, Arc<ProviderAccountStore>); 2] {
        cached_broker_pair(root).map(|backend| {
            let credentials = Arc::new(CredentialManager::new(
                backend,
                Arc::new(platform_posix::PosixClock::new()),
                Arc::new(platform_posix::PosixHttp::new()),
            ));
            let store = Arc::new(ProviderAccountStore::new(
                root.to_owned(),
                "test_scope".into(),
                credentials.clone(),
            ));
            (credentials, store)
        })
    }

    #[cfg(unix)]
    fn assert_account_lock_held(root: &Path, relative: &Path) {
        assert!(matches!(
            lingxi_core::host::rooted_fs::try_lock_exclusive(root, relative, 0o700, 0o600),
            Err(lingxi_core::host::FsError::AlreadyExists(_))
        ));
    }

    #[cfg(unix)]
    async fn check_cancelled_account_mutation(delete: bool) {
        let directory = tempfile::tempdir().unwrap();
        let [(_, store_a), (credentials_b, store_b)] = independent_broker_stores(directory.path());
        let initial = store_a
            .execute(add(AccountAuthKind::ApiKey, 0), Some("synthetic-K1".into()))
            .await
            .unwrap();
        let account = initial.accounts[0].clone();
        let pause =
            BrokerMutationPause::arm(directory.path(), if delete { "delete" } else { "store" });
        let first = tokio::spawn({
            let account_id = account.account_id.clone();
            async move {
                store_a
                    .execute(
                        ProviderAccountCommand {
                            expected_revision: initial.config_revision,
                            operation_id: None,
                            action: if delete {
                                ProviderAccountAction::ClearCredential { account_id }
                            } else {
                                ProviderAccountAction::SetCredential { account_id }
                            },
                        },
                        (!delete).then(|| "synthetic-K2".into()),
                    )
                    .await
            }
        });
        pause.entered().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert_account_lock_held(
            directory.path(),
            Path::new("provider-accounts/test_scope.lock"),
        );
        let mut replacement = Box::pin(store_b.execute(
            ProviderAccountCommand {
                expected_revision: initial.config_revision + 1,
                operation_id: None,
                action: ProviderAccountAction::SetCredential {
                    account_id: account.account_id.clone(),
                },
            },
            Some("synthetic-K3".into()),
        ));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(replacement.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        pause.release();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), replacement)
            .await
            .unwrap()
            .unwrap();
        let live = &snapshot.accounts[0];
        let record = credentials_b
            .get_account_credential_uncached(&account.credential_ref)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(live.identity_generation, 3);
        assert_eq!(record.identity_generation, live.identity_generation);
        assert_eq!(
            provider_config::read_account_credential_version(directory.path(), live).unwrap(),
            Some(record.credential_revision)
        );
        let AccountCredentialKind::ApiKey { key } = record.kind else {
            panic!("replacement must retain the API key")
        };
        assert_eq!(key.expose_secret(), "synthetic-K3");
        assert!(!directory
            .path()
            .join("provider-accounts/test_scope.pending")
            .exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_account_store_keeps_the_lock_until_a_later_storage_can_replace_it() {
        check_cancelled_account_mutation(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_account_delete_cannot_erase_a_later_storage_replacement() {
        check_cancelled_account_mutation(true).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_refresh_secure_store_keeps_both_locks_until_reconnect_can_commit() {
        use secret::AccountCredentialRefresher;
        let directory = tempfile::tempdir().unwrap();
        let [(credentials_a, store_a), (credentials_b, store_b)] =
            independent_broker_stores(directory.path());
        let account = store_a
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        credentials_a
            .store_account_credential(&account.credential_ref, &initial_device_grant())
            .await
            .unwrap();
        publish_account_credential_version(
            directory.path().to_owned(),
            "test_scope".into(),
            AccountCredentialVersion {
                account_id: account.account_id.clone(),
                identity_generation: 1,
                credential_revision: 1,
            },
        )
        .await
        .unwrap();
        let authority = Arc::new(AccountOAuthCoordinator::new(
            store_a,
            Arc::new(llm_runtime::auth::openai::AccountOAuthClient::new(
                Default::default(),
                Arc::new(SingleUseRefreshHttp::default()),
            )),
        ));
        let pause = BrokerMutationPause::arm(directory.path(), "store");
        let refreshing = tokio::spawn({
            let authority = authority.clone();
            let account = account.clone();
            async move { authority.refresh(account, 1).await }
        });
        pause.entered().await;
        refreshing.abort();
        assert!(refreshing.await.unwrap_err().is_cancelled());
        assert_account_lock_held(
            directory.path(),
            Path::new("provider-accounts/test_scope.lock"),
        );
        assert_account_lock_held(
            directory.path(),
            &PathBuf::from("provider-accounts/refresh/test_scope")
                .join(format!("{}.lock", account.account_id)),
        );
        let mut reconnect = Box::pin(store_b.commit_oauth(&account, initial_device_grant()));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(reconnect.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        pause.release();
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), reconnect)
            .await
            .unwrap()
            .unwrap();
        let record = credentials_b
            .get_account_credential_uncached(&account.credential_ref)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.identity_generation, 2);
        assert_eq!(record.credential_revision, 3);
        assert_eq!(
            provider_config::read_account_credential_version(
                directory.path(),
                &snapshot.accounts[0]
            )
            .unwrap(),
            Some(3)
        );
        assert!(authority.pending_grants.lock().unwrap().is_empty());
    }

    #[derive(Default)]
    struct SingleUseRefreshHttp(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl llm_runtime::Transport for SingleUseRefreshHttp {
        async fn send(
            &self,
            request: lingxi_llm_client::HttpRequest,
        ) -> Result<lingxi_llm_client::StreamResponse, lingxi_llm_client::protocol::LlmError>
        {
            assert!(request.url.ends_with("/oauth/token"));
            let attempt = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if attempt != 0 {
                return Err(lingxi_llm_client::protocol::LlmError::Authentication {
                    message: "synthetic refresh token was already consumed".into(),
                });
            }
            Ok(lingxi_llm_client::HttpResponse {
                status: 200,
                headers: vec![],
                body: serde_json::to_vec(&serde_json::json!({
                    "access_token":"synthetic-rotated-access",
                    "refresh_token":"synthetic-rotated-refresh",
                    "expires_in":3600,
                }))
                .unwrap()
                .into(),
            }
            .into())
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn independent_keychain_caches_adopt_the_rotated_grant_under_the_refresh_file_lock() {
        use secret::AccountCredentialRefresher;
        let directory = tempfile::tempdir().unwrap();
        let [backend_a, backend_b] = cached_broker_pair(directory.path());
        let manager = |backend| {
            Arc::new(CredentialManager::new(
                backend,
                Arc::new(platform_posix::PosixClock::new()),
                Arc::new(platform_posix::PosixHttp::new()),
            ))
        };
        let a = manager(backend_a);
        let b = manager(backend_b);
        let store = |credentials| {
            Arc::new(ProviderAccountStore::new(
                directory.path().to_owned(),
                "test_scope".into(),
                credentials,
            ))
        };
        let store_a = store(a.clone());
        let store_b = store(b.clone());
        let account = store_a
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        a.store_account_credential(&account.credential_ref, &initial_device_grant())
            .await
            .unwrap();
        publish_account_credential_version(
            directory.path().to_owned(),
            "test_scope".into(),
            AccountCredentialVersion {
                account_id: account.account_id.clone(),
                identity_generation: 1,
                credential_revision: 1,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            b.get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            1
        );
        let http = Arc::new(SingleUseRefreshHttp::default());
        let handle = Arc::new(llm_runtime::auth::openai::AccountOAuthClient::new(
            Default::default(),
            http.clone(),
        ));
        let authority_a = AccountOAuthCoordinator::new(store_a, handle.clone());
        let authority_b = AccountOAuthCoordinator::new(store_b, handle);
        assert_eq!(
            authority_a
                .refresh(account.clone(), 1)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert_eq!(
            b.get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            1,
            "the second process still has a hot r1 cache"
        );
        assert_eq!(
            authority_b
                .refresh(account.clone(), 1)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert_eq!(
            http.0.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the shared refresh lock must adopt r2 instead of replaying the spent r1 token"
        );
        assert_eq!(
            b.get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert!(!directory
            .path()
            .join("provider-accounts/refresh/test_scope")
            .join(format!("{}.intent", account.account_id))
            .exists());
    }

    #[tokio::test]
    async fn reconnect_retires_a_pending_grant_from_the_old_account_generation() {
        use secret::AccountCredentialRefresher;
        let (directory, credentials, store) = fixture();
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        credentials
            .store_account_credential(&account.credential_ref, &initial_device_grant())
            .await
            .unwrap();
        publish_account_credential_version(
            directory.path().to_owned(),
            "test_scope".into(),
            AccountCredentialVersion {
                account_id: account.account_id.clone(),
                identity_generation: 1,
                credential_revision: 1,
            },
        )
        .await
        .unwrap();
        let path = directory
            .path()
            .join("provider-accounts/versions/test_scope")
            .join(format!("{}.json", account.account_id));
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let (authority, http) = device_coordinator(store);
        assert!(authority.refresh(account.clone(), 1).await.is_err());
        assert!(!authority.pending_grants.lock().unwrap().is_empty());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, original).unwrap();
        let flow = authority
            .begin_device(account.account_id.clone(), 1)
            .await
            .unwrap();
        http.ready.store(true, std::sync::atomic::Ordering::SeqCst);
        *authority
            .device_flows
            .lock()
            .await
            .get(&flow.flow_id)
            .unwrap()
            .gate
            .lock()
            .await = std::time::Instant::now();
        let result = authority
            .poll_device(&account.account_id, &flow.flow_id)
            .await
            .unwrap();
        let reconnected = result.snapshot.unwrap().accounts[0].clone();
        assert_eq!(reconnected.identity_generation, 2);
        let current = credentials
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .unwrap();
        let requests_before = http.requests.lock().unwrap().len();
        let refreshed = authority
            .refresh(reconnected, current.credential_revision)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(refreshed.identity_generation, 2);
        assert!(refreshed.credential_revision > current.credential_revision);
        assert!(http.requests.lock().unwrap().len() > requests_before);
        assert!(authority.pending_grants.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ledger_publication_repair_does_not_reuse_already_persisted_pending_grant() {
        use secret::AccountCredentialRefresher;
        let (directory, credentials, store) = fixture();
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        credentials
            .store_account_credential(&account.credential_ref, &initial_device_grant())
            .await
            .unwrap();
        publish_account_credential_version(
            directory.path().to_owned(),
            "test_scope".into(),
            AccountCredentialVersion {
                account_id: account.account_id.clone(),
                identity_generation: 1,
                credential_revision: 1,
            },
        )
        .await
        .unwrap();
        let path = directory
            .path()
            .join("provider-accounts/versions/test_scope")
            .join(format!("{}.json", account.account_id));
        let original = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let (authority, http) = device_coordinator(store);
        assert!(authority.refresh(account.clone(), 1).await.is_err());
        assert_eq!(
            credentials
                .get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert_eq!(
            authority
                .pending_grants
                .lock()
                .unwrap()
                .get(&account.account_id)
                .unwrap()
                .credential_revision,
            2
        );
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, original).unwrap();
        assert_eq!(
            authority
                .refresh(account.clone(), 1)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert!(authority.pending_grants.lock().unwrap().is_empty());
        assert_eq!(
            authority
                .refresh(account.clone(), 2)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            3
        );
        assert_eq!(
            http.requests.lock().unwrap().len(),
            2,
            "rejection of repaired revision must perform a fresh rotation"
        );
        assert_eq!(
            provider_config::read_account_credential_version(directory.path(), &account).unwrap(),
            Some(3)
        );
    }
    #[tokio::test]
    async fn rotated_grant_survives_rename_reorder_and_enable_edits_during_network() {
        use secret::AccountCredentialRefresher;
        let (_dir, credentials, store) = fixture();
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        credentials
            .store_account_credential(&account.credential_ref, &initial_device_grant())
            .await
            .unwrap();
        let (authority, http) = device_coordinator(store.clone());
        http.block_token
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let refreshing = {
            let authority = authority.clone();
            let account = account.clone();
            tokio::spawn(async move { authority.refresh(account, 1).await })
        };
        entered(&http.entered).await;
        let renamed = store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: 1,
                operation_id: None,
                action: ProviderAccountAction::Update {
                    account_id: account.account_id.clone(),
                    display_name: "Renamed during rotation".into(),
                },
            })
            .await
            .unwrap();
        let reordered = store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: renamed.config_revision,
                operation_id: None,
                action: ProviderAccountAction::Reorder {
                    provider_id: "openai".into(),
                    account_ids: vec![account.account_id.clone()],
                },
            })
            .await
            .unwrap();
        let disabled = store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: reordered.config_revision,
                operation_id: None,
                action: ProviderAccountAction::SetEnabled {
                    account_id: account.account_id.clone(),
                    enabled: false,
                },
            })
            .await
            .unwrap();
        http.release.notify_one();
        let record = refreshing.await.unwrap().unwrap().unwrap();
        assert_eq!(record.credential_revision, 2);
        assert_eq!(record.identity_generation, 1);
        let current = store.snapshot_metadata().await.unwrap();
        assert_eq!(current.config_revision, disabled.config_revision);
        assert_eq!(current.accounts[0].display_name, "Renamed during rotation");
        assert!(!current.accounts[0].enabled);
        assert_eq!(
            credentials
                .get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        assert!(authority.pending_grants.lock().unwrap().is_empty());
        assert_eq!(
            http.requests.lock().unwrap().len(),
            1,
            "metadata edits must never discard and retry the spent refresh token"
        );
    }
    #[tokio::test]
    async fn browser_cancel_wins_during_exchange_even_after_metadata_rename() {
        let (_dir, credentials, store) = fixture();
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        let (authority, http) = device_coordinator(store.clone());
        let session = authority
            .begin(
                account.account_id.clone(),
                1,
                "http://localhost:1455/auth/callback".into(),
            )
            .await
            .unwrap();
        let state = url::Url::parse(&session.authorization_url)
            .unwrap()
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        let callback = format!(
            "http://localhost:1455/auth/callback?code=synthetic-browser-code&state={state}"
        );
        http.block_token
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let completing = {
            let authority = authority.clone();
            let flow = session.flow_id.clone();
            let callback = callback.clone();
            tokio::spawn(async move { authority.complete(flow, callback).await })
        };
        entered(&http.entered).await;
        store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: 1,
                operation_id: None,
                action: ProviderAccountAction::Update {
                    account_id: account.account_id.clone(),
                    display_name: "Rename while authorizing".into(),
                },
            })
            .await
            .unwrap();
        let cancelling = {
            let authority = authority.clone();
            let account_id = account.account_id.clone();
            let flow = session.flow_id.clone();
            tokio::spawn(async move { authority.cancel(&account_id, &flow).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !cancelling.is_finished(),
            "cancel must synchronize with the in-flight exchange"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if authority
                    .flows
                    .lock()
                    .await
                    .get(&session.flow_id)
                    .unwrap()
                    .phase
                    .load(std::sync::atomic::Ordering::Acquire)
                    == FLOW_CANCELLED
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        http.release.notify_one();
        assert!(completing.await.unwrap().is_err());
        cancelling.await.unwrap().unwrap();
        assert!(!credentials
            .has_account_credential(&account.credential_ref)
            .await
            .unwrap());
        assert!(authority.complete(session.flow_id, callback).await.is_err());
        assert_eq!(http.requests.lock().unwrap().len(), 1);
    }
    struct PausedAccountStorage {
        inner: InMemorySecureStorage,
        pause: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl lingxi_core::host::SecureStorage for PausedAccountStorage {
        async fn store(
            &self,
            service: &str,
            account: &str,
            data: lingxi_core::types::SecureStorageData,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            if service.starts_with("provider-accounts.")
                && self.pause.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.store(service, account, data).await
        }
        async fn retrieve(
            &self,
            service: &str,
            account: &str,
        ) -> Result<
            Option<lingxi_core::types::SecureStorageData>,
            lingxi_core::host::SecureStorageError,
        > {
            self.inner.retrieve(service, account).await
        }
        async fn contains(
            &self,
            service: &str,
            account: &str,
        ) -> Result<bool, lingxi_core::host::SecureStorageError> {
            self.inner.contains(service, account).await
        }
        async fn delete(
            &self,
            service: &str,
            account: &str,
        ) -> Result<(), lingxi_core::host::SecureStorageError> {
            self.inner.delete(service, account).await
        }
        async fn list(
            &self,
            service: &str,
        ) -> Result<Vec<String>, lingxi_core::host::SecureStorageError> {
            self.inner.list(service).await
        }
        fn is_encrypted(&self) -> bool {
            false
        }
        fn backend(&self) -> lingxi_core::host::SecureStorageBackend {
            lingxi_core::host::SecureStorageBackend::MemorySession
        }
    }
    #[tokio::test]
    async fn device_cancel_cannot_report_success_after_durable_commit_has_won() {
        let directory = tempfile::tempdir().unwrap();
        let storage = Arc::new(PausedAccountStorage {
            inner: InMemorySecureStorage::new(),
            pause: Default::default(),
            entered: Default::default(),
            release: Default::default(),
        });
        let credentials = Arc::new(CredentialManager::new(
            storage.clone(),
            Arc::new(platform_posix::PosixClock::new()),
            Arc::new(platform_posix::PosixHttp::new()),
        ));
        let store = Arc::new(ProviderAccountStore::new(
            directory.path().to_owned(),
            "test_scope".into(),
            credentials.clone(),
        ));
        let account = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap()
            .accounts[0]
            .clone();
        let (authority, http) = device_coordinator(store.clone());
        let session = authority
            .begin_device(account.account_id.clone(), 1)
            .await
            .unwrap();
        http.ready.store(true, std::sync::atomic::Ordering::SeqCst);
        *authority
            .device_flows
            .lock()
            .await
            .get(&session.flow_id)
            .unwrap()
            .gate
            .lock()
            .await = std::time::Instant::now();
        storage
            .pause
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let polling = {
            let authority = authority.clone();
            let id = account.account_id.clone();
            let flow = session.flow_id.clone();
            tokio::spawn(async move { authority.poll_device(&id, &flow).await })
        };
        entered(&storage.entered).await;
        let cancelling = {
            let authority = authority.clone();
            let id = account.account_id.clone();
            let flow = session.flow_id.clone();
            tokio::spawn(async move { authority.cancel(&id, &flow).await })
        };
        tokio::task::yield_now().await;
        assert!(
            !cancelling.is_finished(),
            "cancel cannot acknowledge ahead of a committing grant"
        );
        storage.release.notify_one();
        assert!(matches!(
            polling.await.unwrap().unwrap().status,
            AccountOAuthPollStatus::Completed
        ));
        assert!(
            cancelling.await.unwrap().is_err(),
            "commit won; cancellation must not falsely report success"
        );
        assert!(credentials
            .has_account_credential(&account.credential_ref)
            .await
            .unwrap());
    }
    #[tokio::test]
    async fn oauth_flow_is_account_bound_and_removal_fences_before_exchange() {
        let (_dir, _credentials, store) = fixture();
        let snapshot = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap();
        let id = snapshot.accounts[0].account_id.clone();
        let authority = coordinator(store.clone());
        let session = authority
            .begin(id.clone(), 1, "http://localhost:1455/auth/callback".into())
            .await
            .unwrap();
        let url = url::Url::parse(&session.authorization_url).unwrap();
        let state = url
            .query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string();
        assert!(authority
            .complete(
                session.flow_id.clone(),
                "http://localhost:1455/auth/callback?code=unused&state=wrong".into()
            )
            .await
            .is_err());
        assert!(authority
            .verify_flow_account(&session.flow_id, "different-account")
            .await
            .is_err());
        store
            .execute_metadata(ProviderAccountCommand {
                expected_revision: 1,
                operation_id: None,
                action: ProviderAccountAction::Remove { account_id: id },
            })
            .await
            .unwrap();
        assert!(authority
            .complete(
                session.flow_id.clone(),
                format!("http://localhost:1455/auth/callback?code=unused&state={state}")
            )
            .await
            .unwrap_err()
            .contains("removed"));
        assert!(authority
            .complete(
                session.flow_id,
                format!("http://localhost:1455/auth/callback?code=unused&state={state}")
            )
            .await
            .is_err());
    }
    #[tokio::test]
    async fn completed_secret_commit_is_recovered_only_by_mutation_and_does_not_lose_command() {
        let (dir, credentials, store) = fixture();
        let original = store
            .execute(add(AccountAuthKind::ApiKey, 0), Some("old".into()))
            .await
            .unwrap();
        let mut account = original.accounts[0].clone();
        let command = ProviderAccountCommand {
            expected_revision: 1,
            operation_id: Some(77),
            action: ProviderAccountAction::SetCredential {
                account_id: account.account_id.clone(),
            },
        };
        let (_, mut registry) = store.locked_registry().await.unwrap();
        account.identity_generation = 2;
        registry.update(account.clone(), 1).unwrap();
        let record = AccountCredential {
            credential_revision: 2,
            identity_generation: 2,
            kind: AccountCredentialKind::ApiKey {
                key: lingxi_core::types::Secret::new("rotated".into()),
            },
        };
        let pending = AccountCommitIntent {
            snapshot: registry.snapshot(),
            credential_ref: account.credential_ref.clone(),
            identity_generation: Some(2),
            credential_revision: Some(2),
            fingerprint: Some(account_command_fingerprint(&command, Some("rotated")).unwrap()),
        };
        lingxi_core::host::rooted_fs::atomic_write(
            dir.path(),
            &store.pending_path(),
            &serde_json::to_vec(&pending).unwrap(),
            Default::default(),
        )
        .unwrap();
        credentials
            .store_account_credential(&account.credential_ref, &record)
            .await
            .unwrap();
        assert!(store
            .snapshot()
            .await
            .unwrap()
            .health
            .iter()
            .any(|h| h.reason.as_deref() == Some("credential_commit_pending")));
        assert!(dir.path().join(store.pending_path()).exists());
        let recovered = store
            .execute(command, Some("rotated".into()))
            .await
            .unwrap();
        assert_eq!(recovered.config_revision, 2);
        assert_eq!(recovered.accounts[0].identity_generation, 2);
        assert!(!dir.path().join(store.pending_path()).exists());
        let AccountCredentialKind::ApiKey { key } = credentials
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .unwrap()
            .kind
        else {
            panic!("wrong kind")
        };
        assert_eq!(key.expose_secret(), "rotated");
    }

    #[tokio::test]
    async fn changed_secret_retry_never_acknowledges_the_previously_committed_secret() {
        let (dir, credentials, store) = fixture();
        let original = store
            .execute(add(AccountAuthKind::ApiKey, 0), Some("old".into()))
            .await
            .unwrap();
        let mut account = original.accounts[0].clone();
        let command = ProviderAccountCommand {
            expected_revision: 1,
            operation_id: Some(77),
            action: ProviderAccountAction::SetCredential {
                account_id: account.account_id.clone(),
            },
        };
        let (_, mut registry) = store.locked_registry().await.unwrap();
        account.identity_generation = 2;
        registry.update(account.clone(), 1).unwrap();
        let record = AccountCredential {
            credential_revision: 2,
            identity_generation: 2,
            kind: AccountCredentialKind::ApiKey {
                key: lingxi_core::types::Secret::new("rotated".into()),
            },
        };
        let pending = AccountCommitIntent {
            snapshot: registry.snapshot(),
            credential_ref: account.credential_ref.clone(),
            identity_generation: Some(2),
            credential_revision: Some(2),
            fingerprint: Some(account_command_fingerprint(&command, Some("rotated")).unwrap()),
        };
        lingxi_core::host::rooted_fs::atomic_write(
            dir.path(),
            &store.pending_path(),
            &serde_json::to_vec(&pending).unwrap(),
            Default::default(),
        )
        .unwrap();
        credentials
            .store_account_credential(&account.credential_ref, &record)
            .await
            .unwrap();
        assert!(store
            .snapshot()
            .await
            .unwrap()
            .health
            .iter()
            .any(|h| h.reason.as_deref() == Some("credential_commit_pending")));
        assert!(dir.path().join(store.pending_path()).exists());
        assert!(store
            .execute(command, Some("different-replacement".into()))
            .await
            .is_err());
        let recovered = store.snapshot_metadata().await.unwrap();
        assert_eq!(recovered.config_revision, 2);
        assert_eq!(recovered.accounts[0].identity_generation, 2);
        assert!(!dir.path().join(store.pending_path()).exists());
        let AccountCredentialKind::ApiKey { key } = credentials
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .unwrap()
            .kind
        else {
            panic!("wrong kind")
        };
        assert_eq!(key.expose_secret(), "rotated");
    }

    #[tokio::test]
    async fn pending_account_write_does_not_hide_or_disable_another_account() {
        let (dir, _credentials, store) = fixture();
        let first = store
            .execute(add(AccountAuthKind::ApiKey, 0), Some("A".into()))
            .await
            .unwrap();
        let snapshot = store
            .execute(add(AccountAuthKind::ApiKey, 1), Some("B".into()))
            .await
            .unwrap();
        let affected = first.accounts[0].clone();
        let healthy = snapshot.accounts[1].clone();
        let pending = AccountCommitIntent {
            snapshot: snapshot.clone(),
            credential_ref: affected.credential_ref.clone(),
            identity_generation: Some(2),
            credential_revision: Some(2),
            fingerprint: None,
        };
        lingxi_core::host::rooted_fs::atomic_write(
            dir.path(),
            &store.pending_path(),
            &serde_json::to_vec(&pending).unwrap(),
            Default::default(),
        )
        .unwrap();
        let status = store.snapshot().await.unwrap();
        assert_eq!(
            status
                .health
                .iter()
                .find(|h| h.account_id == affected.account_id)
                .unwrap()
                .status,
            AccountHealthStatus::StorageUnavailable
        );
        assert_eq!(
            status
                .health
                .iter()
                .find(|h| h.account_id == healthy.account_id)
                .unwrap()
                .status,
            AccountHealthStatus::Ready
        );
        assert!(store.verify_account(&healthy).await.is_ok());
        assert!(store.verify_account(&affected).await.is_err());
        assert!(dir.path().join(store.pending_path()).exists());
    }

    fn grant(principal: &str, workspace: &str) -> AccountCredential {
        AccountCredential {
            credential_revision: 0,
            identity_generation: 0,
            kind: AccountCredentialKind::OAuth {
                access_token: lingxi_core::types::Secret::new(format!("access-{principal}")),
                refresh_token: Some(lingxi_core::types::Secret::new(format!(
                    "refresh-{principal}"
                ))),
                expires_at: std::time::SystemTime::now() + std::time::Duration::from_secs(3600),
                scopes: vec![],
                issuer: "https://auth.openai.com".into(),
                subject: Some(principal.into()),
                organization_id: Some(workspace.into()),
                email: None,
                fedramp: false,
            },
        }
    }
    #[tokio::test]
    async fn oauth_reconnect_resets_generation_and_same_workspace_users_are_distinct() {
        let (_dir, _credentials, store) = fixture();
        let added = store
            .execute_metadata(add(AccountAuthKind::OAuth, 0))
            .await
            .unwrap();
        let a = store
            .commit_oauth(&added.accounts[0], grant("user-a", "workspace"))
            .await
            .unwrap();
        assert_eq!(a.accounts[0].identity_generation, 2);
        let two = store
            .execute_metadata(add(AccountAuthKind::OAuth, 2))
            .await
            .unwrap();
        let b = two.accounts[1].clone();
        assert!(store
            .commit_oauth(&b, grant("user-a", "workspace"))
            .await
            .unwrap_err()
            .contains("already registered"));
        let two = store
            .commit_oauth(&b, grant("user-b", "workspace"))
            .await
            .unwrap();
        assert_eq!(two.accounts.len(), 2);
        let a = two.accounts[0].clone();
        assert!(store
            .commit_oauth(&a, grant("user-a", "other-workspace"))
            .await
            .is_err());
        let reconnected = store
            .commit_oauth(&a, grant("user-a", "workspace"))
            .await
            .unwrap();
        assert_eq!(
            reconnected.accounts[0].identity_generation,
            a.identity_generation + 1
        );
    }
    #[tokio::test]
    async fn unknown_and_cross_provider_bindings_cannot_be_registered() {
        let (_dir, _credentials, store) = fixture();
        let mut command = add(AccountAuthKind::ApiKey, 0);
        if let ProviderAccountAction::Add {
            execution_binding_id,
            ..
        } = &mut command.action
        {
            *execution_binding_id = "unknown-profile".into();
        }
        assert!(store
            .execute_metadata(command)
            .await
            .unwrap_err()
            .contains("unknown execution binding"));
        let mut command = add(AccountAuthKind::ApiKey, 0);
        if let ProviderAccountAction::Add { provider_id, .. } = &mut command.action {
            *provider_id = "gemini".into();
        }
        assert!(store.execute_metadata(command).await.is_err());
        assert!(store.snapshot_metadata().await.unwrap().accounts.is_empty());
    }

    #[test]
    fn installed_refresh_authority_does_not_retain_credential_manager() {
        let (_dir, credentials, store) = fixture();
        let weak = Arc::downgrade(&credentials);
        let authority = coordinator(store.clone());
        credentials.set_account_credential_refresher(authority.clone());
        drop(store);
        drop(credentials);
        assert!(weak.upgrade().is_none());
        assert!(authority.store().is_err());
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct AccountCommitIntent {
    #[serde(default)]
    fingerprint: Option<AccountCommandFingerprint>,
    snapshot: AccountSnapshot,
    credential_ref: CredentialRef,
    identity_generation: Option<u64>,
    credential_revision: Option<u64>,
}
enum CredentialMutation {
    Store(CredentialRef, AccountCredential),
    Delete(CredentialRef),
}
impl ProviderAccountStore {
    fn pending_path(&self) -> PathBuf {
        PathBuf::from("provider-accounts").join(format!("{}.pending", self.store_scope))
    }
    async fn commit_transaction(
        &self,
        lock: lingxi_core::host::RootedFileLock,
        snapshot: AccountSnapshot,
        mutation: Option<CredentialMutation>,
        fingerprint: Option<AccountCommandFingerprint>,
    ) -> Result<AccountSnapshot, String> {
        let store = self.clone();
        tokio::spawn(async move {
            store
                .commit_transaction_owned(lock, snapshot, mutation, fingerprint)
                .await
        })
        .await
        .map_err(|_| "account transaction worker failed")?
    }

    async fn commit_transaction_owned(
        &self,
        lock: lingxi_core::host::RootedFileLock,
        snapshot: AccountSnapshot,
        mutation: Option<CredentialMutation>,
        fingerprint: Option<AccountCommandFingerprint>,
    ) -> Result<AccountSnapshot, String> {
        let mut lock = lock;
        let credential_version = match &mutation {
            Some(CredentialMutation::Store(reference, credential)) => Some((
                snapshot
                    .accounts
                    .iter()
                    .find(|a| {
                        &a.credential_ref == reference
                            && a.identity_generation == credential.identity_generation
                    })
                    .cloned()
                    .ok_or("account credential commit identity mismatch")?,
                credential.credential_revision,
            )),
            _ => None,
        };
        if let Some(mutation) = mutation {
            let (reference, generation, revision) = match &mutation {
                CredentialMutation::Store(r, c) => (
                    r.clone(),
                    Some(c.identity_generation),
                    Some(c.credential_revision),
                ),
                CredentialMutation::Delete(r) => (r.clone(), None, None),
            };
            let intent = AccountCommitIntent {
                fingerprint,
                snapshot: snapshot.clone(),
                credential_ref: reference,
                identity_generation: generation,
                credential_revision: revision,
            };
            let bytes =
                serde_json::to_vec(&intent).map_err(|_| "account transaction encoding failed")?;
            let root = self.data_dir.clone();
            let path = self.pending_path();
            lock = tokio::task::spawn_blocking(move || {
                let lock = lock;
                lingxi_core::host::rooted_fs::atomic_write(
                    &root,
                    &path,
                    &bytes,
                    lingxi_core::host::AtomicWriteOptions::default(),
                )
                .map_err(|_| "account transaction intent failed")?;
                Ok::<_, &str>(lock)
            })
            .await
            .map_err(|_| "account transaction worker failed")??;
            match mutation {
                CredentialMutation::Store(reference, record) => self
                    .credentials
                    .store_account_credential(&reference, &record)
                    .await
                    .map_err(|_| "account credential commit pending")?,
                CredentialMutation::Delete(reference) => self
                    .credentials
                    .delete_account_credential(&reference)
                    .await
                    .map_err(|_| "account credential removal pending")?,
            }
        }
        let output = snapshot.clone();
        let mut snapshot = snapshot;
        snapshot.health.clear();
        snapshot.model_entitlements.clear();
        snapshot.model_availability.clear();
        snapshot.scoped_health.clear();
        let root = self.data_dir.clone();
        let scope = self.store_scope.clone();
        let pending = self.pending_path();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            if let Some((account, revision)) = credential_version {
                provider_config::write_account_credential_version_locked(&root, &account, revision)
                    .map_err(|_| "account credential version commit pending")?;
            }
            let bytes =
                serde_json::to_vec(&snapshot).map_err(|_| "account directory encoding failed")?;
            lingxi_core::host::rooted_fs::atomic_write(
                &root,
                &PathBuf::from("provider-accounts").join(format!("{scope}.json")),
                &bytes,
                lingxi_core::host::AtomicWriteOptions::default(),
            )
            .map_err(|_| "account directory commit pending")?;
            match lingxi_core::host::rooted_fs::remove_file(&root, &pending) {
                Ok(()) | Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => Ok(()),
                Err(_) => Err("account transaction finalization pending"),
            }
        })
        .await
        .map_err(|_| "account directory worker failed")??;
        Ok(output)
    }
    /// A mutating operation may finish an acknowledged secret commit. Reads never repair.
    async fn recover_pending(
        &self,
        mut lock: lingxi_core::host::RootedFileLock,
        command: &ProviderAccountCommand,
        credential: Option<&str>,
        registry: &AccountRegistry,
    ) -> Result<
        (
            lingxi_core::host::RootedFileLock,
            Option<(AccountSnapshot, bool)>,
        ),
        String,
    > {
        let root = self.data_dir.clone();
        let path = self.pending_path();
        let pending = tokio::task::spawn_blocking(move || {
            match lingxi_core::host::rooted_fs::read_to_string_limited(
                &root,
                &path,
                4 * 1024 * 1024,
            ) {
                Ok(text) => serde_json::from_str::<AccountCommitIntent>(&text)
                    .map(Some)
                    .map_err(|_| "account transaction invalid"),
                Err(lingxi_core::host::rooted_fs::FsError::NotFound(_)) => Ok(None),
                Err(_) => Err("account transaction unavailable"),
            }
        })
        .await
        .map_err(|_| "account directory worker failed")??;
        let Some(pending) = pending else {
            return Ok((lock, None));
        };
        let record = self
            .credentials
            .get_account_credential_uncached(&pending.credential_ref)
            .await
            .map_err(|_| "account transaction credential storage unavailable")?;
        let committed = match (
            pending.identity_generation,
            pending.credential_revision,
            record,
        ) {
            (None, None, None) => true,
            (Some(generation), Some(revision), Some(record)) => {
                record.identity_generation == generation && record.credential_revision == revision
            }
            _ => false,
        };
        if committed {
            if let (Some(generation), Some(revision)) =
                (pending.identity_generation, pending.credential_revision)
            {
                let target = pending
                    .snapshot
                    .accounts
                    .iter()
                    .find(|a| {
                        a.credential_ref == pending.credential_ref
                            && a.identity_generation == generation
                    })
                    .cloned()
                    .ok_or("account transaction identity invalid")?;
                let root = self.data_dir.clone();
                lock = tokio::task::spawn_blocking(move || {
                    let lock = lock;
                    provider_config::write_account_credential_version_locked(
                        &root, &target, revision,
                    )?;
                    Ok::<_, String>(lock)
                })
                .await
                .map_err(|_| "account version worker failed")??;
            }

            let same_operation = pending
                .fingerprint
                .as_ref()
                .map(|fingerprint| fingerprint.matches(command, credential))
                .transpose()?
                .unwrap_or(false);
            return Ok((lock, Some((pending.snapshot, same_operation))));
        }
        if matches!(&command.action, ProviderAccountAction::Add { .. })
            && pending
                .fingerprint
                .as_ref()
                .map(|fingerprint| fingerprint.matches(command, credential))
                .transpose()?
                .unwrap_or(false)
        {
            return Ok((lock, None));
        }
        // An explicit credential replacement/removal can retry a failed secure-store write.
        let target = match &command.action {
            ProviderAccountAction::SetCredential { account_id }
            | ProviderAccountAction::Logout { account_id }
            | ProviderAccountAction::ClearCredential { account_id }
            | ProviderAccountAction::Remove { account_id } => Some(account_id),
            _ => None,
        };
        if target.is_some_and(|id| {
            registry
                .snapshot()
                .accounts
                .iter()
                .any(|a| &a.account_id == id && a.credential_ref == pending.credential_ref)
        }) {
            return Ok((lock, None));
        }
        Err("account credential transaction incomplete; retry replacement or removal".into())
    }
}
