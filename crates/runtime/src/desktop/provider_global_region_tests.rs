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
}
