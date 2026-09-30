//! On-disk `settings.enabledPlugins` toggle backing `plugin enable/disable`.
//!
//! This is the CLI seam claude-code's `plugin enable/disable` uses: a
//! read-modify-write of the `enabledPlugins` allowlist in a settings.json file
//! at the chosen scope — NOT the heavy [`plugin::PluginManager`] engine-registry
//! materialization (that runs at bootstrap from the merged allowlist). The
//! allowlist is a pure map `plugin@marketplace → bool`; enabling/disabling a
//! plugin that is not installed is allowed (claude writes the entry regardless).
//!
//! Behavior is 1:1 with `claude plugin enable/disable` 2.1.201 (probed against
//! the real binary in an isolated `$CLAUDE_CONFIG_DIR`):
//!
//! * `enable a@b`   → `{"enabledPlugins":{"a@b":true}}`; stdout
//!   `✔ Successfully enabled plugin: a (scope: user)` (name = the pre-`@` part).
//! * `disable a@b`  → sets the entry to `false` (it is NOT deleted).
//! * a bare `name` (no `@`) resolves to an existing `name@*` key across the
//!   editable scopes; if none matches →
//!   `Plugin "name" not found in any editable settings scope. Use plugin@marketplace format.`
//! * enabling an already-`true` entry → `Plugin "a@b" is already enabled`;
//!   disabling an absent/`false` entry → `Plugin "a@b" is already disabled`.
//! * `disable --all` flips every currently-`true` entry to `false` across the
//!   editable scopes and reports `✔ Disabled N plugins`.
//! * an unknown `--scope` →
//!   `Invalid scope "x". Valid scopes: user, project, local`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use migrations::settings_update::{read_settings_map, update_settings};
use serde_json::{Map, Value};

use crate::plugin_policy;

/// The editable settings scopes, re-exported so this module reads as the seam
/// that owns the `enabledPlugins` allowlist.
///
/// `user` = `<lingxi-home>/settings.json`; `project` =
/// `<cwd>/.lingxi/settings.json`; `local` = `<cwd>/.lingxi/settings.local.json`.
/// (`managed` is a read-only enterprise scope and is not editable here — it is
/// only a valid `--scope` for `plugin update`, handled separately. That refusal
/// is why this stays [`lingxi_core::types::WritableScope`] rather than a bare
/// `lingxi_core::types::Scope`.)
pub use lingxi_core::types::WritableScope;

/// The editable scopes, in auto-detect / `--all` search order.
const EDITABLE: [WritableScope; 3] = [
    WritableScope::User,
    WritableScope::Project,
    WritableScope::Local,
];

/// The editable scopes, in auto-detect / iteration order (shared with the
/// marketplace command).
/// Answers: which scope `plugin enable` auto-detects first.
///
/// One of several orderings over these rungs; `lingxi_core::types::scope`'s module docs index them all and say which question each answers.
pub const SCOPES: [WritableScope; 3] = EDITABLE;

/// The scope's wire label (matches the `(scope: …)` success suffix).
///
/// Local to this subsystem on purpose: the same rung is spelled differently by
/// the hook payload, the MCP telemetry inventory, and the settings admin panel,
/// so there is no shared `WritableScope::as_str`.
pub fn scope_label(scope: WritableScope) -> &'static str {
    match scope {
        WritableScope::User => "user",
        WritableScope::Project => "project",
        WritableScope::Local => "local",
    }
}

/// Parse a `--scope` value; `None` when unrecognized (the caller emits the
/// `Invalid scope …` error). `managed` is intentionally NOT accepted here.
pub fn parse_scope_str(s: &str) -> Option<WritableScope> {
    match s {
        "user" => Some(WritableScope::User),
        "project" => Some(WritableScope::Project),
        "local" => Some(WritableScope::Local),
        _ => None,
    }
}

