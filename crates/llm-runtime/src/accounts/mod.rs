//! Host-owned account health and bounded selection. No credential material is
//! retained here. Stores execute each operation atomically; network I/O never
//! runs under a store lock.
use lingxi_core::host::provider_accounts::{AccountMetadata, AccountSnapshot};
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};
pub type SharedAccountDirectory = Arc<std::sync::RwLock<AccountSnapshot>>;
use crate::{ConnectionHop, LlmError, ResolvedRoute};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureScope {
    Account,
    Model(String),
    Endpoint(String),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    CoolingDown,
    NeedsAuth,
    QuotaExhausted,
    StorageUnavailable,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUnavailable {
    pub account_id: String,
    pub reason: String,
    pub scope: FailureScope,
    pub retryable: bool,
    pub recovery_at_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountResourceScope {
    pub store_scope: String,
    pub account_id: String,
    pub identity_generation: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthRecord {
    #[serde(default)]
    pub credential_revision: Option<u64>,
    pub scope: FailureScope,
    pub status: HealthStatus,
    pub reason: String,
    pub recovery_at_ms: Option<u64>,
    pub revision: u64,
    pub failures: u32,
    pub probe_owner: Option<String>,
    pub probe_until_ms: Option<u64>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HealthDocument {
    pub revision: u64,
    pub identities: BTreeMap<String, (u64, u64)>,
    pub records: BTreeMap<String, HealthRecord>,
    /// Completion tombstones retain probe fencing after a successful recovery
    /// removes its fault row.
    #[serde(default)]
    pub fences: BTreeMap<String, u64>,
    /// Entitlements are observations of actual model business calls, independent
    /// of account authentication health and provider-wide model catalogues.
    #[serde(default)]
    pub model_entitlements:
        BTreeMap<String, lingxi_core::host::provider_accounts::AccountModelEntitlement>,
}
#[derive(Debug, Clone)]
pub enum PoolOperation {
    Claim {
        account: AccountMetadata,
        model: String,
        endpoint: String,
        now_ms: u64,
        owner: String,
    },
    Finish {
        account: AccountMetadata,
        model: String,
        endpoint: String,
        owner: String,
        claimed_revision: u64,
        credential_revision: Option<u64>,
        outcome: Option<AccountFailure>,
        now_ms: u64,
    },
}
#[derive(Debug, Clone)]
pub struct AccountFailure {
    pub status: HealthStatus,
    pub scope: FailureScope,
    pub reason: String,
    pub recovery_at_ms: Option<u64>,
}
#[derive(Debug, Clone)]
pub enum PoolResult {
    Claimed { revision: u64 },
    Unavailable(AccountUnavailable),
    Recorded { recovery_at_ms: Option<u64> },
    Stale,
}
#[async_trait::async_trait]
pub trait AccountPoolStore: std::fmt::Debug + Send + Sync {
    async fn transact(&self, operation: PoolOperation) -> Result<PoolResult, String>;
    async fn latest_snapshot(&self) -> Result<Option<AccountSnapshot>, String> {
        Ok(None)
    }
}
#[derive(Debug, Default)]
pub struct MemoryAccountPoolStore(Mutex<HealthDocument>);
#[async_trait::async_trait]
impl AccountPoolStore for MemoryAccountPoolStore {
    async fn transact(&self, op: PoolOperation) -> Result<PoolResult, String> {
        Ok(self.0.lock().unwrap_or_else(|e| e.into_inner()).apply(op))
    }
}
fn account_key(account: &AccountMetadata) -> String {
    // Structured encoding avoids delimiter collisions between storage domains.
    serde_json::to_string(&(
        account.credential_ref.store_scope.as_str(),
        account.account_id.as_str(),
    ))
    .expect("string tuple")
}
fn record_key(account: &AccountMetadata, scope: &FailureScope) -> String {
    serde_json::to_string(&(account_key(account), account.identity_generation, scope))
        .expect("scope tuple")
}
fn scopes(model: &str, endpoint: &str) -> [FailureScope; 3] {
    [
        FailureScope::Account,
        FailureScope::Model(model.to_owned()),
        FailureScope::Endpoint(endpoint.to_owned()),
    ]
}
impl HealthDocument {
    /// Authentication health is meaningful only for the exact rejected grant.
    pub fn invalidate_auth_for_credential(
        &mut self,
        account: &AccountMetadata,
        current_revision: Option<u64>,
    ) {
        let key = record_key(account, &FailureScope::Account);
        if self.records.get(&key).is_some_and(|row| {
            (row.status == HealthStatus::NeedsAuth || row.reason == "derived_token_rejected")
                && row.credential_revision != current_revision
        }) {
            self.records.remove(&key);
            self.revision = self.revision.saturating_add(1);
            self.fences.insert(key, self.revision);
        }
    }
    pub fn model_entitlements(
        &self,
        account: &AccountMetadata,
        now_ms: u64,
    ) -> Vec<lingxi_core::host::provider_accounts::AccountModelEntitlement> {
        use lingxi_core::host::provider_accounts::AccountModelEntitlementStatus;
        self.model_entitlements
            .iter()
            .filter_map(|(key, value)| {
                let (identity, generation, _): (String, u64, FailureScope) =
                    serde_json::from_str(key).ok()?;
                if identity != account_key(account) || generation != account.identity_generation {
                    return None;
                }
                let mut value = value.clone();
                if value.status == AccountModelEntitlementStatus::Forbidden
                    && value.retry_at_ms.is_some_and(|retry| retry <= now_ms)
                {
                    value.status = AccountModelEntitlementStatus::Unverified;
                }
                Some(value)
            })
            .collect()
    }
    pub fn account_records(&self, account: &AccountMetadata) -> Vec<HealthRecord> {
        let identity = account_key(account);
        self.records
            .iter()
            .filter_map(|(key, record)| {
                let (stored_identity, generation, _): (String, u64, FailureScope) =
                    serde_json::from_str(key).ok()?;
                (stored_identity == identity
                    && generation == account.identity_generation
                    && record.scope == FailureScope::Account)
                    .then(|| record.clone())
            })
            .collect()
    }
    /// Exact-identity model and endpoint faults for metadata-only readiness.
    /// Probe owners stay inside the health backend; projections expose only
    /// the effective unavailable deadline and sanitized reason.
    pub fn scoped_records(&self, account: &AccountMetadata) -> Vec<HealthRecord> {
        let identity = account_key(account);
        self.records
            .iter()
            .filter_map(|(key, record)| {
                let (stored_identity, generation, _): (String, u64, FailureScope) =
                    serde_json::from_str(key).ok()?;
                (stored_identity == identity
                    && generation == account.identity_generation
                    && record.scope != FailureScope::Account)
                    .then(|| record.clone())
            })
            .collect()
    }
    pub fn apply(&mut self, op: PoolOperation) -> PoolResult {
        let account = match &op {
            PoolOperation::Claim { account, .. } | PoolOperation::Finish { account, .. } => account,
        };
        let identity_key = account_key(account);
        let incoming = (account.identity_generation, account.config_revision);
        match self.identities.get(&identity_key) {
            Some(current) if *current > incoming => return PoolResult::Stale,
            Some(current) if *current < incoming => {
                // A credential/config replacement fences every late completion.
                let old_generation = self.identities.get(&identity_key).map(|old| old.0);
                self.identities.insert(identity_key, incoming);
                // A priority/name edit changes configuration revision without
                // changing credential identity. Preserve its cooldown.
                if old_generation.is_some_and(|old| old != incoming.0) {
                    let prefix = serde_json::to_string(&account_key(account)).expect("string");
                    self.records
                        .retain(|key, _| !key.starts_with(&format!("[{prefix},")));
                    self.model_entitlements
                        .retain(|key, _| !key.starts_with(&format!("[{prefix},")));
                }
            }
            None => {
                self.identities.insert(identity_key, incoming);
            }
            _ => {}
        }
        match op {
            PoolOperation::Claim {
                account,
                model,
                endpoint,
                now_ms,
                owner,
            } => {
                if !account.enabled {
                    return PoolResult::Unavailable(AccountUnavailable {
                        account_id: account.account_id,
                        reason: "disabled".into(),
                        scope: FailureScope::Account,
                        retryable: false,
                        recovery_at_ms: None,
                    });
                }
                // Inspect all scopes before mutating any leases.
                for scope in scopes(&model, &endpoint) {
                    if let Some(row) = self.records.get(&record_key(&account, &scope)) {
                        let recovered = row.recovery_at_ms.is_some_and(|until| until <= now_ms);
                        let held = row.probe_until_ms.is_some_and(|until| until > now_ms);
                        if !recovered || held {
                            return PoolResult::Unavailable(AccountUnavailable {
                                account_id: account.account_id.clone(),
                                reason: if held {
                                    "probe_in_progress".into()
                                } else {
                                    row.reason.clone()
                                },
                                scope,
                                retryable: row.recovery_at_ms.is_some(),
                                recovery_at_ms: if held {
                                    row.probe_until_ms
                                } else {
                                    row.recovery_at_ms
                                },
                            });
                        }
                    }
                }
                let probing = scopes(&model, &endpoint)
                    .iter()
                    .any(|scope| self.records.contains_key(&record_key(&account, scope)));
                if probing {
                    self.revision = self.revision.saturating_add(1);
                }
                for scope in scopes(&model, &endpoint) {
                    if let Some(row) = self.records.get_mut(&record_key(&account, &scope)) {
                        row.probe_owner = Some(owner.clone());
                        row.probe_until_ms = Some(now_ms.saturating_add(30_000));
                        self.fences
                            .insert(record_key(&account, &scope), self.revision);
                    }
                }
                PoolResult::Claimed {
                    revision: self.revision,
                }
            }
            PoolOperation::Finish {
                account,
                model,
                endpoint,
                owner,
                claimed_revision,
                credential_revision,
                outcome,
                now_ms,
            } => {
                let finishing_scopes = outcome.as_ref().map_or_else(
                    || scopes(&model, &endpoint).to_vec(),
                    |failure| vec![failure.scope.clone()],
                );
                if finishing_scopes.iter().any(|scope| {
                    self.fences
                        .get(&record_key(&account, scope))
                        .is_some_and(|revision| *revision > claimed_revision)
                }) {
                    return PoolResult::Stale;
                }
                // Only the lease owner may finish an expired/reclaimed probe.
                for scope in scopes(&model, &endpoint) {
                    if self
                        .records
                        .get(&record_key(&account, &scope))
                        .is_some_and(|row| {
                            row.probe_owner
                                .as_ref()
                                .is_some_and(|current| current != &owner)
                        })
                    {
                        return PoolResult::Stale;
                    }
                }
                // A success that started before a newer failure cannot erase
                // that failure. Probe owners still require the lease token.
                if outcome.is_none()
                    && scopes(&model, &endpoint).iter().any(|scope| {
                        self.records
                            .get(&record_key(&account, scope))
                            .is_some_and(|row| row.revision > claimed_revision)
                    })
                {
                    return PoolResult::Stale;
                }
                self.revision = self.revision.saturating_add(1);
                for scope in finishing_scopes {
                    self.fences
                        .insert(record_key(&account, &scope), self.revision);
                }
                let mut recorded_recovery = None;
                if let Some(failure) = outcome {
                    let key = record_key(&account, &failure.scope);
                    let failures = self
                        .records
                        .get(&key)
                        .map_or(1, |r| r.failures.saturating_add(1));
                    let recovery_at_ms = if failure.status == HealthStatus::CoolingDown {
                        Some(failure.recovery_at_ms.unwrap_or_else(|| {
                            now_ms.saturating_add(
                                30_000u64
                                    .saturating_mul(1u64 << failures.saturating_sub(1).min(4))
                                    .min(600_000),
                            )
                        }))
                    } else {
                        failure.recovery_at_ms
                    };
                    recorded_recovery = recovery_at_ms;
                    if let FailureScope::Model(model_id) = &failure.scope {
                        if matches!(
                            failure.reason.as_str(),
                            "model_permission_denied" | "model_unavailable"
                        ) {
                            self.model_entitlements.insert(key.clone(), lingxi_core::host::provider_accounts::AccountModelEntitlement {
                                account_id: account.account_id.clone(),
                                identity_generation: account.identity_generation,
                                model_id: model_id.clone(),
                                status: lingxi_core::host::provider_accounts::AccountModelEntitlementStatus::Forbidden,
                                observed_at_ms: now_ms,
                                retry_at_ms: recovery_at_ms,
                                reason: Some(failure.reason.clone()),
                            });
                        }
                    }
                    self.records.insert(
                        key,
                        HealthRecord {
                            credential_revision: (failure.status == HealthStatus::NeedsAuth
                                || failure.reason == "derived_token_rejected")
                                .then_some(credential_revision)
                                .flatten(),
                            scope: failure.scope,
                            status: failure.status,
                            reason: failure.reason,
                            recovery_at_ms,
                            revision: self.revision,
                            failures,
                            probe_owner: None,
                            probe_until_ms: None,
                        },
                    );
                    // Release any other scope leases owned by this attempt.
                    for scope in scopes(&model, &endpoint) {
                        if let Some(row) = self.records.get_mut(&record_key(&account, &scope)) {
                            if row.probe_owner.as_deref() == Some(&owner) {
                                row.probe_owner = None;
                                row.probe_until_ms = None;
                            }
                        }
                    }
                } else {
                    for scope in scopes(&model, &endpoint) {
                        self.records.remove(&record_key(&account, &scope));
                    }
                    self.model_entitlements.insert(record_key(&account, &FailureScope::Model(model.clone())), lingxi_core::host::provider_accounts::AccountModelEntitlement {
                        account_id: account.account_id.clone(),
                        identity_generation: account.identity_generation,
                        model_id: model,
                        status: lingxi_core::host::provider_accounts::AccountModelEntitlementStatus::Confirmed,
                        observed_at_ms: now_ms,
                        retry_at_ms: None,
                        reason: None,
                    });
                }
                PoolResult::Recorded {
                    recovery_at_ms: recorded_recovery,
                }
            }
        }
    }
}

pub trait AccountClock: std::fmt::Debug + Send + Sync {
    fn now_ms(&self) -> u64;
}
#[derive(Debug)]
struct SystemClock;
impl AccountClock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}
#[derive(Debug)]
pub struct AccountPool {
    directory: SharedAccountDirectory,
    store: Arc<dyn AccountPoolStore>,
    clock: Arc<dyn AccountClock>,
}
impl AccountPool {
    pub(crate) async fn refresh_from_store(&self) -> Result<(), LlmError> {
        if let Some(snapshot) = self
            .store
            .latest_snapshot()
            .await
            .map_err(|_| LlmError::AccountStorageUnavailable)?
        {
            let current = self.snapshot();
            if snapshot.config_revision > current.config_revision {
                self.refresh(snapshot)
                    .map_err(|_| LlmError::AccountStorageUnavailable)?;
            }
        }
        Ok(())
    }
    pub fn new(accounts: Vec<AccountMetadata>, store: Arc<dyn AccountPoolStore>) -> Self {
        let snapshot = AccountSnapshot {
            store_scope: accounts
                .first()
                .map(|a| a.credential_ref.store_scope.clone())
                .unwrap_or_default(),
            config_revision: accounts
                .iter()
                .map(|a| a.config_revision)
                .max()
                .unwrap_or(0),
            accounts,
            health: vec![],
            model_entitlements: vec![],
            scoped_health: vec![],
            model_availability: vec![],
            provider_policies: Default::default(),
        };
        Self::from_directory(Arc::new(std::sync::RwLock::new(snapshot)), store)
    }
    pub fn from_directory(
        directory: SharedAccountDirectory,
        store: Arc<dyn AccountPoolStore>,
    ) -> Self {
        Self {
            directory,
            store,
            clock: Arc::new(SystemClock),
        }
    }
    pub fn directory(&self) -> SharedAccountDirectory {
        self.directory.clone()
    }
    pub fn snapshot(&self) -> AccountSnapshot {
        self.directory
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    pub fn refresh(&self, snapshot: AccountSnapshot) -> Result<(), String> {
        let mut current = self.directory.write().unwrap_or_else(|e| e.into_inner());
        if current.store_scope != snapshot.store_scope
            || snapshot.config_revision < current.config_revision
            || (snapshot.config_revision == current.config_revision
                && (snapshot.accounts != current.accounts
                    || snapshot.provider_policies != current.provider_policies))
        {
            return Err("stale account directory or storage scope mismatch".into());
        }
        *current = snapshot;
        Ok(())
    }
    pub fn with_clock(mut self, clock: Arc<dyn AccountClock>) -> Self {
        self.clock = clock;
        self
    }
    pub fn account_for_profile(&self, profile: &str) -> Option<AccountMetadata> {
        self.snapshot()
            .accounts
            .into_iter()
            .find(|a| profile == format!("account.{}", a.account_id))
    }
    pub fn coordinator(
        self: &Arc<Self>,
        route: &ResolvedRoute,
        endpoints: &BTreeMap<String, String>,
    ) -> Option<AttemptCoordinator> {
        let mut candidates = vec![ConnectionHop {
            profile_name: route.profile_name.clone(),
            request_model: route.request_model.clone(),
        }];
        candidates.extend(route.connection_chain.clone());
        self.coordinator_from_candidates(&route.display_model, candidates, endpoints)
    }
    pub(crate) fn coordinator_from_candidates(
        self: &Arc<Self>,
        model: &str,
        candidates: Vec<ConnectionHop>,
        endpoints: &BTreeMap<String, String>,
    ) -> Option<AttemptCoordinator> {
        self.coordinator_from_snapshot(model, candidates, endpoints, self.snapshot())
    }
    pub(crate) fn coordinator_from_snapshot(
        self: &Arc<Self>,
        model: &str,
        mut candidates: Vec<ConnectionHop>,
        endpoints: &BTreeMap<String, String>,
        snapshot: AccountSnapshot,
    ) -> Option<AttemptCoordinator> {
        let accounts: BTreeMap<_, _> = snapshot
            .accounts
            .into_iter()
            .map(|a| (format!("account.{}", a.account_id), a))
            .collect();
        candidates.retain(|hop| accounts.contains_key(&hop.profile_name));
        if candidates.is_empty() {
            return None;
        }
        candidates.sort_by_key(|hop| {
            accounts
                .get(&hop.profile_name)
                .map_or(u32::MAX, |a| a.user_priority)
        });
        Some(AttemptCoordinator {
            pool: self.clone(),
            accounts,
            candidates,
            endpoints: endpoints.clone(),
            model: model.to_owned(),
            visited: HashSet::new(),
            active: None,
            active_model: model.to_owned(),
            rejected_credential_revision: None,
            unavailable: Vec::new(),
            last_failed: None,
        })
    }
}
#[derive(Debug)]
pub struct AttemptCoordinator {
    pool: Arc<AccountPool>,
    accounts: BTreeMap<String, AccountMetadata>,
    candidates: Vec<ConnectionHop>,
    endpoints: BTreeMap<String, String>,
    model: String,
    visited: HashSet<String>,
    active: Option<(AccountMetadata, String, String, u64)>,
    active_model: String,
    rejected_credential_revision: Option<u64>,
    unavailable: Vec<AccountUnavailable>,
    last_failed: Option<String>,
}
impl AttemptCoordinator {
    pub(crate) fn set_rejected_credential_revision(&mut self, revision: Option<u64>) {
        self.rejected_credential_revision = revision;
    }
    pub(crate) fn map_candidate_models(&mut self, mut model: impl FnMut(&str) -> String) {
        for candidate in &mut self.candidates {
            candidate.request_model = model(&candidate.profile_name);
        }
    }
    pub(crate) fn candidate_profiles(&self) -> Vec<String> {
        self.candidates
            .iter()
            .map(|candidate| candidate.profile_name.clone())
            .collect()
    }
    pub(crate) fn retain_supported(&mut self, mut supported: impl FnMut(&ConnectionHop) -> bool) {
        self.candidates.retain(|candidate| {
            if supported(candidate) {
                return true;
            }
            if let Some(account) = self.accounts.get(&candidate.profile_name) {
                self.unavailable.push(AccountUnavailable {
                    account_id: account.account_id.clone(),
                    reason: "capability_ineligible".into(),
                    scope: FailureScope::Model(self.model.clone()),
                    retryable: false,
                    recovery_at_ms: None,
                });
            }
            false
        });
    }
    pub(crate) fn prefer_profile(&mut self, profile: &str) {
        if let Some(index) = self
            .candidates
            .iter()
            .position(|candidate| candidate.profile_name == profile)
        {
            let candidate = self.candidates.remove(index);
            self.candidates.insert(0, candidate);
        }
    }
    pub(crate) fn pin_scope(&mut self, scope: &AccountResourceScope) -> bool {
        self.candidates.retain(|candidate| {
            self.accounts
                .get(&candidate.profile_name)
                .is_some_and(|account| {
                    account.account_id == scope.account_id
                        && account.identity_generation == scope.identity_generation
                        && account.credential_ref.store_scope == scope.store_scope
                })
        });
        !self.candidates.is_empty()
    }
    pub(crate) fn permits(
        &self,
        scope: &AccountResourceScope,
        profile: &str,
        model: &str,
    ) -> Option<AccountMetadata> {
        let account = self.accounts.get(profile)?;
        (account.enabled
            && account.account_id == scope.account_id
            && account.identity_generation == scope.identity_generation
            && account.credential_ref.store_scope == scope.store_scope
            && self.candidates.iter().any(|candidate| {
                candidate.profile_name == profile && candidate.request_model == model
            }))
        .then(|| account.clone())
    }
    pub fn usage_attribution(
        &self,
    ) -> Option<lingxi_core::host::provider_accounts::AccountUsageAttribution> {
        self.active.as_ref().map(|(account, _, _, _)| {
            lingxi_core::host::provider_accounts::AccountUsageAttribution::from_account(account)
        })
    }
    pub fn active_scope(&self) -> Option<AccountResourceScope> {
        self.active
            .as_ref()
            .map(|(a, _, _, _)| AccountResourceScope {
                store_scope: a.credential_ref.store_scope.clone(),
                account_id: a.account_id.clone(),
                identity_generation: a.identity_generation,
            })
    }
    pub fn last_failed_account_key(&self) -> Option<String> {
        self.last_failed.clone()
    }
    pub async fn select_with_scope(
        &mut self,
        scope: &crate::model::retry_scope::ModelCallRetryScope,
    ) -> Result<ConnectionHop, LlmError> {
        self.visited
            .extend(self.candidates.iter().filter_map(|hop| {
                let account = self.accounts.get(&hop.profile_name)?;
                scope
                    .account_failed(&record_key(
                        account,
                        &FailureScope::Model(hop.request_model.clone()),
                    ))
                    .then(|| account.account_id.clone())
            }));
        self.select().await
    }
    pub async fn select(&mut self) -> Result<ConnectionHop, LlmError> {
        for hop in &self.candidates {
            let account = self
                .accounts
                .get(&hop.profile_name)
                .expect("captured account")
                .clone();
            if !self.visited.insert(account.account_id.clone()) {
                continue;
            }
            if self.visited.len() > 64 {
                break;
            }
            let owner = uuid::Uuid::new_v4().to_string();
            let endpoint = self
                .endpoints
                .get(&hop.profile_name)
                .cloned()
                .unwrap_or_default();
            let result = self
                .pool
                .store
                .transact(PoolOperation::Claim {
                    account: account.clone(),
                    model: hop.request_model.clone(),
                    endpoint: endpoint.clone(),
                    now_ms: self.pool.clock.now_ms(),
                    owner: owner.clone(),
                })
                .await;
            match result {
                Ok(PoolResult::Claimed { revision }) => {
                    self.active_model = hop.request_model.clone();
                    self.active = Some((account, endpoint, owner, revision));
                    return Ok(hop.clone());
                }
                Ok(PoolResult::Unavailable(reason)) => self.unavailable.push(reason),
                Ok(_) => self.unavailable.push(AccountUnavailable {
                    account_id: account.account_id,
                    reason: "stale_configuration".into(),
                    scope: FailureScope::Account,
                    retryable: false,
                    recovery_at_ms: None,
                }),
                Err(_) => self.unavailable.push(AccountUnavailable {
                    account_id: account.account_id,
                    reason: "storage_unavailable".into(),
                    scope: FailureScope::Account,
                    retryable: true,
                    recovery_at_ms: None,
                }),
            }
        }
        Err(LlmError::AllAccountsUnavailable {
            earliest_recovery_ms: self
                .unavailable
                .iter()
                .filter_map(|a| a.recovery_at_ms)
                .min(),
            accounts: self.unavailable.clone(),
        })
    }
    pub async fn failed(&mut self, error: &LlmError) -> bool {
        self.failed_with_retry_after(error, None).await
    }
    pub(crate) async fn failed_with_retry_after(
        &mut self,
        error: &LlmError,
        retry_after: Option<std::time::Duration>,
    ) -> bool {
        let Some((_, endpoint, _, _)) = self.active.as_ref() else {
            return false;
        };
        let Some(mut failure) = classify(
            error,
            &self.active_model,
            endpoint,
            self.pool.clock.now_ms(),
        ) else {
            return false;
        };
        if matches!(error.classification_source(), LlmError::QuotaExceeded) {
            if let Some(delay) = retry_after {
                failure.recovery_at_ms = Some(
                    self.pool
                        .clock
                        .now_ms()
                        .saturating_add(delay.as_millis().try_into().unwrap_or(u64::MAX)),
                );
            }
        }
        self.record_failure(failure).await
    }
    pub async fn refresh_failed(&mut self, error: &LlmError) -> bool {
        let failure = if matches!(
            error.classification_source(),
            LlmError::OAuthRefreshDead | LlmError::Authentication { .. }
        ) {
            classify(error, &self.active_model, "", self.pool.clock.now_ms())
                .expect("auth classification")
        } else {
            AccountFailure {
                status: HealthStatus::CoolingDown,
                scope: FailureScope::Account,
                reason: "auth_refresh_unavailable".into(),
                recovery_at_ms: Some(self.pool.clock.now_ms().saturating_add(5_000)),
            }
        };
        self.record_failure(failure).await
    }
    pub(crate) async fn derived_token_rejected(&mut self) -> bool {
        self.record_failure(AccountFailure {
            status: HealthStatus::CoolingDown,
            scope: FailureScope::Account,
            reason: "derived_token_rejected".into(),
            recovery_at_ms: Some(self.pool.clock.now_ms().saturating_add(5_000)),
        })
        .await
    }
    async fn record_failure(&mut self, failure: AccountFailure) -> bool {
        let Some((account, endpoint, owner, claimed_revision)) = self.active.clone() else {
            return false;
        };
        self.last_failed = Some(record_key(
            &account,
            &FailureScope::Model(self.active_model.clone()),
        ));
        self.unavailable.push(AccountUnavailable {
            account_id: account.account_id.clone(),
            reason: failure.reason.clone(),
            scope: failure.scope.clone(),
            retryable: failure.recovery_at_ms.is_some()
                || failure.status == HealthStatus::CoolingDown,
            recovery_at_ms: failure.recovery_at_ms,
        });
        let recorded = self
            .pool
            .store
            .transact(PoolOperation::Finish {
                account,
                model: self.active_model.clone(),
                endpoint,
                owner,
                claimed_revision,
                credential_revision: self.rejected_credential_revision,
                outcome: Some(failure),
                now_ms: self.pool.clock.now_ms(),
            })
            .await;
        let stale = matches!(&recorded, Ok(PoolResult::Stale));
        if let Ok(PoolResult::Recorded { recovery_at_ms }) = recorded {
            if let Some(reason) = self.unavailable.last_mut() {
                reason.recovery_at_ms = recovery_at_ms;
            }
        }
        self.active = None;
        !stale
    }
    pub async fn succeeded(&mut self) {
        if let Some((account, endpoint, owner, claimed_revision)) = self.active.take() {
            let _ = self
                .pool
                .store
                .transact(PoolOperation::Finish {
                    account,
                    model: self.active_model.clone(),
                    endpoint,
                    owner,
                    claimed_revision,
                    credential_revision: None,
                    outcome: None,
                    now_ms: self.pool.clock.now_ms(),
                })
                .await;
        }
    }
}
/// Only explicit provider rejections authorize account replay. Generic transport
/// errors have unknown remote settlement and never rotate accounts here.
pub fn classify(
    error: &LlmError,
    model: &str,
    endpoint: &str,
    now_ms: u64,
) -> Option<AccountFailure> {
    let (status, scope, reason, recovery_at_ms) = match error.classification_source() {
        LlmError::AccountStorageUnavailable => (
            HealthStatus::StorageUnavailable,
            FailureScope::Account,
            "storage_unavailable",
            Some(now_ms.saturating_add(5_000)),
        ),
        LlmError::Authentication { .. } | LlmError::OAuthRefreshDead => (
            HealthStatus::NeedsAuth,
            FailureScope::Account,
            "needs_auth",
            None,
        ),
        LlmError::QuotaExceeded => (
            HealthStatus::QuotaExhausted,
            FailureScope::Account,
            "quota_exhausted",
            Some(now_ms.saturating_add(300_000)),
        ),
        LlmError::PermissionDenied { .. } => (
            HealthStatus::CoolingDown,
            FailureScope::Model(model.into()),
            "model_permission_denied",
            Some(now_ms.saturating_add(300_000)),
        ),
        LlmError::ModelUnavailable => (
            HealthStatus::CoolingDown,
            FailureScope::Model(model.into()),
            "model_unavailable",
            Some(now_ms.saturating_add(300_000)),
        ),
        LlmError::RateLimited { retry_after, scope } => (
            HealthStatus::CoolingDown,
            if scope.as_deref() == Some("account") {
                FailureScope::Account
            } else {
                FailureScope::Model(model.into())
            },
            "rate_limited",
            retry_after.map(|delay| {
                now_ms.saturating_add(delay.as_millis().try_into().unwrap_or(u64::MAX))
            }),
        ),
        LlmError::Overloaded { .. } | LlmError::ProviderInternal => (
            HealthStatus::CoolingDown,
            FailureScope::Endpoint(endpoint.into()),
            "endpoint_unavailable",
            None,
        ),
        _ => return None,
    };
    Some(AccountFailure {
        status,
        scope,
        reason: reason.into(),
        recovery_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{AccountAuthKind, CredentialRef};
    fn account() -> AccountMetadata {
        AccountMetadata {
            account_id: "a".into(),
            provider_id: "p".into(),
            display_name: "a".into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "p".into(),
            credential_ref: CredentialRef {
                store_scope: "test".into(),
                secret_ref: "acct.a".into(),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 1,
            identity_generation: 1,
        }
    }
    #[tokio::test]
    async fn model_health_uses_selected_wire_model_rather_than_display_alias() {
        let a = account();
        let store = Arc::new(MemoryAccountPoolStore::default());
        let pool = Arc::new(AccountPool::new(vec![a.clone()], store.clone()));
        let candidates = vec![ConnectionHop {
            profile_name: "account.a".into(),
            request_model: "wire-model".into(),
        }];
        let mut coordinator = pool
            .coordinator_from_candidates("Friendly display", candidates.clone(), &BTreeMap::new())
            .unwrap();
        coordinator.select().await.unwrap();
        assert!(coordinator.failed(&LlmError::ModelUnavailable).await);
        let entitlement = store.0.lock().unwrap().model_entitlements(&a, 0);
        assert_eq!(entitlement[0].model_id, "wire-model");
        let mut alias = pool
            .coordinator_from_candidates("Another alias", candidates, &BTreeMap::new())
            .unwrap();
        assert!(matches!(
            alias.select().await,
            Err(LlmError::AllAccountsUnavailable { .. })
        ));
    }
    #[test]
    fn entitlement_is_business_observed_model_scoped_and_expires_for_reprobe() {
        use lingxi_core::host::provider_accounts::AccountModelEntitlementStatus;
        let a = account();
        let mut doc = HealthDocument::default();
        doc.apply(claim(&a, 0, "first"));
        doc.apply(finish(
            &a,
            "first",
            0,
            classify(&LlmError::ModelUnavailable, "m", "endpoint", 0),
        ));
        let denied = doc.model_entitlements(&a, 1);
        assert_eq!(denied[0].status, AccountModelEntitlementStatus::Forbidden);
        assert_eq!(denied[0].retry_at_ms, Some(300_000));
        assert_eq!(
            doc.model_entitlements(&a, 300_000)[0].status,
            AccountModelEntitlementStatus::Unverified
        );
        let revision = match doc.apply(claim(&a, 300_000, "probe")) {
            PoolResult::Claimed { revision } => revision,
            _ => panic!("expired model denial must permit one real probe"),
        };
        doc.apply(finish(&a, "probe", revision, None));
        assert_eq!(
            doc.model_entitlements(&a, 300_001)[0].status,
            AccountModelEntitlementStatus::Confirmed
        );
        let mut replaced = a.clone();
        replaced.identity_generation += 1;
        assert!(doc.model_entitlements(&replaced, 300_001).is_empty());
    }
    fn claim(a: &AccountMetadata, now: u64, owner: &str) -> PoolOperation {
        PoolOperation::Claim {
            account: a.clone(),
            model: "m".into(),
            endpoint: "endpoint".into(),
            now_ms: now,
            owner: owner.into(),
        }
    }
    fn finish(
        a: &AccountMetadata,
        owner: &str,
        revision: u64,
        failure: Option<AccountFailure>,
    ) -> PoolOperation {
        PoolOperation::Finish {
            account: a.clone(),
            model: "m".into(),
            endpoint: "endpoint".into(),
            owner: owner.into(),
            claimed_revision: revision,
            credential_revision: None,
            outcome: failure,
            now_ms: 0,
        }
    }
    #[test]
    fn cooling_probe_lease_reclaims_and_rejects_old_owner() {
        let a = account();
        let mut doc = HealthDocument::default();
        doc.apply(claim(&a, 0, "original"));
        doc.apply(finish(
            &a,
            "original",
            0,
            classify(
                &LlmError::RateLimited {
                    retry_after: None,
                    scope: None,
                },
                "m",
                "endpoint",
                0,
            ),
        ));
        assert!(matches!(
            doc.apply(claim(&a, 29_999, "early")),
            PoolResult::Unavailable(_)
        ));
        let revision = match doc.apply(claim(&a, 30_000, "probe")) {
            PoolResult::Claimed { revision } => revision,
            _ => panic!("probe"),
        };
        assert!(matches!(
            doc.apply(claim(&a, 30_001, "other")),
            PoolResult::Unavailable(_)
        ));
        let new_revision = match doc.apply(claim(&a, 60_001, "new")) {
            PoolResult::Claimed { revision } => revision,
            _ => panic!("reclaimed probe"),
        };
        assert!(matches!(
            doc.apply(finish(&a, "probe", revision, None)),
            PoolResult::Stale
        ));
        assert!(matches!(
            doc.apply(finish(&a, "new", new_revision, None)),
            PoolResult::Recorded { .. }
        ));
        assert!(
            matches!(
                doc.apply(finish(
                    &a,
                    "probe",
                    revision,
                    classify(
                        &LlmError::Authentication {
                            message: String::new()
                        },
                        "m",
                        "endpoint",
                        0
                    )
                )),
                PoolResult::Stale
            ),
            "old probe stays fenced after new success removes the fault row"
        );
        assert!(matches!(
            doc.apply(claim(&a, 60_002, "ready")),
            PoolResult::Claimed { .. }
        ));
    }
    #[tokio::test]
    async fn fifty_concurrent_requests_have_exactly_one_recovery_probe() {
        let store = Arc::new(MemoryAccountPoolStore::default());
        let a = account();
        store.transact(claim(&a, 0, "first")).await.unwrap();
        store
            .transact(finish(
                &a,
                "first",
                0,
                classify(
                    &LlmError::RateLimited {
                        retry_after: None,
                        scope: None,
                    },
                    "m",
                    "endpoint",
                    0,
                ),
            ))
            .await
            .unwrap();
        let mut tasks = Vec::new();
        for id in 0..50 {
            let store = store.clone();
            let a = a.clone();
            tasks.push(tokio::spawn(async move {
                matches!(
                    store
                        .transact(claim(&a, 30_000, &id.to_string()))
                        .await
                        .unwrap(),
                    PoolResult::Claimed { .. }
                )
            }));
        }
        let mut claimed = 0;
        for task in tasks {
            claimed += usize::from(task.await.unwrap());
        }
        assert_eq!(claimed, 1);
    }
    #[test]
    fn old_generation_and_concurrent_old_success_cannot_clear_new_failures() {
        let a = account();
        let mut doc = HealthDocument::default();
        doc.apply(claim(&a, 0, "old"));
        doc.apply(finish(
            &a,
            "failure",
            0,
            classify(&LlmError::QuotaExceeded, "m", "endpoint", 0),
        ));
        assert!(matches!(
            doc.apply(finish(&a, "old", 0, None)),
            PoolResult::Stale
        ));
        let mut replacement = a.clone();
        replacement.identity_generation = 2;
        replacement.config_revision = 2;
        assert!(matches!(
            doc.apply(claim(&replacement, 1, "new")),
            PoolResult::Claimed { .. }
        ));
        assert!(matches!(
            doc.apply(finish(
                &a,
                "old",
                0,
                classify(
                    &LlmError::Authentication {
                        message: String::new()
                    },
                    "m",
                    "endpoint",
                    0
                )
            )),
            PoolResult::Stale
        ));
    }
    #[test]
    fn rate_limits_scope_models_and_preserve_explicit_long_retry_after() {
        let a = account();
        let mut doc = HealthDocument::default();
        let failure = classify(
            &LlmError::RateLimited {
                retry_after: Some(std::time::Duration::from_secs(3600)),
                scope: None,
            },
            "m",
            "endpoint",
            0,
        );
        doc.apply(finish(&a, "a", 0, failure));
        let other = PoolOperation::Claim {
            account: a.clone(),
            model: "other".into(),
            endpoint: "endpoint".into(),
            now_ms: 1000,
            owner: "other".into(),
        };
        assert!(matches!(doc.apply(other), PoolResult::Claimed { .. }));
        assert!(matches!(
            doc.apply(claim(&a, 600_001, "same")),
            PoolResult::Unavailable(AccountUnavailable {
                recovery_at_ms: Some(3_600_000),
                ..
            })
        ));
    }
    #[test]
    fn storage_unavailable_does_not_mean_needs_auth_or_transport_replay() {
        assert_eq!(
            classify(&LlmError::AccountStorageUnavailable, "m", "e", 0)
                .unwrap()
                .status,
            HealthStatus::StorageUnavailable
        );
        assert!(classify(
            &LlmError::Transport {
                message: "unknown remote outcome".into()
            },
            "m",
            "e",
            0
        )
        .is_none());
    }
}

#[async_trait::async_trait]
pub trait AccountRoutingReporter: Send + Sync {
    async fn report(
        &self,
        event: lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent,
    );
}
impl AccountUnavailable {
    pub fn as_public_failure(
        &self,
    ) -> lingxi_core::host::provider_accounts::ProviderAccountFailure {
        let (scope_kind, scope_id) = match &self.scope {
            FailureScope::Account => ("account", None),
            FailureScope::Model(model) => ("model", Some(model.clone())),
            FailureScope::Endpoint(endpoint) => ("endpoint", Some(endpoint.clone())),
        };
        lingxi_core::host::provider_accounts::ProviderAccountFailure {
            account_id: self.account_id.clone(),
            reason_code: self.reason.clone(),
            scope_kind: scope_kind.into(),
            scope_id,
            retryable: self.retryable,
            recovery_at_ms: self.recovery_at_ms,
        }
    }
}
impl AttemptCoordinator {
    pub fn routing_event(
        &self,
        logical_call_id: String,
        profile: Option<String>,
    ) -> lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent {
        let active = self.active.as_ref().map(|(account, _, _, _)| account);
        lingxi_core::host::provider_accounts::ProviderAccountRoutingEvent {
            logical_call_id,
            model: self.model.clone(),
            attempt: self.visited.len() as u32,
            status: if active.is_some() {
                "selected"
            } else {
                "all_unavailable"
            }
            .into(),
            selected_account_id: active.map(|account| account.account_id.clone()),
            profile,
            identity_generation: active.map(|account| account.identity_generation),
            config_revision: active.map(|account| account.config_revision),
            failed_accounts: self
                .unavailable
                .iter()
                .map(AccountUnavailable::as_public_failure)
                .collect(),
            earliest_recovery_ms: self
                .unavailable
                .iter()
                .filter_map(|failure| failure.recovery_at_ms)
                .min(),
        }
    }
}
