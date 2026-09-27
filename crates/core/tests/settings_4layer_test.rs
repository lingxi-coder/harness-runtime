//! Integration test for the settings loader end-to-end.
//!
//! Spec coverage: file-layer precedence, local-settings path semantics,
//! §7 wire-identifier byte-alignment (paths, prefix order, array fields).
//!
//! Every test mutates the process-wide `HOME` env var so the loader's
//! `user_settings_path()` redirects into a tempdir. Because integration tests
//! in a single binary run in parallel by default, we serialize them on a
//! file-local `HOME_LOCK` mutex — mirroring the
//! `crate::settings::test_support::HOME_LOCK` pattern used by the unit tests
//! (which is `pub(crate)` and therefore unreachable from `tests/`).

use lingxi_core::settings::schema::SettingsJson;
use lingxi_core::settings::{LoadInputs, Settings};
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

static HOME_LOCK: Mutex<()> = Mutex::new(());

fn write_file(path: &std::path::Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut f = std::fs::File::create(path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
}

#[test]
fn user_settings_path_is_dot_claude_settings_json() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", tmp.path());
    let p = lingxi_core::settings::loader::user_settings_path().unwrap();
    // Byte-for-byte literal from spec §7.
    assert_eq!(p, tmp.path().join(".lingxi").join("settings.json"));
}

#[test]
fn project_settings_path_is_dot_claude_settings_json() {
    let tmp = tempfile::tempdir().unwrap();
    let p = lingxi_core::settings::loader::project_settings_path(tmp.path());
    assert_eq!(p, tmp.path().join(".lingxi").join("settings.json"));
}

#[test]
fn local_settings_path_is_dot_claude_settings_local_json() {
    let tmp = tempfile::tempdir().unwrap();
    let p = lingxi_core::settings::loader::local_settings_path(tmp.path());
    assert_eq!(p, tmp.path().join(".lingxi").join("settings.local.json"));
}

#[test]
fn five_layer_priority_env_local_project_user_defaults() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::env::set_var("HOME", &home);

    write_file(
        &home.join(".lingxi").join("settings.json"),
        r#"{"model": "user-model"}"#,
    );
    write_file(
        &tmp.path()
            .join("project")
            .join(".lingxi")
            .join("settings.json"),
        r#"{"model": "project-model"}"#,
    );
    write_file(
        &tmp.path()
            .join("project")
            .join(".lingxi")
            .join("settings.local.json"),
        r#"{"model": "local-model"}"#,
    );

    let defaults = SettingsJson {
        model: Some("default-model".into()),
        ..Default::default()
    };

    // Case A: no env override.
    let eff = Settings::load(LoadInputs {
        env: &BTreeMap::new(),
        project_dir: &tmp.path().join("project"),
        defaults: defaults.clone(),
    })
    .unwrap();
    assert_eq!(eff.settings.model.as_deref(), Some("local-model"));

    // Case B: env override wins.
    let mut env = BTreeMap::new();
    env.insert("LINGXI_MODEL".to_string(), "env-model".to_string());
    let eff = Settings::load(LoadInputs {
        env: &env,
        project_dir: &tmp.path().join("project"),
        defaults,
    })
    .unwrap();
    assert_eq!(eff.settings.model.as_deref(), Some("env-model"));
}

#[test]
fn env_prefix_lingxi_is_read() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", tmp.path().join("home_eppp"));

    // Clean break: only the LINGXI_ prefix is read for settings overrides.
    let mut env = BTreeMap::new();
    env.insert("LINGXI_MODEL".into(), "from-lingxi".into());

    let eff = Settings::load(LoadInputs {
        env: &env,
        project_dir: tmp.path(),
        defaults: SettingsJson::default(),
    })
    .unwrap();
    assert_eq!(eff.settings.model.as_deref(), Some("from-lingxi"));
}

#[test]
fn array_concat_dedup_across_defaults_user_project_local_env_layers() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home_acd");
    std::env::set_var("HOME", &home);

    write_file(
        &home.join(".lingxi").join("settings.json"),
        r#"{"enabledTools": ["Read", "Edit"]}"#,
    );
    write_file(
        &tmp.path().join("p").join(".lingxi").join("settings.json"),
        r#"{"enabledTools": ["Bash", "Read"]}"#,
    );
    write_file(
        &tmp.path()
            .join("p")
            .join(".lingxi")
            .join("settings.local.json"),
        r#"{"enabledTools": ["Task", "Read"]}"#,
    );

    let defaults = SettingsJson {
        enabled_tools: Some(vec!["Grep".into(), "Bash".into()]),
        ..Default::default()
    };

    let mut env = BTreeMap::new();
    env.insert("LINGXI_ENABLED_TOOLS".into(), "WebFetch:Grep".into());

    let eff = Settings::load(LoadInputs {
        env: &env,
        project_dir: &tmp.path().join("p"),
        defaults,
    })
    .unwrap();

    // Order is defaults → user → project → local → env, deduped.
    assert_eq!(
        eff.settings.enabled_tools.as_deref(),
        Some(
            &[
                "Grep".to_string(),
                "Bash".to_string(),
                "Read".to_string(),
                "Edit".to_string(),
                "Task".to_string(),
                "WebFetch".to_string(),
            ][..]
        )
    );
}

#[test]
fn dollar_schema_field_is_tolerated_but_not_required() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home_ds");
    std::env::set_var("HOME", &home);

    // Older / IDE-injected file with $schema.
    write_file(
        &home.join(".lingxi").join("settings.json"),
        r#"{"$schema": "https://example/x.json", "model": "m"}"#,
    );

    let eff = Settings::load(LoadInputs {
        env: &BTreeMap::new(),
        project_dir: tmp.path(),
        defaults: SettingsJson::default(),
    })
    .unwrap();
    assert_eq!(eff.settings.model.as_deref(), Some("m"));
}

#[test]
fn effective_for_returns_provenance_for_each_field() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home_ef");
    std::env::set_var("HOME", &home);

    write_file(
        &home.join(".lingxi").join("settings.json"),
        r#"{"model": "user-m"}"#,
    );

    let eff = Settings::load(LoadInputs {
        env: &BTreeMap::new(),
        project_dir: tmp.path(),
        defaults: SettingsJson::default(),
    })
    .unwrap();

    let prov = eff.effective_for("model").unwrap();
    assert_eq!(
        prov.contributors,
        vec![lingxi_core::settings::tracer::Source::User]
    );
    assert!(eff.effective_for("nonsense_field").is_none());
}

#[test]
fn missing_user_and_project_files_yield_defaults_only() {
    let _guard = HOME_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", tmp.path().join("nothing_here"));
    let defaults = SettingsJson {
        model: Some("only-default".into()),
        ..Default::default()
    };
    let eff = Settings::load(LoadInputs {
        env: &BTreeMap::new(),
        project_dir: tmp.path(),
        defaults,
    })
    .unwrap();
    assert_eq!(eff.settings.model.as_deref(), Some("only-default"));
}