/// Resolve the settings.json path for this scope.
pub fn scope_path(scope: WritableScope, home: &Path, cwd: &Path) -> PathBuf {
    match scope {
        WritableScope::User => home.join("settings.json"),
        WritableScope::Project => cwd.join(branding::DOT_DIR).join("settings.json"),
        WritableScope::Local => cwd.join(branding::DOT_DIR).join("settings.local.json"),
    }
}

/// The display name of a `plugin@marketplace` id: the segment before the first
/// `@` (a bare name is returned as-is).
fn name_of(id: &str) -> &str {
    id.split('@').next().unwrap_or(id)
}

/// Marketplace segment of a `plugin@marketplace` id.
fn marketplace_of(id: &str) -> Option<&str> {
    id.split_once('@').map(|(_, marketplace)| marketplace)
}

pub fn marketplace_source(
    home: &Path,
    marketplace: &str,
) -> Option<plugin_policy::MarketplaceSourceIdentity> {
    std::fs::read_to_string(home.join("plugins").join("known_marketplaces.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|registry| registry.get(marketplace).cloned())
        .as_ref()
        .and_then(plugin_policy::MarketplaceSourceIdentity::from_value)
}

/// The `enabledPlugins` map from a scope's settings file (missing key / missing
/// / malformed file ⇒ empty map).
pub fn read_enabled(path: &Path) -> Map<String, Value> {
    read_settings_map(path)
        .ok()
        .and_then(|m| m.get("enabledPlugins").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// The current allowlist value of `id` in a scope (`None` when the key is
/// absent or is not a bool).
fn current_value(path: &Path, id: &str) -> Option<bool> {
    read_enabled(path).get(id).and_then(Value::as_bool)
}

/// Read-modify-write `enabledPlugins[id] = value` into a scope's settings file,
/// preserving every other key (delegates to the JSON-faithful settings writer).
fn set_value(path: &Path, id: &str, value: bool) -> Result<(), String> {
    let mut map = read_enabled(path);
    map.insert(id.to_string(), Value::Bool(value));
    update_settings(
        path,
        vec![("enabledPlugins".to_string(), Some(Value::Object(map)))],
    )
}

fn installed_plugins(home: &Path) -> Value {
    std::fs::read_to_string(home.join("plugins").join("installed_plugins.json"))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({"version":2,"plugins":{}}))
}

fn installed_record<'a>(
    db: &'a Value,
    id: &str,
    scope: WritableScope,
    project_path: &Option<String>,
) -> Option<&'a Value> {
    db.get("plugins")
        .and_then(|plugins| plugins.get(id))
        .and_then(Value::as_array)?
        .iter()
        .find(|record| {
            if record.get("scope").and_then(Value::as_str) != Some(scope_label(scope)) {
                return false;
            }
            match project_path {
                Some(path) => {
                    record.get("projectPath").and_then(Value::as_str) == Some(path.as_str())
                }
                None => record.get("projectPath").is_none(),
            }
        })
}

fn scope_project_path(scope: WritableScope, cwd: &Path) -> Option<String> {
    match scope {
        WritableScope::User => None,
        WritableScope::Project | WritableScope::Local => Some(
            std::fs::canonicalize(cwd)
                .unwrap_or_else(|_| cwd.to_path_buf())
                .display()
                .to_string(),
        ),
    }
}

fn record_dependencies(record: &Value, owner_id: &str) -> Vec<String> {
    let Some(path) = record.get("installPath").and_then(Value::as_str) else {
        return Vec::new();
    };
    let dependencies = std::fs::read_to_string(
        Path::new(path)
            .join(branding::PLUGIN_MANIFEST_DIR)
            .join("plugin.json"),
    )
    .ok()
    .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
    .and_then(|manifest| manifest.get("dependencies").cloned());
    let owner_marketplace = marketplace_of(owner_id).unwrap_or("");
    plugin::parse_dependencies(dependencies.as_ref())
        .unwrap_or_default()
        .into_iter()
        .map(|dependency| dependency.resolved_id(owner_marketplace))
        .collect()
}

fn collect_dependencies(
    db: &Value,
    id: &str,
    scope: WritableScope,
    project_path: &Option<String>,
    seen: &mut HashSet<String>,
) {
    if !seen.insert(id.to_string()) {
        return;
    }
    let Some(record) = installed_record(db, id, scope, project_path) else {
        return;
    };
    for dependency in record_dependencies(record, id) {
        collect_dependencies(db, &dependency, scope, project_path, seen);
    }
}

fn enabled_dependents(
    db: &Value,
    id: &str,
    scope: WritableScope,
    project_path: &Option<String>,
    settings_path: &Path,
) -> Vec<String> {
    let enabled = read_enabled(settings_path);
    let Some(record) = installed_record(db, id, scope, project_path) else {
        return Vec::new();
    };
    record
        .get("requiredBy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|owner| enabled.get(*owner).and_then(Value::as_bool) == Some(true))
        .map(ToOwned::to_owned)
        .collect()
}

/// Resolve a user-supplied plugin argument to a full `plugin@marketplace` id.
///
/// A value already containing `@` is a full id (returned verbatim). A bare
/// `name` is resolved by searching the editable scopes' allowlists for a key
/// whose name-part matches; the first match wins. No match →
/// `Plugin "name" not found in any editable settings scope. Use plugin@marketplace format.`
fn resolve_id(plugin: &str, home: &Path, cwd: &Path) -> Result<String, String> {
    if plugin.contains('@') {
        return Ok(plugin.to_string());
    }
    for scope in EDITABLE {
        let map = read_enabled(&scope_path(scope, home, cwd));
        if let Some(key) = map.keys().find(|k| name_of(k) == plugin) {
            return Ok(key.clone());
        }
    }
    Err(format!(
        "Plugin \"{plugin}\" not found in any editable settings scope. \
         Use plugin@marketplace format."
    ))
}

/// The first editable scope whose allowlist carries `id` as a key (any value),
/// for auto-detect when no `--scope` is given.
fn scope_holding(id: &str, home: &Path, cwd: &Path) -> Option<WritableScope> {
    EDITABLE
        .into_iter()
        .find(|scope| read_enabled(&scope_path(*scope, home, cwd)).contains_key(id))
}

/// The formatted `✘ Failed to <verb> plugin "<id>": <reason>` line.
fn fail(verb: &str, id: &str, reason: &str) -> String {
    format!("✘ Failed to {verb} plugin \"{id}\": {reason}")
}

/// The `Invalid scope …` line (no ✘ prefix — matches the binary).
fn invalid_scope(s: &str) -> String {
    format!("Invalid scope \"{s}\". Valid scopes: user, project, local")
}

/// Parse an optional `--scope`, surfacing the `Invalid scope …` error.
fn parse_scope(scope: Option<&str>) -> Result<Option<WritableScope>, String> {
    match scope {
        None => Ok(None),
        Some(s) => parse_scope_str(s).map(Some).ok_or_else(|| invalid_scope(s)),
    }
}

/// `plugin enable <plugin>` — set the allowlist entry to `true`.
///
/// Returns the `✔ Successfully enabled …` line on success, or the full
/// (already-formatted) error line for the caller to print to stderr.
pub fn run_enable(
    plugin: &str,
    scope: Option<&str>,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let requested = parse_scope(scope)?;
    let id = resolve_id(plugin, home, cwd).map_err(|reason| fail("enable", plugin, &reason))?;
    if let Some(marketplace) = marketplace_of(&id) {
        let source = marketplace_source(home, marketplace);
        plugin_policy::ensure_marketplace_source_allowed(Some(marketplace), source.as_ref())
            .map_err(|reason| fail("enable", &id, &reason))?;
    }
    let scope = requested
        .or_else(|| scope_holding(&id, home, cwd))
        .unwrap_or(WritableScope::User);
    let path = scope_path(scope, home, cwd);
    if current_value(&path, &id) == Some(true) {
        return Err(fail(
            "enable",
            &id,
            &format!("Plugin \"{id}\" is already enabled"),
        ));
    }
    let installed = installed_plugins(home);
    let project_path = scope_project_path(scope, cwd);
    let mut enable = HashSet::new();
    collect_dependencies(&installed, &id, scope, &project_path, &mut enable);
    enable.insert(id.clone());
    let mut map = read_enabled(&path);
    for plugin_id in enable {
        map.insert(plugin_id, Value::Bool(true));
    }
    update_settings(
        &path,
        vec![("enabledPlugins".to_string(), Some(Value::Object(map)))],
    )
    .map_err(|e| fail("enable", &id, &e))?;
    Ok(format!(
        "✔ Successfully enabled plugin: {} (scope: {})",
        name_of(&id),
        scope_label(scope)
    ))
}

/// `plugin disable [plugin]` / `plugin disable --all` — set entries to `false`.
///
/// With `all`, flips every currently-`true` entry to `false` across the
/// requested scope (or all editable scopes) and reports the count. Otherwise
/// disables the single resolved id at its holding scope. Returns the `✔` line
/// or the formatted error line.
pub fn run_disable(
    plugin: Option<&str>,
    scope: Option<&str>,
    all: bool,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    let requested = parse_scope(scope)?;

    if all {
        let scopes: Vec<WritableScope> = requested.map_or_else(|| EDITABLE.to_vec(), |s| vec![s]);
        let mut count = 0usize;
        for scope in scopes {
            let path = scope_path(scope, home, cwd);
            let mut map = read_enabled(&path);
            let mut changed = false;
            for value in map.values_mut() {
                if value.as_bool() == Some(true) {
                    *value = Value::Bool(false);
                    count += 1;
                    changed = true;
                }
            }
            if changed {
                update_settings(
                    &path,
                    vec![("enabledPlugins".to_string(), Some(Value::Object(map)))],
                )
                .map_err(|e| fail("disable", "--all", &e))?;
            }
        }
        return Ok(format!("✔ Disabled {count} plugins"));
    }

    let Some(plugin) = plugin else {
        return Err(
            "✘ Failed to disable plugin: no plugin specified (pass a plugin id or --all)"
                .to_string(),
        );
    };
    let id = resolve_id(plugin, home, cwd).map_err(|reason| fail("disable", plugin, &reason))?;
    // Auto-detect: the scope where the id is currently enabled (value == true).
    let scope = match requested {
        Some(s) => s,
        None => EDITABLE
            .into_iter()
            .find(|s| current_value(&scope_path(*s, home, cwd), &id) == Some(true))
            .ok_or_else(|| {
                fail(
                    "disable",
                    &id,
                    &format!("Plugin \"{id}\" is already disabled"),
                )
            })?,
    };
    let path = scope_path(scope, home, cwd);
    if current_value(&path, &id) != Some(true) {
        return Err(fail(
            "disable",
            &id,
            &format!("Plugin \"{id}\" is already disabled"),
        ));
    }
    let project_path = scope_project_path(scope, cwd);
    let dependents = enabled_dependents(&installed_plugins(home), &id, scope, &project_path, &path);
    if !dependents.is_empty() {
        return Err(fail(
            "disable",
            &id,
            &format!(
                "Plugin \"{id}\" is required by enabled plugin(s): {}",
                dependents.join(", ")
            ),
        ));
    }
    set_value(&path, &id, false).map_err(|e| fail("disable", &id, &e))?;
    Ok(format!(
        "✔ Successfully disabled plugin: {} (scope: {})",
        name_of(&id),
        scope_label(scope)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A throwaway home + project dir pair.
    struct Env {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Env {
            _tmp: tmp,
            home,
            cwd,
        }
    }

    fn user_settings(e: &Env) -> Value {
        let raw = std::fs::read_to_string(e.home.join("settings.json")).unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn install_record(e: &Env, id: &str, dependencies: &[&str], required_by: &[&str]) {
        let install = e
            .home
            .join("plugins")
            .join("cache")
            .join(id.replace('@', "--"));
        std::fs::create_dir_all(install.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            install
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            serde_json::to_vec(&json!({
                "name": name_of(id),
                "dependencies": dependencies,
            }))
            .unwrap(),
        )
        .unwrap();
        let db_path = e.home.join("plugins").join("installed_plugins.json");
        std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
        let mut db = std::fs::read_to_string(&db_path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .unwrap_or_else(|| json!({"version":2,"plugins":{}}));
        db["plugins"][id] = json!([{
            "scope":"user",
            "installPath":install,
            "requiredBy":required_by,
        }]);
        std::fs::write(db_path, serde_json::to_vec(&db).unwrap()).unwrap();
    }

    #[test]
    fn enable_writes_true_to_user_and_reports_name() {
        let e = env();
        let msg = run_enable("foo@bar", None, &e.home, &e.cwd).unwrap();
        assert_eq!(msg, "✔ Successfully enabled plugin: foo (scope: user)");
        assert_eq!(
            user_settings(&e),
            json!({"enabledPlugins": {"foo@bar": true}})
        );
    }

    #[test]
    fn enable_preserves_other_top_level_keys() {
        let e = env();
        std::fs::write(
            e.home.join("settings.json"),
            r#"{"model":"opus","enabledPlugins":{"a@m":true}}"#,
        )
        .unwrap();
        run_enable("b@m", None, &e.home, &e.cwd).unwrap();
        let v = user_settings(&e);
        assert_eq!(v["model"], json!("opus"));
        assert_eq!(v["enabledPlugins"], json!({"a@m": true, "b@m": true}));
    }

    #[test]
    fn enable_already_enabled_errors() {
        let e = env();
        run_enable("foo@bar", None, &e.home, &e.cwd).unwrap();
        let err = run_enable("foo@bar", None, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to enable plugin \"foo@bar\": Plugin \"foo@bar\" is already enabled"
        );
    }

    #[test]
    fn enable_recursively_enables_installed_dependencies() {
        let e = env();
        install_record(&e, "app@m", &["dep"], &[]);
        install_record(&e, "dep@m", &["leaf"], &["app@m"]);
        install_record(&e, "leaf@m", &[], &["dep@m"]);

        run_enable("app@m", None, &e.home, &e.cwd).unwrap();
        assert_eq!(
            user_settings(&e)["enabledPlugins"],
            json!({"app@m":true,"dep@m":true,"leaf@m":true})
        );
    }

    #[test]
    fn disable_rejects_dependency_used_by_enabled_plugin() {
        let e = env();
        install_record(&e, "app@m", &["dep"], &[]);
        install_record(&e, "dep@m", &[], &["app@m"]);
        std::fs::write(
            e.home.join("settings.json"),
            r#"{"enabledPlugins":{"app@m":true,"dep@m":true}}"#,
        )
        .unwrap();

        let error = run_disable(Some("dep@m"), None, false, &e.home, &e.cwd).unwrap_err();
        assert!(
            error.contains("required by enabled plugin(s): app@m"),
            "{error}"
        );
        assert_eq!(user_settings(&e)["enabledPlugins"]["dep@m"], true);
    }

    #[test]
    fn enable_bare_name_not_found_errors() {
        let e = env();
        let err = run_enable("solo", None, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to enable plugin \"solo\": Plugin \"solo\" not found in any \
             editable settings scope. Use plugin@marketplace format."
        );
    }

    #[test]
    fn enable_bare_name_resolves_existing_entry() {
        let e = env();
        // Seed a disabled entry, then enable by bare name.
        std::fs::write(
            e.home.join("settings.json"),
            r#"{"enabledPlugins":{"foo@bar":false}}"#,
        )
        .unwrap();
        let msg = run_enable("foo", None, &e.home, &e.cwd).unwrap();
        assert_eq!(msg, "✔ Successfully enabled plugin: foo (scope: user)");
        assert_eq!(
            user_settings(&e),
            json!({"enabledPlugins": {"foo@bar": true}})
        );
    }

    #[test]
    fn disable_sets_false_not_delete() {
        let e = env();
        run_enable("foo@bar", None, &e.home, &e.cwd).unwrap();
        let msg = run_disable(Some("foo@bar"), None, false, &e.home, &e.cwd).unwrap();
        assert_eq!(msg, "✔ Successfully disabled plugin: foo (scope: user)");
        assert_eq!(
            user_settings(&e),
            json!({"enabledPlugins": {"foo@bar": false}})
        );
    }

    #[test]
    fn disable_absent_is_already_disabled() {
        let e = env();
        let err = run_disable(Some("neverset@x"), None, false, &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "✘ Failed to disable plugin \"neverset@x\": Plugin \"neverset@x\" is already disabled"
        );
    }

    #[test]
    fn disable_all_flips_true_entries_and_counts() {
        let e = env();
        run_enable("a@m", None, &e.home, &e.cwd).unwrap();
        run_enable("b@m", None, &e.home, &e.cwd).unwrap();
        let msg = run_disable(None, None, true, &e.home, &e.cwd).unwrap();
        assert_eq!(msg, "✔ Disabled 2 plugins");
        assert_eq!(
            user_settings(&e),
            json!({"enabledPlugins": {"a@m": false, "b@m": false}})
        );
    }

    #[test]
    fn scope_local_and_project_paths() {
        let e = env();
        run_enable("loc@m", Some("local"), &e.home, &e.cwd).unwrap();
        run_enable("prj@m", Some("project"), &e.home, &e.cwd).unwrap();
        let local: Value = serde_json::from_str(
            &std::fs::read_to_string(e.cwd.join(".lingxi").join("settings.local.json")).unwrap(),
        )
        .unwrap();
        let project: Value = serde_json::from_str(
            &std::fs::read_to_string(e.cwd.join(".lingxi").join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(local, json!({"enabledPlugins": {"loc@m": true}}));
        assert_eq!(project, json!({"enabledPlugins": {"prj@m": true}}));
    }

    #[test]
    fn invalid_scope_errors() {
        let e = env();
        let err = run_enable("a@b", Some("bogus"), &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Invalid scope \"bogus\". Valid scopes: user, project, local"
        );
    }

    /// `managed` is a real rung elsewhere in the engine but is deliberately NOT
    /// a `--scope` value here: a plugin cannot be enabled into the
    /// administrator-managed tier. Pinned separately from the `bogus` case
    /// because a wider parse would read as a typo fix rather than the
    /// privilege change it is.
    #[test]
    fn managed_is_rejected_as_a_scope_like_any_unknown_value() {
        assert_eq!(parse_scope_str("managed"), None);

        let e = env();
        let err = run_enable("a@b", Some("managed"), &e.home, &e.cwd).unwrap_err();
        assert_eq!(
            err,
            "Invalid scope \"managed\". Valid scopes: user, project, local"
        );
    }

    /// Pins the three `(scope: …)` suffix spellings. `enable_writes_true_to_user_and_reports_name`
    /// covers `user` through the whole message; this pins the other two so a
    /// relabel cannot slip through on the paths that test does not walk.
    #[test]
    fn scope_labels_match_the_cli_suffix_spellings() {
        assert_eq!(scope_label(WritableScope::User), "user");
        assert_eq!(scope_label(WritableScope::Project), "project");
        assert_eq!(scope_label(WritableScope::Local), "local");
    }
}
