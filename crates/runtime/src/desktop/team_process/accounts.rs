//! Account-only credentials carried exclusively over the trusted Team pipes.
use super::TeamProcessError;
use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, AccountSnapshot};
use secret::{AccountCredential, AccountCredentialKind, CredentialManager};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

/// No Debug: authentication material never enters diagnostics or command arguments.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TeamAccountEnvelope {
    pub(crate) provider_accounts: AccountSnapshot,
    pub(crate) provider_account_home: PathBuf,
    pub(crate) provider_account_profiles: Vec<lingxi_llm_client::protocol::ProviderProfile>,
    #[serde(default)]
    pub(crate) provider_account_credentials: BTreeMap<String, AccountCredential>,
}
impl TeamAccountEnvelope {
    pub(crate) fn clone_private(&self) -> Result<Self, TeamProcessError> {
        let bytes = lingxi_core::types::Secret::new(
            serde_json::to_vec(self).map_err(|_| TeamProcessError::Protocol)?,
        );
        serde_json::from_slice(bytes.expose_secret()).map_err(|_| TeamProcessError::Protocol)
    }
    /// Trusted parent verifies the live owner registry, then materializes only
    /// the selected account pool under the private Bot×Chat home. The worker
    /// never receives the owner's registry path or unrelated account metadata.
    pub(crate) fn for_worker(&self, home: &std::path::Path) -> Result<Self, TeamProcessError> {
        self.validate()?;
        if home.canonicalize()? != home {
            return Err(TeamProcessError::Model);
        }
        let mut private = self.clone_private()?;
        let names = private
            .provider_account_profiles
            .iter()
            .map(|p| p.profile_name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        private
            .provider_accounts
            .accounts
            .retain(|a| names.contains(a.execution_binding_id.as_str()));
        let ids = private
            .provider_accounts
            .accounts
            .iter()
            .map(|a| a.account_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let providers = private
            .provider_accounts
            .accounts
            .iter()
            .map(|a| a.provider_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        private
            .provider_accounts
            .provider_policies
            .retain(|id, _| providers.contains(id));
        private
            .provider_accounts
            .health
            .retain(|a| ids.contains(&a.account_id));
        private
            .provider_accounts
            .model_entitlements
            .retain(|a| ids.contains(&a.account_id));
        private
            .provider_accounts
            .scoped_health
            .retain(|a| ids.contains(&a.account_id));
        private
            .provider_accounts
            .model_availability
            .retain(|a| names.contains(a.execution_binding_id.as_str()));
        for row in &mut private.provider_accounts.model_availability {
            row.eligible_account_ids.retain(|id| ids.contains(id));
            row.confirmed_account_ids.retain(|id| ids.contains(id));
            row.unverified_account_ids.retain(|id| ids.contains(id));
            row.forbidden_account_ids.retain(|id| ids.contains(id));
        }
        private.provider_account_home = home.to_owned();
        let scope = &private.provider_accounts.store_scope;
        let _lock = lingxi_core::host::rooted_fs::lock_exclusive(
            home,
            &PathBuf::from(format!("provider-accounts/{scope}.lock")),
            0o700,
            0o600,
        )
        .map_err(|_| TeamProcessError::Model)?;
        lingxi_core::host::rooted_fs::atomic_write(
            home,
            &PathBuf::from(format!("provider-accounts/{scope}.json")),
            &serde_json::to_vec(&private.provider_accounts).map_err(|_| TeamProcessError::Model)?,
            Default::default(),
        )
        .map_err(|_| TeamProcessError::Model)?;
        for account in &private.provider_accounts.accounts {
            if let Some(record) = private
                .provider_account_credentials
                .get(&account.account_id)
            {
                provider_config::write_account_credential_version_locked(
                    home,
                    account,
                    record.credential_revision,
                )
                .map_err(|_| TeamProcessError::Model)?;
            }
        }
        Ok(private)
    }
    pub(crate) fn validate(&self) -> Result<(), TeamProcessError> {
        if !self.provider_account_home.is_absolute() {
            return Err(TeamProcessError::Model);
        }
        provider_config::AccountRegistry::from_snapshot(self.provider_accounts.clone())
            .map_err(|_| TeamProcessError::Model)?;
        let current = crate::provider_accounts::load_account_snapshot(
            &self.provider_account_home,
            &self.provider_accounts.store_scope,
        )
        .map_err(|_| TeamProcessError::Model)?;
        if current.config_revision != self.provider_accounts.config_revision
            || current.accounts != self.provider_accounts.accounts
            || current.provider_policies != self.provider_accounts.provider_policies
        {
            return Err(TeamProcessError::Model);
        }
        self.validate_scope()?;
        for (id, record) in &self.provider_account_credentials {
            let account = self
                .provider_accounts
                .accounts
                .iter()
                .find(|a| &a.account_id == id)
                .ok_or(TeamProcessError::Model)?;
            if record.identity_generation != account.identity_generation
                || record.credential_revision == 0
            {
                return Err(TeamProcessError::Model);
            }
            let token = match (&account.auth_kind, &record.kind) {
                (AccountAuthKind::ApiKey, AccountCredentialKind::ApiKey { key }) => {
                    key.expose_secret()
                }
                (
                    AccountAuthKind::OAuth,
                    AccountCredentialKind::OAuth {
                        access_token,
                        refresh_token: None,
                        ..
                    },
                ) => access_token.expose_secret(),
                _ => return Err(TeamProcessError::Model),
            };
            if token.is_empty() || token.len() > 16384 || token.chars().any(char::is_control) {
                return Err(TeamProcessError::Model);
            }
        }
        Ok(())
    }
    fn validate_scope(&self) -> Result<(), TeamProcessError> {
        let mut names = std::collections::BTreeSet::new();
        let mut logical: Option<&str> = None;
        if self.provider_account_profiles.is_empty() {
            return Err(TeamProcessError::Model);
        }
        for profile in &self.provider_account_profiles {
            if !names.insert(profile.profile_name.as_str()) || profile.models.len() != 1 {
                return Err(TeamProcessError::Model);
            }
            let route = super::TeamModelRoute::for_account(
                profile.clone(),
                &profile.models[0].request_model,
            )?;
            if route.model_profile != *profile {
                return Err(TeamProcessError::Model);
            }
            let anchor = self
                .provider_accounts
                .accounts
                .iter()
                .find(|a| a.execution_binding_id == profile.profile_name)
                .ok_or(TeamProcessError::Model)?;
            if logical.is_some_and(|id| id != anchor.provider_id) {
                return Err(TeamProcessError::Model);
            }
            logical = Some(&anchor.provider_id);
        }
        for id in self.provider_account_credentials.keys() {
            let account = self
                .provider_accounts
                .accounts
                .iter()
                .find(|a| &a.account_id == id)
                .ok_or(TeamProcessError::Model)?;
            if Some(account.provider_id.as_str()) != logical
                || !names.contains(account.execution_binding_id.as_str())
            {
                return Err(TeamProcessError::Model);
            }
        }
        Ok(())
    }
    pub(crate) async fn seed(
        &self,
        credentials: &Arc<CredentialManager>,
    ) -> Result<(), TeamProcessError> {
        self.validate()?;
        credentials.use_host_managed_provider_keys();
        let mut records = self.clone_private()?.provider_account_credentials;
        for account in self.provider_accounts.accounts.iter().filter(|a| {
            self.provider_account_profiles
                .iter()
                .any(|p| p.profile_name == a.execution_binding_id)
        }) {
            let incoming = records.remove(&account.account_id);
            if let (Some(existing), Some(incoming)) = (
                credentials
                    .get_account_credential(&account.credential_ref)
                    .await
                    .map_err(|_| TeamProcessError::Model)?,
                incoming.as_ref(),
            ) {
                if existing.identity_generation > incoming.identity_generation
                    || (existing.identity_generation == incoming.identity_generation
                        && existing.credential_revision > incoming.credential_revision)
                {
                    continue;
                }
                if existing.identity_generation == incoming.identity_generation
                    && existing.credential_revision == incoming.credential_revision
                {
                    let old = lingxi_core::types::Secret::new(
                        serde_json::to_vec(&existing).map_err(|_| TeamProcessError::Model)?,
                    );
                    let new = lingxi_core::types::Secret::new(
                        serde_json::to_vec(incoming).map_err(|_| TeamProcessError::Model)?,
                    );
                    if old.expose_secret() != new.expose_secret() {
                        return Err(TeamProcessError::Model);
                    }
                    continue;
                }
            }
            credentials
                .set_account_credential_ephemeral(&account.credential_ref, incoming)
                .await
                .map_err(|_| TeamProcessError::Model)?;
        }
        Ok(())
    }
    pub(crate) fn supports(&self, route: &super::TeamModelRoute) -> bool {
        self.validate_scope().is_ok()
            && self
                .provider_account_profiles
                .iter()
                .all(|profile| profile.models[0].request_model == route.model_id)
            && self
                .provider_account_profiles
                .iter()
                .any(|profile| profile == &route.model_profile)
            && self.provider_accounts.accounts.iter().any(|a| {
                a.enabled
                    && a.execution_binding_id == route.model_provider
                    && self
                        .provider_account_credentials
                        .contains_key(&a.account_id)
            })
    }
    pub(crate) fn requested_account(
        &self,
        account: &AccountMetadata,
        revision: u64,
    ) -> Result<(), TeamProcessError> {
        let selected = self
            .provider_accounts
            .accounts
            .iter()
            .find(|a| {
                a.account_id == account.account_id
                    && a.identity_generation == account.identity_generation
                    && a.credential_ref == account.credential_ref
                    && a.enabled
            })
            .ok_or(TeamProcessError::Model)?;
        let record = self
            .provider_account_credentials
            .get(&selected.account_id)
            .ok_or(TeamProcessError::Model)?;
        if record.identity_generation != account.identity_generation
            || record.credential_revision <= revision
        {
            return Err(TeamProcessError::Model);
        }
        Ok(())
    }
}

pub(super) struct WorkerAccountRefresher {
    pub credentials: std::sync::Weak<CredentialManager>,
    pub bridge: Arc<super::interaction::ChildInteraction>,
    pub registry: crate::desktop::FusionCatalogRegistry,
    pub home: PathBuf,
    pub scope: String,
    pub profiles: Vec<lingxi_llm_client::protocol::ProviderProfile>,
    pub locks: tokio::sync::Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}
#[async_trait::async_trait]
impl secret::AccountCredentialRefresher for WorkerAccountRefresher {
    async fn refresh(
        &self,
        account: AccountMetadata,
        revision: u64,
    ) -> Result<Option<AccountCredential>, secret::CredentialError> {
        let credentials = self
            .credentials
            .upgrade()
            .ok_or(secret::CredentialError::Unavailable)?;
        let lock = self
            .locks
            .lock()
            .await
            .entry(account.account_id.clone())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if let Some(record) = credentials
            .get_account_credential(&account.credential_ref)
            .await?
        {
            if record.identity_generation == account.identity_generation
                && record.credential_revision > revision
            {
                return Ok(Some(record));
            }
        }
        let envelope = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.bridge.refresh_provider_account(&account, revision),
        )
        .await
        .map_err(|_| secret::CredentialError::Unavailable)?
        .map_err(|_| secret::CredentialError::Unavailable)?;
        if envelope.provider_account_home != self.home
            || envelope.provider_accounts.store_scope != self.scope
            || envelope.provider_account_profiles != self.profiles
        {
            return Err(secret::CredentialError::Unavailable);
        }
        envelope
            .validate()
            .map_err(|_| secret::CredentialError::Unavailable)?;
        envelope
            .requested_account(&account, revision)
            .map_err(|_| secret::CredentialError::Unavailable)?;
        envelope
            .seed(&credentials)
            .await
            .map_err(|_| secret::CredentialError::Unavailable)?;
        self.registry
            .refresh_provider_accounts(envelope.provider_accounts)
            .map_err(|_| secret::CredentialError::Unavailable)?;
        credentials
            .get_account_credential(&account.credential_ref)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::provider_accounts::{AccountAuthKind, CredentialRef};
    use lingxi_core::types::Secret;

    fn fixture() -> (tempfile::TempDir, TeamAccountEnvelope) {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().canonicalize().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let account = AccountMetadata {
            account_id: id.clone(),
            provider_id: "openai".into(),
            display_name: "Fixture".into(),
            auth_kind: AccountAuthKind::ApiKey,
            execution_binding_id: "openai".into(),
            credential_ref: CredentialRef {
                store_scope: "fixture_team".into(),
                secret_ref: format!("acct.{id}"),
            },
            enabled: true,
            user_priority: 0,
            config_revision: 0,
            identity_generation: 1,
        };
        let mut registry = provider_config::AccountRegistry::new("fixture_team".into());
        registry.add(account, 0).unwrap();
        let snapshot = registry.snapshot();
        std::fs::create_dir(home.join("provider-accounts")).unwrap();
        std::fs::write(
            home.join("provider-accounts/fixture_team.json"),
            serde_json::to_vec(&snapshot).unwrap(),
        )
        .unwrap();
        let envelope = TeamAccountEnvelope {
            provider_accounts: snapshot,
            provider_account_home: home,
            provider_account_profiles: vec![super::super::TeamModelRoute::for_account(serde_json::from_value(serde_json::json!({
                "provider_id":"openai","profile_name":"openai","base_url":"https://api.openai.com/v1","protocol":"open_ai_responses","auth":"api_key", "credential":{"source":"host_managed","key":"team-selected-model"},
                "models":[{"display_model":"fixture","request_model":"fixture","billing_model":"fixture","capability_support":{"streaming":"supported","tools":"supported"}}],"extra":{}
            })).unwrap(),"fixture").unwrap().model_profile],
            provider_account_credentials: BTreeMap::from([(
                id,
                AccountCredential {
                    credential_revision: 1,
                    identity_generation: 1,
                    kind: AccountCredentialKind::ApiKey {
                        key: Secret::new("synthetic-team-account".into()),
                    },
                },
            )]),
        };
        (directory, envelope)
    }

    #[test]
    fn team_envelope_rejects_stale_metadata_and_refresh_material() {
        let (_dir, mut envelope) = fixture();
        envelope.validate().unwrap();
        envelope.provider_accounts.accounts[0].identity_generation += 1;
        assert!(envelope.validate().is_err());
        envelope.provider_accounts.accounts[0].identity_generation = 1;
        let account = &mut envelope.provider_accounts.accounts[0];
        account.auth_kind = AccountAuthKind::OAuth;
        // Authentication material is rejected even when the canonical directory matches.
        std::fs::write(
            envelope
                .provider_account_home
                .join("provider-accounts/fixture_team.json"),
            serde_json::to_vec(&envelope.provider_accounts).unwrap(),
        )
        .unwrap();
        let record = envelope
            .provider_account_credentials
            .values_mut()
            .next()
            .unwrap();
        record.kind = AccountCredentialKind::OAuth {
            access_token: Secret::new("synthetic-access".into()),
            refresh_token: Some(Secret::new("synthetic-refresh".into())),
            expires_at: std::time::SystemTime::now(),
            scopes: vec![],
            issuer: "https://auth.openai.com".into(),
            subject: Some("principal".into()),
            organization_id: Some("workspace".into()),
            email: None,
            fedramp: false,
        };
        assert!(envelope.validate().is_err());
    }

    #[test]
    fn team_scope_requires_exact_binding_endpoint_and_one_logical_provider() {
        let (_dir, mut envelope) = fixture();
        let profile = |name: &str, base: &str| {
            serde_json::from_value(serde_json::json!({
            "provider_id":"openai","profile_name":name,"base_url":base,"protocol":"open_ai_responses","auth":"api_key",
            "credential":{"source":"host_managed","key":"team-selected-model"},
            "models":[{"display_model":"fixture","request_model":"fixture","billing_model":"fixture","capability_support":{"streaming":"supported","tools":"supported"}}],"extra":{}
        })).unwrap()
        };
        let route = super::super::TeamModelRoute::for_account(
            profile("custom-proxy", "https://proxy.invalid/v1"),
            "fixture",
        )
        .unwrap();
        assert!(!envelope.supports(&route));
        let wrong_endpoint = super::super::TeamModelRoute::for_account(
            profile("openai", "https://proxy.invalid/v1"),
            "fixture",
        )
        .unwrap();
        assert!(!envelope.supports(&wrong_endpoint));
        let matching = super::super::TeamModelRoute::for_account(
            envelope.provider_account_profiles[0].clone(),
            "fixture",
        )
        .unwrap();
        assert!(envelope.supports(&matching));
        let mut unrelated = envelope.provider_account_profiles[0].clone();
        unrelated.profile_name = "other-binding".into();
        unrelated.models[0].request_model = "other-model".into();
        unrelated.models[0].display_model = "other-model".into();
        unrelated.models[0].billing_model = "other-model".into();
        let unrelated = super::super::TeamModelRoute::for_account(unrelated, "other-model")
            .unwrap()
            .model_profile;
        let mut second = envelope.provider_accounts.accounts[0].clone();
        second.account_id = uuid::Uuid::new_v4().to_string();
        second.execution_binding_id = "other-binding".into();
        second.credential_ref.secret_ref = format!("acct.{}", second.account_id);
        envelope.provider_accounts.accounts.push(second);
        envelope.provider_account_profiles.push(unrelated);
        assert!(!envelope.supports(&matching));
        envelope.provider_account_profiles.pop();
        envelope.provider_accounts.accounts.pop();
        let mut foreign = envelope.provider_accounts.accounts[0].clone();
        foreign.account_id = uuid::Uuid::new_v4().to_string();
        foreign.provider_id = "custom-proxy".into();
        foreign.execution_binding_id = "custom-proxy".into();
        foreign.credential_ref.secret_ref = format!("acct.{}", foreign.account_id);
        let foreign_id = foreign.account_id.clone();
        envelope.provider_accounts.accounts.push(foreign);
        // Complete canonical metadata is not a grant for sibling providers.
        assert!(envelope.supports(&matching));
        envelope.provider_account_credentials.insert(
            foreign_id,
            AccountCredential {
                identity_generation: 1,
                credential_revision: 1,
                kind: AccountCredentialKind::ApiKey {
                    key: Secret::new("foreign-key".into()),
                },
            },
        );
        assert!(!envelope.supports(&matching));
    }

    #[tokio::test]
    async fn team_injection_is_exact_and_rejects_same_revision_changed_payload() {
        let (_dir, mut envelope) = fixture();
        let manager = Arc::new(CredentialManager::new(
            Arc::new(lingxi_core::host::InMemorySecureStorage::new()),
            Arc::new(platform_posix::PosixClock::new()),
            Arc::new(platform_posix::PosixHttp::new()),
        ));
        envelope.seed(&manager).await.unwrap();
        let mut foreign = envelope.provider_accounts.accounts[0].clone();
        foreign.account_id = uuid::Uuid::new_v4().to_string();
        foreign.provider_id = "anthropic".into();
        foreign.execution_binding_id = "anthropic".into();
        foreign.credential_ref.secret_ref = format!("acct.{}", foreign.account_id);
        let foreign_ref = foreign.credential_ref.clone();
        envelope.provider_accounts.accounts.push(foreign);
        std::fs::write(
            envelope
                .provider_account_home
                .join("provider-accounts/fixture_team.json"),
            serde_json::to_vec(&envelope.provider_accounts).unwrap(),
        )
        .unwrap();
        envelope.seed(&manager).await.unwrap();
        assert!(manager
            .get_account_credential(&foreign_ref)
            .await
            .unwrap()
            .is_none());
        let account = &envelope.provider_accounts.accounts[0];
        let stored = manager
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.credential_revision, 1);
        envelope
            .provider_account_credentials
            .values_mut()
            .next()
            .unwrap()
            .kind = AccountCredentialKind::ApiKey {
            key: Secret::new("changed-same-revision".into()),
        };
        assert!(envelope.seed(&manager).await.is_err());
        envelope
            .provider_account_credentials
            .values_mut()
            .next()
            .unwrap()
            .credential_revision = 2;
        envelope.seed(&manager).await.unwrap();
        assert_eq!(
            manager
                .get_account_credential(&account.credential_ref)
                .await
                .unwrap()
                .unwrap()
                .credential_revision,
            2
        );
        envelope.provider_account_credentials.clear();
        envelope.seed(&manager).await.unwrap();
        assert!(manager
            .get_account_credential(&account.credential_ref)
            .await
            .unwrap()
            .is_none());
    }
    #[test]
    fn worker_envelope_excludes_owner_registry_and_unselected_account_metadata() {
        let (_source, mut envelope) = fixture();
        let mut foreign = envelope.provider_accounts.accounts[0].clone();
        foreign.account_id = uuid::Uuid::new_v4().to_string();
        foreign.provider_id = "private-provider".into();
        foreign.execution_binding_id = "private-binding".into();
        foreign.display_name = "UNSELECTED_OWNER_METADATA".into();
        foreign.credential_ref.secret_ref = format!("acct.{}", foreign.account_id);
        envelope.provider_accounts.accounts.push(foreign);
        std::fs::write(
            envelope
                .provider_account_home
                .join("provider-accounts/fixture_team.json"),
            serde_json::to_vec(&envelope.provider_accounts).unwrap(),
        )
        .unwrap();
        let target = tempfile::tempdir().unwrap();
        let home = target.path().canonicalize().unwrap();
        let private = envelope.for_worker(&home).unwrap();
        assert_eq!(private.provider_account_home, home);
        assert_eq!(private.provider_accounts.accounts.len(), 1);
        private.validate().unwrap();
        let wire = serde_json::to_string(&private).unwrap();
        assert!(!wire.contains("UNSELECTED_OWNER_METADATA"));
        assert!(!wire.contains(envelope.provider_account_home.to_str().unwrap()));
    }
}
