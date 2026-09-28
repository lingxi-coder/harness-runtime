use super::*;

fn write_settings(path: &std::path::Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn effective(cfg: &DesktopConfig) -> Option<lingxi_core::settings::EffectiveSettings> {
    load_effective_settings_for_config(cfg, &[])
}

/// A config whose USER settings file is not also its PROJECT settings file.
///
/// 🚨 `test_config` puts `lingxi_home` at `<cwd>/.lingxi`, which is exactly
/// where the project layer lives — so writing "the project's settings"
/// there writes the user's too, and a tier test passes for the wrong
/// reason. Asserted below rather than assumed.
fn distinct_layer_config() -> (tempfile::TempDir, DesktopConfig) {
    let (tmp, mut cfg) = tests::test_config(true);
    cfg.lingxi_home = tmp.path().join("home");
    cfg.permission_mode = permission::PermissionMode::Default;
    assert_ne!(
        cfg.lingxi_home.join("settings.json"),
        cfg.cwd.join(".lingxi").join("settings.json"),
        "the user and project settings paths must differ"
    );
    (tmp, cfg)
}

/// 🚨 The security property of `y0r`. A checked-in project settings file
/// must not be able to turn the feature on: only the USER / flag / policy
/// tiers reach the arm that enables it outside a permissive mode.
#[test]
fn a_project_settings_file_cannot_turn_the_diff_on() {
    let (_tmp, cfg) = distinct_layer_config();
    write_settings(
        &cfg.cwd.join(".lingxi").join("settings.json"),
        r#"{"bashEditDiffEnabled": true}"#,
    );

    let eff = effective(&cfg);
    assert_eq!(
        eff.as_ref().and_then(|e| e.settings.bash_edit_diff_enabled),
        Some(true),
        "the project layer must actually be reaching the merged settings, \
         or this test proves nothing"
    );
    assert!(
        resolve_bash_edit_diff_with(&cfg, eff.as_ref(), &[], "s", None, false).is_none(),
        "a project `true` reached the enabling arm — a repository can now \
         make Claude hash and diff its files"
    );
}

/// …and the USER tier can, in the same (non-permissive) mode.
#[test]
fn the_user_tier_turns_the_diff_on() {
    let (_tmp, cfg) = distinct_layer_config();
    write_settings(
        &cfg.lingxi_home.join("settings.json"),
        r#"{"bashEditDiffEnabled": true}"#,
    );

    let setup =
        resolve_bash_edit_diff_with(&cfg, effective(&cfg).as_ref(), &[], "sess", None, false)
            .expect("the user tier enables it");
    assert_eq!(
        setup.shadow_root,
        cfg.lingxi_home.join("bash-edit-diff").join("sess"),
        "the shadow root is per SESSION, so two sessions on one checkout \
         never share a shadow index"
    );
}

/// A `false` from ANY layer still wins — including the project layer the
/// enabling arm ignores. The two reads are not the same read.
#[test]
fn a_project_false_turns_off_what_the_user_tier_turned_on() {
    let (_tmp, cfg) = distinct_layer_config();
    write_settings(
        &cfg.lingxi_home.join("settings.json"),
        r#"{"bashEditDiffEnabled": true}"#,
    );
    write_settings(
        &cfg.cwd.join(".lingxi").join("settings.json"),
        r#"{"bashEditDiffEnabled": false}"#,
    );
    assert!(
        resolve_bash_edit_diff_with(&cfg, effective(&cfg).as_ref(), &[], "s", None, false)
            .is_none(),
        "the merged value is read separately from the tier, and a `false` \
         from any layer wins"
    );
}

/// Unconfigured, in auto mode, the feature is OFF: its last arm needs the
/// `tengu_thrifty_sonic` rollout, which defaults false upstream too.
#[test]
fn an_unconfigured_install_is_off_even_in_auto_mode() {
    let (_tmp, mut cfg) = distinct_layer_config();
    cfg.permission_mode = permission::PermissionMode::Auto;
    assert!(
        resolve_bash_edit_diff_with(&cfg, effective(&cfg).as_ref(), &[], "s", None, false)
            .is_none()
    );
    assert!(
        resolve_bash_edit_diff_with(&cfg, effective(&cfg).as_ref(), &[], "s", None, true)
            .is_some(),
        "…and it is the ROLLOUT that is missing, not the mode"
    );
}

/// `build()` must actually FILL the context field. Every test above passes
/// with the call site deleted — the resolver would simply never run, and
/// the whole feature would be dead code with a green suite. No runtime test
/// can observe this without standing up the desktop stack, so the gate
/// reads this file's own source. Needles are assembled at runtime so they
/// cannot match the comment that explains them.
#[test]
fn build_fills_the_bash_edit_diff_context_field_from_the_resolver() {
    const SRC: &str = include_str!("../mod.rs");
    let production = SRC
        .split_once("\n#[cfg(test)]\nmod tests")
        .map_or(SRC, |(prod, _)| prod);
    let field = "bash_edit_dif".to_string() + "f: resolve_bash_edit_diff(";
    assert_eq!(
        production.matches(&field).count(),
        1,
        "BuiltinToolContext must be built with `{field}…)`, or CLI-5 never runs"
    );
    let tier_call = "load_trusted_tier_bash_edit_dif".to_string() + "f(cfg,";
    assert_eq!(
        production.matches(&tier_call).count(),
        1,
        "the gate must read the TIER separately (`{tier_call}…`); the merged \
         value alone lets a project settings file enable the feature"
    );
}
