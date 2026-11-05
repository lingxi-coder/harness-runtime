use super::{resolve_provider_region, DesktopConfig};
use lingxi_core::settings::{ProviderRegion, SettingsJson};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::fs;

fn policy(json: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(json).unwrap()
}

fn config(root: &std::path::Path, project: &str) -> DesktopConfig {
    let home = root.join("host");
    let cwd = root.join(project);
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(cwd.join(".lingxi")).unwrap();
    DesktopConfig {
        lingxi_home: home,
        cwd,
        ..DesktopConfig::default()
    }
}

#[test]
fn conflicting_projects_and_session_flags_cannot_override_host_region() {
    let root = tempfile::tempdir().unwrap();
    let mut first = config(root.path(), "first");
    let second = config(root.path(), "second");
    fs::write(
        first.lingxi_home.join("settings.json"),
        r#"{"providerRegion":"international"}"#,
    )
    .unwrap();
    for cfg in [&first, &second] {
        fs::write(
            cfg.cwd.join(".lingxi/settings.json"),
            r#"{"providerRegion":"china_mainland"}"#,
        )
        .unwrap();
        fs::write(
            cfg.cwd.join(".lingxi/settings.local.json"),
            r#"{"providerRegion":"china_mainland"}"#,
        )
        .unwrap();
    }
    first.flag_settings = Some(SettingsJson {
        provider_region: Some(ProviderRegion::ChinaMainland),
        ..SettingsJson::default()
    });
    assert!(matches!(
        resolve_provider_region(&first, &policy("{}")).unwrap(),
        llm_runtime::Region::International
    ));
    assert!(matches!(
        resolve_provider_region(&second, &policy("{}")).unwrap(),
        llm_runtime::Region::International
    ));
    fs::write(
        first.lingxi_home.join("settings.json"),
        r#"{"providerRegion":"china_mainland"}"#,
    )
    .unwrap();
    assert!(matches!(
        resolve_provider_region(&first, &policy("{}")).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
    assert!(matches!(
        resolve_provider_region(&second, &policy("{}")).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
}

#[test]
fn host_provider_region_is_available_without_project_settings_admission() {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = config(root.path(), "project");
    cfg.restricted = true;
    cfg.setting_source_scope = (false, false);
    fs::write(
        cfg.lingxi_home.join("settings.json"),
        r#"{"providerRegion":"china_mainland"}"#,
    )
    .unwrap();
    assert!(matches!(
        resolve_provider_region(&cfg, &policy("{}")).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
}

#[test]
fn global_managed_region_keeps_precedence() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), "project");
    fs::write(
        cfg.lingxi_home.join("settings.json"),
        r#"{"providerRegion":"international"}"#,
    )
    .unwrap();
    assert!(matches!(
        resolve_provider_region(&cfg, &policy(r#"{"providerRegion":"china_mainland"}"#)).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
}

#[test]
fn missing_host_settings_use_default_or_managed_region_without_project_input() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), "project");
    for name in ["settings.json", "settings.local.json"] {
        fs::write(
            cfg.cwd.join(".lingxi").join(name),
            r#"{"providerRegion":"china_mainland"}"#,
        )
        .unwrap();
    }
    assert!(matches!(
        resolve_provider_region(&cfg, &policy("{}")).unwrap(),
        llm_runtime::Region::International
    ));
    assert!(matches!(
        resolve_provider_region(&cfg, &policy(r#"{"providerRegion":"china_mainland"}"#)).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
}

#[test]
fn malformed_host_settings_fail_instead_of_using_project_region() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), "project");
    fs::write(cfg.lingxi_home.join("settings.json"), "{broken").unwrap();
    fs::write(
        cfg.cwd.join(".lingxi/settings.json"),
        r#"{"providerRegion":"china_mainland"}"#,
    )
    .unwrap();
    assert!(resolve_provider_region(&cfg, &policy("{}")).is_err());
    assert!(
        resolve_provider_region(&cfg, &policy(r#"{"providerRegion":"china_mainland"}"#)).is_err()
    );
}

#[test]
fn invalid_non_file_or_oversized_host_settings_fail_closed() {
    let root = tempfile::tempdir().unwrap();
    let cfg = config(root.path(), "project");
    let path = cfg.lingxi_home.join("settings.json");
    fs::write(&path, r#"{"providerRegion":"invalid-region"}"#).unwrap();
    assert!(resolve_provider_region(&cfg, &policy("{}")).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(resolve_provider_region(&cfg, &policy("{}")).is_err());
    fs::remove_dir(&path).unwrap();
    fs::File::create(&path)
        .unwrap()
        .set_len(lingxi_core::settings::loader::MAX_SETTINGS_FILE_BYTES + 1)
        .unwrap();
    assert!(resolve_provider_region(&cfg, &policy("{}")).is_err());
}

#[cfg(target_os = "macos")]
#[test]
fn team_uses_admitted_region_without_reading_host_or_project_settings() {
    use super::team_invocation::{
        TeamInvocationIdentity, TeamInvocationLimits, TeamInvocationProfile,
    };
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let profile = TeamInvocationProfile::prepare(
        &root.path().canonicalize().unwrap(),
        TeamInvocationIdentity {
            owner_uid: "owner".into(),
            bot_key: "bot".into(),
            team_id: "team".into(),
            conversation_id: "conversation".into(),
            origin_human_uid: "human".into(),
            membership_generation: 1,
        },
        uuid::Uuid::new_v4(),
        "Team instructions".into(),
        TeamInvocationLimits::default(),
    )
    .unwrap();
    let mut cfg = DesktopConfig {
        default_model: "anthropic/claude-sonnet-4-20250514".into(),
        ..DesktopConfig::default()
    };
    profile.apply_to(&mut cfg).unwrap();
    fs::write(cfg.lingxi_home.join("settings.json"), "{broken").unwrap();
    fs::create_dir_all(cfg.cwd.join(".lingxi")).unwrap();
    for name in ["settings.json", "settings.local.json"] {
        fs::write(
            cfg.cwd.join(".lingxi").join(name),
            r#"{"providerRegion":"china_mainland"}"#,
        )
        .unwrap();
    }
    assert!(matches!(
        resolve_provider_region(&cfg, &policy("{}")).unwrap(),
        llm_runtime::Region::International
    ));
    cfg.flag_settings = Some(SettingsJson {
        provider_region: Some(ProviderRegion::ChinaMainland),
        ..SettingsJson::default()
    });
    assert!(matches!(
        resolve_provider_region(&cfg, &policy("{}")).unwrap(),
        llm_runtime::Region::ChinaMainland
    ));
    assert!(matches!(
        resolve_provider_region(&cfg, &policy(r#"{"providerRegion":"international"}"#)).unwrap(),
        llm_runtime::Region::International
    ));
}
