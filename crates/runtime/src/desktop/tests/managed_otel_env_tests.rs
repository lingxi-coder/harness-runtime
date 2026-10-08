#[tokio::test]
async fn production_loader_threads_nested_managed_env_into_otel_config() {
    let _guard = super::tests::MANAGED_ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("managed-settings.json"),
        r#"{
            "LINGXI_ENABLE_TELEMETRY": false,
            "OTEL_SERVICE_NAME": "base-root",
            "env": {
                "LINGXI_ENABLE_TELEMETRY": true,
                "OTEL_SERVICE_NAME": "base-env"
            }
        }"#,
    )
    .expect("write base managed settings");
    let drop_in = tmp.path().join("managed-settings.d");
    std::fs::create_dir_all(&drop_in).expect("create drop-in directory");
    std::fs::write(
        drop_in.join("10-telemetry.json"),
        r#"{
            "OTEL_SERVICE_NAME": "drop-in-root",
            "env": {
                "OTEL_SERVICE_NAME": "drop-in-env",
                "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": true
            }
        }"#,
    )
    .expect("write drop-in managed settings");
    let previous_managed_dir = std::env::var_os(super::settings_watch::MANAGED_DIR_ENV);
    std::env::set_var(super::settings_watch::MANAGED_DIR_ENV, tmp.path());

    let managed = super::managed_otel_env_overrides().await;
    if let Some(value) = previous_managed_dir {
        std::env::set_var(super::settings_watch::MANAGED_DIR_ENV, value);
    } else {
        std::env::remove_var(super::settings_watch::MANAGED_DIR_ENV);
    }

    let previous_beta = std::env::var_os(telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA);
    std::env::remove_var(telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA);
    let config = telemetry::otel::OtelConfig::from_env_with_managed(&managed);
    let mut without_beta = managed;
    without_beta.remove(telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA);
    let config_without_beta = telemetry::otel::OtelConfig::from_env_with_managed(&without_beta);
    if let Some(value) = previous_beta {
        std::env::set_var(telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA, value);
    }

    assert!(config.enabled);
    assert!(config.enhanced_telemetry);
    assert_eq!(config.service_name, "drop-in-env");
    assert!(!config_without_beta.enhanced_telemetry);
}

#[test]
fn nested_env_overrides_legacy_root_within_the_same_tier() {
    let tiers = vec![
        r#"{
            "LINGXI_ENABLE_TELEMETRY": false,
            "OTEL_SERVICE_NAME": "legacy",
            "env": {
                "LINGXI_ENABLE_TELEMETRY": true,
                "OTEL_SERVICE_NAME": "canonical"
            }
        }"#
        .to_string(),
    ];

    let values = super::fold_managed_otel_env_overrides(&tiers);
    assert_eq!(
        values
            .get(telemetry::otel::config::ENV_ENABLE_TELEMETRY)
            .map(String::as_str),
        Some("true")
    );
    assert_eq!(
        values
            .get(telemetry::otel::config::ENV_SERVICE_NAME)
            .map(String::as_str),
        Some("canonical")
    );
}

#[test]
fn later_managed_tier_wins_and_enhanced_beta_is_allowlisted() {
    let tiers = vec![
        r#"{
            "env": {
                "OTEL_SERVICE_NAME": "base",
                "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": false
            }
        }"#
        .to_string(),
        r#"{
            "OTEL_SERVICE_NAME": "drop-in-root",
            "env": {
                "OTEL_SERVICE_NAME": "drop-in-env",
                "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA": true
            }
        }"#
        .to_string(),
    ];

    let values = super::fold_managed_otel_env_overrides(&tiers);
    assert_eq!(
        values
            .get(telemetry::otel::config::ENV_SERVICE_NAME)
            .map(String::as_str),
        Some("drop-in-env")
    );
    assert_eq!(
        values
            .get(telemetry::otel::config::ENV_ENHANCED_TELEMETRY_BETA)
            .map(String::as_str),
        Some("true")
    );
}

#[test]
fn legacy_root_remains_supported_and_non_scalars_are_ignored() {
    let tiers = vec![
        r#"{
            "OTEL_SERVICE_NAME": "legacy",
            "OTEL_TRACES_EXPORTER": ["otlp"],
            "env": {
                "OTEL_LOGS_EXPORTER": {"kind": "otlp"},
                "NOT_AN_OTEL_KEY": "ignored"
            }
        }"#
        .to_string(),
    ];

    let values = super::fold_managed_otel_env_overrides(&tiers);
    assert_eq!(
        values
            .get(telemetry::otel::config::ENV_SERVICE_NAME)
            .map(String::as_str),
        Some("legacy")
    );
    assert!(!values.contains_key(telemetry::otel::config::ENV_TRACES_EXPORTER));
    assert!(!values.contains_key(telemetry::otel::config::ENV_LOGS_EXPORTER));
    assert!(!values.contains_key("NOT_AN_OTEL_KEY"));
}
