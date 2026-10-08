use super::assembly::apply_server_fallback_model_policy;
use llm_runtime::model::allowlist::{ModelEnforcement, PolicyModelView, PolicySource};
use orchestrator::OrchestratorConfig;

#[test]
fn failed_managed_policy_survives_composition_and_regular_defaults() {
    let mut config = OrchestratorConfig::default();
    let settings = serde_json::from_value(serde_json::json!({
        "availableModels": ["sonnet"],
        "modelOverrides": {"claude-sonnet-5": "wire-sonnet"}
    }))
    .unwrap();
    apply_server_fallback_model_policy(
        &mut config,
        &PolicySource::Failed,
        Some(&settings),
        "claude-opus-5",
    );
    assert_eq!(
        config.server_fallback_model_enforcement,
        Some(ModelEnforcement::Refused)
    );
    assert_eq!(
        config.server_fallback_regular_available_models,
        Some(vec!["sonnet".into()])
    );
    assert_eq!(
        config.server_fallback_regular_model_overrides["claude-sonnet-5"],
        "wire-sonnet"
    );
    assert_eq!(
        config.server_fallback_default_model.as_deref(),
        Some("claude-opus-5")
    );
}

#[test]
fn inactive_and_empty_regular_policy_remain_distinct_from_missing_policy() {
    let mut config = OrchestratorConfig::default();
    let source = PolicySource::Loaded(PolicyModelView::default());
    let settings = serde_json::from_value(serde_json::json!({"availableModels": []})).unwrap();
    apply_server_fallback_model_policy(&mut config, &source, Some(&settings), "claude-opus-5");
    assert_eq!(
        config.server_fallback_model_enforcement,
        Some(ModelEnforcement::Inactive)
    );
    assert_eq!(
        config.server_fallback_regular_available_models,
        Some(vec![])
    );
    apply_server_fallback_model_policy(&mut config, &source, None, "claude-sonnet-5");
    assert_eq!(config.server_fallback_regular_available_models, None);
    assert!(config.server_fallback_regular_model_overrides.is_empty());
}

#[tokio::test]
async fn managed_read_failure_does_not_become_an_absent_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("managed-settings.json"), [0xff]).unwrap();
    let drops = dir.path().join("managed-settings.d");
    std::fs::create_dir(&drops).unwrap();
    std::fs::write(drops.join("valid.json"), "{\"availableModels\":[\"opus\"]}").unwrap();
    let snapshot = super::settings_watch::read_managed_settings_snapshot(dir.path()).await;
    assert!(snapshot.read_failed);
    assert_eq!(
        snapshot.raw_tiers.len(),
        1,
        "best-effort readers still retain readable tiers"
    );
}

#[tokio::test]
async fn missing_managed_files_and_drop_in_directory_are_not_read_failures() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = super::settings_watch::read_managed_settings_snapshot(dir.path()).await;
    assert!(!snapshot.read_failed);
    assert!(snapshot.raw_tiers.is_empty());
    std::fs::write(dir.path().join("managed-settings.d"), "not a directory").unwrap();
    let snapshot = super::settings_watch::read_managed_settings_snapshot(dir.path()).await;
    assert!(
        !snapshot.read_failed,
        "native ENOTDIR denotes an absent tier"
    );
}
