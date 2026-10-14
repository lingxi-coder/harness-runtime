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

pub(crate) fn policy(
    home: &Path,
    flag: Option<&SettingsJson>,
    managed: &[SettingsJson],
) -> Result<Policy, lingxi_core::settings::SettingsError> {
    let managed = managed.iter().cloned().try_fold(
        SettingsJson::default(),
        lingxi_core::settings::merger::merge,
    )?;
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
    Ok(Policy {
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
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_returns_native_projection_merge_failures() {
        let home = tempfile::tempdir().unwrap();
        let lower: SettingsJson = serde_json::from_str(r#"{"policyHelpers":{"x":[]}}"#).unwrap();
        let middle: SettingsJson =
            serde_json::from_str(r#"{"policyHelpers":{"x":{"foo":[]}}}"#).unwrap();
        let upper: SettingsJson =
            serde_json::from_str(r#"{"policyHelpers":{"x":{"foo":{"length":1.5}}}}"#).unwrap();

        let error = policy(home.path(), None, &[lower, middle, upper])
            .expect_err("Native-compatible array length coercion must remain fallible");
        assert_eq!(error.to_string(), "RangeError: Invalid array length");
        assert!(matches!(
            error,
            lingxi_core::settings::SettingsError::Projection(
                lingxi_core::types::utf16_json::Utf16JsonProjectionError::InvalidArraySetLength { .. }
            )
        ));
    }

    #[test]
    fn managed_fast_denial_is_not_lost_behind_a_flag_opt_in() {
        let home = tempfile::tempdir().unwrap();
        let flag: SettingsJson = serde_json::from_str(r#"{"fastMode":true}"#).unwrap();
        let managed: SettingsJson = serde_json::from_str(
            r#"{"fastMode":false,"fastModePerSessionOptIn":true,"availableModels":["sonnet"]}"#,
        )
        .unwrap();
        let policy =
            policy(home.path(), Some(&flag), &[managed]).expect("valid policy settings merge");
        assert!(policy.flag_fast);
        assert_eq!(policy.policy_fast, Some(false));
        assert_eq!(policy.policy_session_opt_in, Some(true));
        assert_eq!(
            policy.allowed_models,
            Some(
                lingxi_core::types::utf16_json::Utf16JsonProjection::parse(r#"["sonnet"]"#)
                    .unwrap()
            )
        );
    }
}

#[cfg(test)]
#[path = "fast_settings_session_test.rs"]
mod session_tests;
