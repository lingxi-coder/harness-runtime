//! Pure account directory validation and compare-and-swap mutations.
use lingxi_core::host::provider_accounts::{AccountMetadata, AccountSnapshot};
use std::collections::HashSet;

/// In-memory account snapshot with revision-checked metadata mutations.
#[derive(Debug, Clone)]
pub struct AccountRegistry {
    snapshot: AccountSnapshot,
}
impl AccountRegistry {
    /// Create an empty registry for a credential store scope at revision zero.
    pub fn new(store_scope: String) -> Self {
        Self {
            snapshot: AccountSnapshot {
                provider_policies: Default::default(),
                store_scope,
                config_revision: 0,
                accounts: vec![],
                health: vec![],
                model_entitlements: vec![],
                model_availability: vec![],
                scoped_health: vec![],
            },
        }
    }
    /// Validate account identities and credential references in a snapshot.
    pub fn from_snapshot(snapshot: AccountSnapshot) -> Result<Self, String> {
        let mut ids = HashSet::new();
        let mut refs = HashSet::new();
        for account in &snapshot.accounts {
            validate(account, &snapshot.store_scope)?;
            if !ids.insert(&account.account_id) || !refs.insert(&account.credential_ref.secret_ref)
            {
                return Err("duplicate account identity or credential reference".into());
            }
        }
        Ok(Self { snapshot })
    }
    /// Return a copy of the current account snapshot.
    pub fn snapshot(&self) -> AccountSnapshot {
        self.snapshot.clone()
    }
    /// Reject a mutation when its expected configuration revision is stale.
    pub fn check_revision(&self, expected: u64) -> Result<(), String> {
        if self.snapshot.config_revision != expected {
            return Err(format!(
                "account configuration conflict: expected {expected}, current {}",
                self.snapshot.config_revision
            ));
        }
        Ok(())
    }
    /// Add a validated account at the next provider priority and revision.
    pub fn add(&mut self, mut account: AccountMetadata, expected: u64) -> Result<(), String> {
        self.check_revision(expected)?;
        validate(&account, &self.snapshot.store_scope)?;
        if self.snapshot.accounts.iter().any(|a| {
            a.account_id == account.account_id || a.credential_ref == account.credential_ref
        }) {
            return Err("account already exists".into());
        }
        account.user_priority = self
            .snapshot
            .accounts
            .iter()
            .filter(|a| a.provider_id == account.provider_id)
            .map(|a| a.user_priority)
            .max()
            .map_or(0, |n| n.saturating_add(1));
        account.config_revision = self.advance()?;
        self.snapshot.accounts.push(account);
        Ok(())
    }
    /// Update account metadata while preserving its bound route and priority.
    pub fn update(&mut self, account: AccountMetadata, expected: u64) -> Result<(), String> {
        self.check_revision(expected)?;
        validate(&account, &self.snapshot.store_scope)?;
        let index = self
            .snapshot
            .accounts
            .iter()
            .position(|a| a.account_id == account.account_id)
            .ok_or("unknown account")?;
        let old = &self.snapshot.accounts[index];
        if old.provider_id != account.provider_id
            || old.credential_ref != account.credential_ref
            || old.execution_binding_id != account.execution_binding_id
            || old.auth_kind != account.auth_kind
            || old.user_priority != account.user_priority
        {
            return Err("immutable account identity or priority changed".into());
        }
        let revision = self.advance()?;
        self.snapshot.accounts[index] = AccountMetadata {
            config_revision: revision,
            ..account
        };
        Ok(())
    }
    /// Replace a provider's metered-fallback policy at the expected revision.
    pub fn set_policy(
        &mut self,
        provider_id: String,
        allow_subscription_to_metered: bool,
        expected: u64,
    ) -> Result<(), String> {
        self.check_revision(expected)?;
        if provider_id.trim().is_empty() {
            return Err("provider required".into());
        }
        let revision = self.advance()?;
        for account in self
            .snapshot
            .accounts
            .iter_mut()
            .filter(|account| account.provider_id == provider_id)
        {
            account.config_revision = revision;
        }
        self.snapshot.provider_policies.insert(
            provider_id,
            lingxi_core::host::provider_accounts::ProviderAccountPolicy {
                allow_subscription_to_metered,
            },
        );
        Ok(())
    }
    /// Remove account metadata and matching entries from the snapshot's health list.
    ///
    /// Credential storage is managed separately by the caller.
    pub fn remove(&mut self, account_id: &str, expected: u64) -> Result<AccountMetadata, String> {
        self.check_revision(expected)?;
        let index = self
            .snapshot
            .accounts
            .iter()
            .position(|a| a.account_id == account_id)
            .ok_or("unknown account")?;
        self.advance()?;
        self.snapshot.health.retain(|h| h.account_id != account_id);
        Ok(self.snapshot.accounts.remove(index))
    }
    /// Set provider priorities from a list containing each of its accounts once.
    pub fn reorder(
        &mut self,
        provider_id: &str,
        ids: &[String],
        expected: u64,
    ) -> Result<(), String> {
        self.check_revision(expected)?;
        let actual: HashSet<_> = self
            .snapshot
            .accounts
            .iter()
            .filter(|a| a.provider_id == provider_id)
            .map(|a| a.account_id.as_str())
            .collect();
        let requested: HashSet<_> = ids.iter().map(String::as_str).collect();
        if ids.len() != requested.len() || actual != requested {
            return Err("reorder must contain every provider account exactly once".into());
        }
        let revision = self.advance()?;
        for (priority, id) in ids.iter().enumerate() {
            let account = self
                .snapshot
                .accounts
                .iter_mut()
                .find(|a| a.account_id == *id)
                .ok_or("unknown account")?;
            account.user_priority = u32::try_from(priority).map_err(|_| "too many accounts")?;
            account.config_revision = revision;
        }
        Ok(())
    }
    fn advance(&mut self) -> Result<u64, String> {
        self.snapshot.config_revision = self
            .snapshot
            .config_revision
            .checked_add(1)
            .ok_or("revision exhausted")?;
        Ok(self.snapshot.config_revision)
    }
}
fn validate(a: &AccountMetadata, scope: &str) -> Result<(), String> {
    if uuid::Uuid::parse_str(&a.account_id).map_or(true, |id| id.to_string() != a.account_id)
        || a.credential_ref.secret_ref != format!("acct.{}", a.account_id)
    {
        return Err("invalid account identity or credential slot".into());
    }
    if a.credential_ref.store_scope != scope
        || a.provider_id.trim().is_empty()
        || a.display_name.trim().is_empty()
        || a.execution_binding_id.trim().is_empty()
        || a.identity_generation == 0
    {
        return Err("invalid account metadata".into());
    }
    Ok(())
}

