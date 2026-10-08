//! Explicit-root Fast settings: preserve flag and managed policy separately.
use lingxi_core::settings::SettingsJson;
use llm_runtime::model::fast_admission::Policy;
use std::path::Path;

pub(crate) fn session_identity_from_environment(
) -> lingxi_llm_client::providers::anthropic::session_identity::SessionIdentity {
    use lingxi_llm_client::providers::anthropic::session_identity::{
        session_identity, SessionEnvironment,
    };
    let token = std::env::var("CLAUDE_CODE_SESSION_ACCESS_TOKEN").ok();
    let remote = std::env::var("CLAUDE_CODE_REMOTE").ok();
    let kind = std::env::var("CLAUDE_CODE_ENVIRONMENT_KIND").ok();
    let entrypoint = std::env::var("CLAUDE_CODE_ENTRYPOINT").ok();
    session_identity(SessionEnvironment {
        access_token: token.as_deref(),
        remote: remote.as_deref(),
        environment_kind: kind.as_deref(),
        entrypoint: entrypoint.as_deref(),
        ..SessionEnvironment::default()
    })
}

pub(crate) fn policy(home: &Path, flag: Option<&SettingsJson>, managed: &[SettingsJson]) -> Policy {
    let managed = managed.iter().cloned().fold(
        SettingsJson::default(),
        lingxi_core::settings::merger::merge,
    );
    let native_global = home.join(branding::LEGACY_GLOBAL_CONFIG_FILE);
    let path = if native_global.exists() {
        Some(native_global)
    } else {
        migrations::global_config::lingxi_config_home()
            .filter(|value| value == home)
            .and_then(|_| migrations::global_config::global_config_path())
    };
    let cached_org_enabled = path
        .and_then(|path| migrations::global_config::read_map(&path).ok())
        .and_then(|map| {
            map.get("penguinModeOrgEnabled")
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(false);
    let session = session_identity_from_environment();
    Policy {
        flag_fast: flag.and_then(|settings| settings.fast_mode) == Some(true),
        policy_fast: managed.fast_mode,
        policy_session_opt_in: managed.fast_mode_per_session_opt_in,
        allowed_models: managed.available_models,
        model_overrides: managed.model_overrides.unwrap_or_default(),
        cached_org_enabled,
        session_access_token: session.has_session_token,
        no_user_account: session.no_user_account,
        agent_owned_remote: session.agent_owned_remote,
        ..Policy::default()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_fast_denial_is_not_lost_behind_a_flag_opt_in() {
        let home = tempfile::tempdir().unwrap();
        let flag: SettingsJson = serde_json::from_str(r#"{"fastMode":true}"#).unwrap();
        let managed: SettingsJson = serde_json::from_str(
            r#"{"fastMode":false,"fastModePerSessionOptIn":true,"availableModels":["sonnet"]}"#,
        )
        .unwrap();
        let policy = policy(home.path(), Some(&flag), &[managed]);
        assert!(policy.flag_fast);
        assert_eq!(policy.policy_fast, Some(false));
        assert_eq!(policy.policy_session_opt_in, Some(true));
        assert_eq!(policy.allowed_models, Some(vec!["sonnet".into()]));
    }
}

#[cfg(test)]
#[path = "fast_settings_session_test.rs"]
mod session_tests;
