//! Synthetic account-only Team credentials; no native store or real account access.
use harness_runtime::desktop::team_process::{
    TeamAccountEnvelope, TeamModelRoute, TeamProcessError, TeamProcessModel,
};
use lingxi_core::host::provider_accounts::{AccountAuthKind, AccountMetadata, CredentialRef};
use lingxi_core::types::Secret;
use std::collections::BTreeMap;

pub fn selected(route: TeamModelRoute, key: String) -> Result<TeamProcessModel, TeamProcessError> {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let root = ROOT.get_or_init(|| tempfile::tempdir().unwrap());
    let home = root.path().canonicalize().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let scope = format!("team_fixture_{}", uuid::Uuid::new_v4().simple());
    let mut registry = provider_config::AccountRegistry::new(scope.clone());
    registry
        .add(
            AccountMetadata {
                account_id: id.clone(),
                provider_id: route
                    .model_profile
                    .connection
                    .group
                    .clone()
                    .unwrap_or_else(|| route.model_provider.clone()),
                display_name: "Synthetic Team fixture".into(),
                auth_kind: AccountAuthKind::ApiKey,
                execution_binding_id: route.model_provider.clone(),
                credential_ref: CredentialRef {
                    store_scope: scope.clone(),
                    secret_ref: format!("acct.{id}"),
                },
                enabled: true,
                user_priority: 0,
                config_revision: 0,
                identity_generation: 1,
            },
            0,
        )
        .unwrap();
    let snapshot = registry.snapshot();
    std::fs::create_dir_all(home.join("provider-accounts")).unwrap();
    std::fs::write(
        home.join(format!("provider-accounts/{scope}.json")),
        serde_json::to_vec(&snapshot).unwrap(),
    )
    .unwrap();
    let _lock = lingxi_core::host::rooted_fs::lock_exclusive(
        &home,
        &std::path::PathBuf::from(format!("provider-accounts/{scope}.lock")),
        0o700,
        0o600,
    )
    .unwrap();
    provider_config::write_account_credential_version_locked(&home, &snapshot.accounts[0], 1)
        .unwrap();
    drop(_lock);
    let records = BTreeMap::from([(
        id,
        secret::AccountCredential {
            credential_revision: 1,
            identity_generation: 1,
            kind: secret::AccountCredentialKind::ApiKey {
                key: Secret::new(key),
            },
        },
    )]);
    let envelope:TeamAccountEnvelope=serde_json::from_value(serde_json::json!({"provider_accounts":snapshot,"provider_account_home":home,"provider_account_credentials":records,"provider_account_profiles":[route.model_profile.clone()]})).unwrap();
    TeamProcessModel::selected_account_pool(route, envelope)
}

pub fn model(
    model: String,
    endpoint: String,
    key: String,
) -> Result<TeamProcessModel, TeamProcessError> {
    let mut profile = llm_runtime::anthropic_provider_profile(
        &endpoint,
        llm_runtime::AuthStrategy::ApiKey,
        llm_runtime::CredentialConfig::Static {
            id: "team-selected-model".into(),
        },
    );
    profile.models.retain(|entry| entry.request_model == model);
    let client = llm_runtime::ModelRuntime::from_config(llm_runtime::ClientConfig {
        providers: vec![profile],
    })
    .map_err(|_| TeamProcessError::Model)?;
    let mut profile = client
        .provider_service_profiles()
        .into_iter()
        .next()
        .ok_or(TeamProcessError::Model)?;
    profile.profile_name = "fixture-anthropic".into();
    selected(TeamModelRoute::for_account(profile, &model)?, key)
}