/// Derive account routes from declared execution bindings, with exact credentials and no ambient fallback.
pub fn assemble_account_profiles(
    assembled: &mut crate::Assembled,
    accounts: &[AccountMetadata],
) -> Result<(), String> {
    assemble_account_client_config(&mut assembled.client_config, accounts)?;
    assembled
        .credential_sources
        .retain(|s| !s.profile_name.starts_with("account."));
    for account in accounts.iter().filter(|a| a.enabled) {
        let Some(profile) = assembled
            .client_config
            .providers
            .iter()
            .find(|p| p.profile_name == format!("account.{}", account.account_id))
        else {
            continue;
        };
        assembled.credential_sources.push(crate::CredentialSource {
            provider_id: profile.provider_id.clone(),
            profile_name: profile.profile_name.clone(),
            credential_id: account.credential_ref.secret_ref.clone(),
            env_var: None,
            kind: match account.auth_kind {
                lingxi_core::host::provider_accounts::AccountAuthKind::ApiKey => {
                    crate::CredentialKind::Keychain
                }
                lingxi_core::host::provider_accounts::AccountAuthKind::OAuth => {
                    crate::CredentialKind::OAuth
                }
            },
        });
    }
    assembled.pricing = crate::cost_translate::pricing_for(&assembled.client_config.providers);
    Ok(())
}
/// Idempotently derive executable account routes from authoritative existing bindings.
pub fn assemble_account_client_config(
    config: &mut llm_runtime::ClientConfig,
    accounts: &[AccountMetadata],
) -> Result<(), String> {
    config
        .providers
        .retain(|p| !p.profile_name.starts_with("account."));
    for account in accounts.iter().filter(|a| a.enabled) {
        let Some(mut profile) = config
            .providers
            .iter()
            .find(|p| p.profile_name == account.execution_binding_id)
            .cloned()
        else {
            continue;
        };
        profile.profile_name = format!("account.{}", account.account_id);
        profile.credential = llm_runtime::CredentialConfig::Static {
            id: account.credential_ref.secret_ref.clone(),
        };
        profile.connection.group = Some(account.provider_id.clone());
        profile.connection.connection_id = Some(account.account_id.clone());
        profile.connection.order = account.user_priority;
        profile.connection.hidden = true;
        config.providers.push(profile);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{AccountAuthKind, CredentialRef};
    fn account(scope: &str) -> AccountMetadata {
        let id = uuid::Uuid::new_v4().to_string();
        AccountMetadata {
            account_id: id.clone(),
            provider_id: "openai".into(),
            display_name: "Test".into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "openai".into(),
            credential_ref: CredentialRef {
                store_scope: scope.into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 0,
            identity_generation: 1,
        }
    }
    #[test]
    fn revisions_and_exact_reorder() {
        let mut r = AccountRegistry::new("test".into());
        let a = account("test");
        let b = account("test");
        r.add(a.clone(), 0).unwrap();
        r.add(b.clone(), 1).unwrap();
        assert!(r.reorder("openai", &[a.account_id.clone()], 2).is_err());
        assert!(r
            .reorder("openai", &[a.account_id.clone(), a.account_id.clone()], 2)
            .is_err());
        r.reorder("openai", &[b.account_id.clone(), a.account_id.clone()], 2)
            .unwrap();
        assert_eq!(r.snapshot.accounts[0].user_priority, 1);
        assert!(r.remove(&a.account_id, 2).is_err());
        assert_eq!(r.snapshot.config_revision, 3);
    }
    #[test]
    fn scope_and_identity_are_validated() {
        let mut r = AccountRegistry::new("test".into());
        assert!(r.add(account("other"), 0).is_err());
        assert_eq!(r.snapshot.config_revision, 0);
    }
}
