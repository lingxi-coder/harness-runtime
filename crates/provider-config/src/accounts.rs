//! Pure account directory validation and compare-and-swap mutations.
use lingxi_core::host::provider_accounts::{
    AccountMetadata, AccountSnapshot, ADDITIONAL_OPENAI_MODEL_IDS,
};
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
    /// Consume the registry, retaining the validated directory without cloning its rows.
    pub fn into_snapshot(self) -> AccountSnapshot {
        self.snapshot
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
fn is_openai_api_profile(profile: &llm_runtime::ProviderProfile) -> bool {
    matches!(profile.profile_name.as_str(), "openai" | "openai-chat")
        && profile.provider_id == llm_runtime::ProviderId::OpenAI
        && profile.base_url.trim_end_matches('/') == "https://api.openai.com/v1"
}

fn project_additional_openai_models(profile: &mut llm_runtime::ProviderProfile, enabled: bool) {
    // https://developers.openai.com/api/docs/deprecations#past-deprecations
    const RETIRED: &[&str] = &["gpt-4.1-nano", "o1", "o3-mini", "o4-mini"];
    let is_additional = |id: &str| ADDITIONAL_OPENAI_MODEL_IDS.contains(&id);
    let keep = |id: &str| !RETIRED.contains(&id) && (enabled || !is_additional(id));
    profile.models.retain(|model| keep(&model.request_model));
    if let Some(wire) = &mut profile.wire_profile {
        wire.models.retain(|model| keep(&model.request_model));
    }
    if !enabled {
        return;
    }

    static BUILTINS: std::sync::OnceLock<llm_runtime::BuiltinCatalog> = std::sync::OnceLock::new();
    let Some(source) = BUILTINS
        .get_or_init(llm_runtime::builtin_presets)
        .providers
        .iter()
        .find(|source| {
            source.profile_name == profile.profile_name && source.protocol == profile.protocol
        })
    else {
        return;
    };

    for model in source
        .models
        .iter()
        .filter(|model| is_additional(&model.request_model) && keep(&model.request_model))
    {
        if let Some(existing) = profile
            .models
            .iter_mut()
            .find(|existing| existing.request_model == model.request_model)
        {
            existing.metadata.catalog_offered = true;
        } else {
            let mut model = model.clone();
            model.metadata.catalog_offered = true;
            profile.models.push(model);
        }
    }
    if let (Some(wire), Some(source)) = (&mut profile.wire_profile, &source.wire_profile) {
        for model in source
            .models
            .iter()
            .filter(|model| is_additional(&model.request_model) && keep(&model.request_model))
        {
            if !wire
                .models
                .iter()
                .any(|existing| existing.request_model == model.request_model)
            {
                wire.models.push(model.clone());
            }
        }
    }
}

/// Idempotently derive executable account routes from authoritative existing bindings.
pub fn assemble_account_client_config(
    config: &mut llm_runtime::ClientConfig,
    accounts: &[AccountMetadata],
) -> Result<(), String> {
    config
        .providers
        .retain(|p| !p.profile_name.starts_with("account."));
    let additional_models_enabled = accounts.iter().any(|account| {
        account.enabled
            && account.supports_additional_openai_models()
            && account.additional_openai_models
    });
    for profile in config
        .providers
        .iter_mut()
        .filter(|profile| is_openai_api_profile(profile))
    {
        project_additional_openai_models(profile, additional_models_enabled);
    }
    for account in accounts.iter().filter(|a| a.enabled) {
        let Some(binding) = config
            .providers
            .iter()
            .find(|p| p.profile_name == account.execution_binding_id)
        else {
            continue;
        };
        // Materialize the configured default on the account itself. A link
        // back to a catalog profile would carry its catalog credential.
        let alternate = binding.wire_profile.as_ref().and_then(|wire| {
            let protocol = wire
                .connection
                .default_protocol
                .filter(|p| *p != binding.protocol)?;
            let name = wire.connection.protocol_profiles.get(&protocol)?;
            config.providers.iter().find(|target| {
                &target.profile_name == name
                    && target.protocol == protocol
                    && target.provider_id == binding.provider_id
                    && target.credential == binding.credential
                    && target.pricing.billing_mode == binding.pricing.billing_mode
            })
        });
        let mut profile = alternate.unwrap_or(binding).clone();
        if alternate.is_some() {
            if let Some(wire) = &mut profile.wire_profile {
                wire.connection.default_protocol = None;
                wire.connection.protocol_profiles.clear();
            }
        }
        if additional_models_enabled
            && is_openai_api_profile(&profile)
            && !(account.supports_additional_openai_models() && account.additional_openai_models)
        {
            project_additional_openai_models(&mut profile, false);
        }
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
            additional_openai_models: false,
            user_priority: 0,
            config_revision: 0,
            identity_generation: 1,
        }
    }
    #[test]
    fn additional_openai_models_follow_account_opt_in() {
        let mut opted_in = account("test");
        let opted_out = account("test");
        let mut config = llm_runtime::ClientConfig {
            providers: llm_runtime::builtin_presets()
                .providers
                .into_iter()
                .filter(|profile| profile.profile_name == "openai")
                .collect(),
        };

        assemble_account_client_config(&mut config, &[opted_in.clone(), opted_out.clone()])
            .unwrap();
        assert!(config.providers.iter().all(|profile| profile
            .models
            .iter()
            .all(|model| !ADDITIONAL_OPENAI_MODEL_IDS.contains(&model.request_model.as_str()))));

        opted_in.additional_openai_models = true;
        let mut registry = AccountRegistry::new("test".into());
        registry.add(opted_in.clone(), 0).unwrap();
        registry.add(opted_out.clone(), 1).unwrap();
        let mut snapshot = registry.snapshot();
        use lingxi_core::host::provider_accounts::{AccountHealth, AccountHealthStatus};
        snapshot.health = snapshot
            .accounts
            .iter()
            .map(|account| AccountHealth {
                account_id: account.account_id.clone(),
                identity_generation: account.identity_generation,
                status: AccountHealthStatus::Ready,
                reason: None,
                cooldown_until_ms: None,
                health_revision: 0,
            })
            .collect();
        assemble_account_client_config(&mut config, &snapshot.accounts).unwrap();
        let rows =
            llm_runtime::ModelRuntime::project_account_model_availability(&config, &snapshot, 0)
                .unwrap();
        for model in ADDITIONAL_OPENAI_MODEL_IDS {
            let row = rows.iter().find(|row| row.model_id == *model);
            if ["gpt-4.1-nano", "o1", "o3-mini", "o4-mini"].contains(model) {
                assert!(row.is_none(), "retired model {model}");
            } else {
                assert_eq!(
                    row.unwrap().eligible_account_ids,
                    [opted_in.account_id.clone()],
                    "{model}"
                );
            }
        }

        opted_in.additional_openai_models = false;
        assemble_account_client_config(&mut config, &[opted_in, opted_out]).unwrap();
        assert!(config.providers.iter().all(|profile| profile
            .models
            .iter()
            .all(|model| !ADDITIONAL_OPENAI_MODEL_IDS.contains(&model.request_model.as_str()))));
        assert!(!config.providers[0].models.is_empty());
    }

    #[test]
    fn revisions_and_exact_reorder() {
        let mut r = AccountRegistry::new("test".into());
        let a = account("test");
        let b = account("test");
        r.add(a.clone(), 0).unwrap();
        r.add(b.clone(), 1).unwrap();
        assert!(r
            .reorder("openai", std::slice::from_ref(&a.account_id), 2)
            .is_err());
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
