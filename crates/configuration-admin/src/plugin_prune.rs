//! `plugin prune` (alias `autoremove`) — remove auto-installed plugin
//! dependencies that are no longer reachable from any explicitly-installed
//! plugin, 1:1 with claude-code 2.1.201 (probed against the real binary in an
//! isolated `$CLAUDE_CONFIG_DIR`).
//!
//! A v2 `installed_plugins.json` record carries an `autoInstalled: true` marker
//! ("True when pulled in as a dependency. Eligible for orphan sweep."). Prune
//! computes, at the chosen scope, the set of `auto`-installed plugins that are
//! NOT reachable — via the `manifest.dependencies` graph — from any
//! non-`auto` (explicitly installed) plugin, and removes them.
//!
//! Output (branding-adjusted — the oracle's `claude` → `lingxi-cli`):
//!
//! * unresolvable graph (some record's plugin fails to load) →
//!   `Skipped — cannot determine orphans: <ids> failed to load. Fix or uninstall, then retry.`
//! * nothing auto-installed → `Nothing to prune (no auto-installed plugins at <scope> scope).`
//! * auto-installed present but all still needed →
//!   `Nothing to prune (N auto-installed plugin(s) at <scope> scope, all still needed).`
//! * orphans found →
//!   ```text
//!   N auto-installed plugin(s) no longer needed at <scope> scope:
//!     <id> (<version>)
//!   ```
//!   then, per mode: `\n(dry run — nothing removed)` (`--dry-run`);
//!   `\nNot a TTY — run \`lingxi-cli plugin prune[ --scope S] -y\` to remove.`
//!   (non-interactive without `-y`); an interactive `Remove? [y/N]` prompt
//!   (`Aborted.` on decline); otherwise the removal runs and returns
//!   `Removed N auto-installed plugin(s): <names>`.
//!
//! An unknown `--scope` yields the install-family wording
//! `Invalid scope: <s>. Must be one of: user, project, local.`
//!
//! Legacy `auto` and snake-case `auto_installed` markers remain readable.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use migrations::settings_update::read_settings_map;
use serde_json::{Map, Value};
use telemetry::AnalyticsBus;

use crate::plugin_settings::{parse_scope_str, scope_label, scope_path};
use protocol::WritableScope;

/// `<plugins>/installed_plugins.json` path.
#[cfg(test)]
fn installed_path(plugins_dir: &Path) -> PathBuf {
    plugins_dir.join("installed_plugins.json")
}

/// Load the v2 installed DB (`{version:2, plugins:{...}}`); missing/malformed ⇒
/// a fresh empty v2 doc.
fn load_installed(plugins_dir: &Path) -> Value {
    super::plugin_install::load_installed(plugins_dir)
}

/// Parse the prune `--scope` (default `user`); invalid-scope wording matches the
/// install family (`Invalid scope: <s>. Must be one of: user, project, local.`).
fn parse_scope(scope: &str) -> Result<WritableScope, String> {
    parse_scope_str(scope)
        .ok_or_else(|| format!("Invalid scope: {scope}. Must be one of: user, project, local."))
}

/// The scope's `projectPath` selector: `Some(cwd)` for project/local,
/// `None` for user (mirrors the oracle's `Svt`, which returns the originalCwd
/// for project/local and `undefined` for user).
fn scope_project_path(scope: WritableScope, cwd: &Path) -> Option<String> {
    match scope {
        WritableScope::User => None,
        WritableScope::Project | WritableScope::Local => Some(cwd.display().to_string()),
    }
}

/// `singular`/`plural` selector (mirrors the oracle's `on(count, s, p)`).
fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else {
        many
    }
}

/// The pre-`@` display name of a `plugin@marketplace` id (mirrors `zo(id).name`).
fn name_of(id: &str) -> &str {
    id.split('@').next().unwrap_or(id)
}

/// The marketplace segment of a `plugin@marketplace` id (the SECOND `@`-part,
/// matching the oracle's `zo`, which keys off `split("@")[1]`).
fn marketplace_of(id: &str) -> Option<&str> {
    id.split('@').nth(1)
}

/// Resolve a bare dependency spec to a full id in the context of its declaring
/// plugin's id (mirrors the oracle's `e1`): a spec that already names a
/// marketplace is returned verbatim; otherwise it inherits the source plugin's
/// marketplace — unless that marketplace is a non-catalog sentinel
/// (`inline` / `skills-dir`), in which case the bare spec is kept.
fn resolve_dep(dep: &str, source_id: &str) -> String {
    if marketplace_of(dep).is_some() {
        return dep.to_string();
    }
    match marketplace_of(source_id) {
        Some(m) if m != "inline" && m != "skills-dir" => format!("{dep}@{m}"),
        _ => dep.to_string(),
    }
}

/// The record within an id's record-array that matches the active scope +
/// projectPath (mirrors `find(u => u.scope===scope && u.projectPath===pp)`).
fn scoped_record<'a>(
    records: &'a [Value],
    scope: WritableScope,
    project_path: &Option<String>,
) -> Option<&'a Value> {
    records.iter().find(|r| {
        let rec_scope = r.get("scope").and_then(Value::as_str);
        if rec_scope != Some(scope_label(scope)) {
            return false;
        }
        let rec_pp = r.get("projectPath").and_then(Value::as_str);
        match project_path {
            Some(p) => rec_pp == Some(p.as_str()),
            None => rec_pp.is_none(),
        }
    })
}

/// The outcome of the orphan scan (mirrors the oracle's `Ria`).
struct Scan {
    /// Auto-installed ids that are no longer reachable (in DB order).
    orphans: Vec<String>,
    /// Ids whose plugin failed to load (blocks any pruning when non-empty).
    unloadable: Vec<String>,
    /// Total number of auto-installed plugins at the scope.
    auto_count: usize,
}

/// The plugins object of the DB (`{id: [record, …]}`), or an empty map.
fn plugins_obj(db: &Value) -> Map<String, Value> {
    db.get("plugins")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// A plugin's declared dependency specs, read from its `installPath` manifest
/// (`<installPath>/.lingxi-plugin/plugin.json` → `dependencies: []`). `None`
/// when the manifest is missing or unreadable (⇒ the plugin failed to load).
fn load_dependencies(install_path: &str) -> Option<Vec<String>> {
    let manifest = Path::new(install_path)
        .join(branding::PLUGIN_MANIFEST_DIR)
        .join("plugin.json");
    let raw = std::fs::read_to_string(manifest).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    // `dependencies` is optional; an absent key is an empty dependency list (a
    // loadable plugin), NOT a load failure.
    let deps = plugin::parse_dependencies(value.get("dependencies"))
        .ok()?
        .into_iter()
        .map(|dependency| match dependency.marketplace {
            Some(marketplace) => format!("{}@{marketplace}", dependency.name),
            None => dependency.name,
        })
        .collect();
    Some(deps)
}

/// Compute the orphan set at `scope` (mirrors `Ria`): auto-installed plugins
/// unreachable from any manual plugin via the `dependencies` graph.
fn scan(db: &Value, scope: WritableScope, project_path: &Option<String>) -> Scan {
    let plugins = plugins_obj(db);

    // Partition the scope's records into manual (`o`) and auto (`s`), in DB order.
    let mut manual: Vec<String> = Vec::new();
    let mut auto: Vec<String> = Vec::new();
    for (id, records) in &plugins {
        let arr = match records.as_array() {
            Some(a) => a,
            None => continue,
        };
        let rec = match scoped_record(arr, scope, project_path) {
            Some(r) => r,
            None => continue,
        };
        let auto_installed = ["autoInstalled", "auto_installed", "auto"]
            .iter()
            .any(|key| rec.get(*key).and_then(Value::as_bool) == Some(true));
        if auto_installed {
            auto.push(id.clone());
        } else {
            manual.push(id.clone());
        }
    }

    if auto.is_empty() {
        return Scan {
            orphans: Vec::new(),
            unloadable: Vec::new(),
            auto_count: 0,
        };
    }
    let auto_count = auto.len();

    // Load every in-scope plugin's dependency list from its installPath manifest.
    // Any that fails to load blocks pruning entirely.
    let mut loaded: HashMap<String, Vec<String>> = HashMap::new();
    let mut unloadable: Vec<String> = Vec::new();
    for id in manual.iter().chain(auto.iter()) {
        let install_path = plugins
            .get(id)
            .and_then(Value::as_array)
            .and_then(|a| scoped_record(a, scope, project_path))
            .and_then(|r| r.get("installPath").and_then(Value::as_str))
            .unwrap_or("");
        match load_dependencies(install_path) {
            Some(deps) => {
                loaded.insert(id.clone(), deps);
            }
            None => unloadable.push(id.clone()),
        }
    }
    if !unloadable.is_empty() {
        return Scan {
            orphans: Vec::new(),
            unloadable,
            auto_count,
        };
    }

    // Reachability DFS from every manual plugin, following resolved dependencies.
    let mut reachable: HashSet<String> = HashSet::new();
    for id in &manual {
        dfs(id, &loaded, &mut reachable);
    }

    // Orphans = auto plugins not reached (DB order preserved).
    let orphans: Vec<String> = auto
        .into_iter()
        .filter(|id| !reachable.contains(id))
        .collect();

    Scan {
        orphans,
        unloadable: Vec::new(),
        auto_count,
    }
}

/// Mark `id` and everything it transitively depends on as reachable.
fn dfs(id: &str, loaded: &HashMap<String, Vec<String>>, reachable: &mut HashSet<String>) {
    if reachable.contains(id) {
        return;
    }
    reachable.insert(id.to_string());
    if let Some(deps) = loaded.get(id) {
        for dep in deps {
            let resolved = resolve_dep(dep, id);
            dfs(&resolved, loaded, reachable);
        }
    }
}

/// The orphan ids [`scan`] finds at `scope`, or an empty list when the
/// dependency graph is unresolvable — mirrors the silent-skip the oracle's
/// `plugin uninstall` post-removal orphan check takes rather than surfacing a
/// scary "cannot determine orphans" error after a successful uninstall.
pub fn scan_orphans(
    db: &Value,
    scope: WritableScope,
    project_path: &Option<String>,
) -> Vec<String> {
    let result = scan(db, scope, project_path);
    if result.unloadable.is_empty() {
        result.orphans
    } else {
        Vec::new()
    }
}

/// §22: the proactive notice `plugin uninstall` appends (mirrors oracle
/// `QWn`) when removing a plugin leaves auto-installed dependencies newly
/// unreachable and the caller did NOT pass `--prune` to remove them
/// immediately. Empty when there is nothing to report — the caller appends
/// this verbatim, so an empty orphan set contributes no trailing text.
///
/// Lists up to 5 orphan names (by their pre-`@` display name), then `, …`
/// when there are more; `scope_label` "user" contributes no `--scope` suffix
/// to the suggested command (matching every other `user`-is-the-default
/// wording in this file).
pub fn orphan_notice(orphans: &[String], scope_label: &str) -> String {
    const MAX_NAMES: usize = 5;
    if orphans.is_empty() {
        return String::new();
    }
    let names: Vec<&str> = orphans.iter().map(|id| name_of(id)).collect();
    let listed = if names.len() <= MAX_NAMES {
        names.join(", ")
    } else {
        format!("{}, \u{2026}", names[..MAX_NAMES].join(", "))
    };
    let suffix = if scope_label == "user" {
        String::new()
    } else {
        format!(" --scope {scope_label}")
    };
    format!(
        "\n{} auto-installed {} no longer needed: {listed}. Run `lingxi-cli plugin prune{suffix}` \
         to remove.",
        orphans.len(),
        plural(orphans.len(), "dependency", "dependencies"),
    )
}

/// Read a scope's `enabledPlugins` map.
fn read_enabled(path: &Path) -> Map<String, Value> {
    read_settings_map(path)
        .ok()
        .and_then(|m| m.get("enabledPlugins").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// Remove the orphan records at the scope in-memory; the caller persists first,
/// then applies any on-disk cleanup only after the new references are durable.
fn collect_scoped_orphan_records(
    db: &Value,
    orphans: &[String],
    scope: WritableScope,
    project_path: &Option<String>,
) -> Vec<(String, Value)> {
    let mut removed = Vec::new();
    for id in orphans {
        if let Some(record) = db
            .get("plugins")
            .and_then(|p| p.get(id))
            .and_then(Value::as_array)
            .and_then(|arr| scoped_record(arr, scope, project_path))
            .cloned()
        {
            removed.push((id.clone(), record));
        }
    }
    removed
}

fn drop_orphan_records(
    db: &mut Value,
    orphans: &[String],
    scope: WritableScope,
    project_path: &Option<String>,
) {
    for id in orphans {
        if let Some(plugins) = db.get_mut("plugins").and_then(Value::as_object_mut) {
            if let Some(Value::Array(arr)) = plugins.get_mut(id) {
                arr.retain(|r| {
                    scoped_record(std::slice::from_ref(r), scope, project_path).is_none()
                });
                if arr.is_empty() {
                    plugins.remove(id);
                }
            }
        }
    }
}

/// Read a `y`/`yes` (case-insensitive) confirmation line from stdin.
fn read_yes_from_stdin() -> bool {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn registry_error_kind(error: &str) -> &'static str {
    if error.contains("expected value")
        || error.contains("EOF while parsing")
        || error.contains("trailing characters")
        || error.contains("key must be a string")
    {
        "parse"
    } else if error.contains("lock") {
        "lock"
    } else {
        "io"
    }
}

/// `plugin prune [--dry-run] [-y] [--scope S]`.
///
/// Returns the message to print (`Ok`) or the formatted error line (`Err`).
pub fn run_prune(
    dry_run: bool,
    yes: bool,
    scope: &str,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
) -> Result<String, String> {
    crate::plugin_telemetry::current_thread_runtime().block_on(run_prune_with_bus(
        dry_run,
        yes,
        scope,
        plugins_dir,
        home,
        cwd,
        None,
    ))
}

pub async fn run_prune_with_bus(
    dry_run: bool,
    yes: bool,
    scope: &str,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    let is_tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    run_prune_inner_with_bus(
        dry_run,
        yes,
        scope,
        plugins_dir,
        home,
        cwd,
        is_tty,
        &mut read_yes_from_stdin,
        analytics_bus,
    )
    .await
}

/// Testable core: `is_tty` and the confirmation reader are injected so the
/// interactive path can be exercised deterministically.
#[allow(clippy::too_many_arguments)]
fn run_prune_inner(
    dry_run: bool,
    yes: bool,
    scope_str: &str,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    is_tty: bool,
    confirm: &mut dyn FnMut() -> bool,
) -> Result<String, String> {
    crate::plugin_telemetry::current_thread_runtime().block_on(run_prune_inner_with_bus(
        dry_run,
        yes,
        scope_str,
        plugins_dir,
        home,
        cwd,
        is_tty,
        confirm,
        None,
    ))
}

async fn run_prune_inner_with_bus(
    dry_run: bool,
    yes: bool,
    scope_str: &str,
    plugins_dir: &Path,
    home: &Path,
    cwd: &Path,
    is_tty: bool,
    confirm: &mut dyn FnMut() -> bool,
    analytics_bus: Option<&Arc<AnalyticsBus>>,
) -> Result<String, String> {
    let scope = parse_scope(scope_str)?;
    let label = scope_label(scope);
    let project_path = scope_project_path(scope, cwd);
    let settings_path = scope_path(scope, home, cwd);
    let previous_settings = std::fs::read(&settings_path).ok();
    let mut installed_tx = match plugin::installed::InstalledRegistryTransaction::begin(plugins_dir)
    {
        Ok(tx) => tx,
        Err(error) => {
            crate::plugin_telemetry::emit_plugin_state_file_error(
                analytics_bus,
                "prune",
                "transaction_begin",
                registry_error_kind(&error),
            )
            .await;
            return Err(error);
        }
    };
    let result = scan(installed_tx.document(), scope, &project_path);

    // Unresolvable graph — cannot safely determine orphans.
    if !result.unloadable.is_empty() {
        return Ok(format!(
            "Skipped \u{2014} cannot determine orphans: {} failed to load. Fix or uninstall, then retry.",
            result.unloadable.join(", ")
        ));
    }

    // Nothing to remove.
    if result.orphans.is_empty() {
        if result.auto_count == 0 {
            return Ok(format!(
                "Nothing to prune (no auto-installed plugins at {label} scope)."
            ));
        }
        return Ok(format!(
            "Nothing to prune ({} auto-installed {} at {label} scope, all still needed).",
            result.auto_count,
            plural(result.auto_count, "plugin", "plugins"),
        ));
    }

    // Build the orphan listing (2-space indent; version from the scoped record).
    let plugins = plugins_obj(installed_tx.document());
    let lines: Vec<String> = result
        .orphans
        .iter()
        .map(|id| {
            let version = plugins
                .get(id)
                .and_then(Value::as_array)
                .and_then(|a| scoped_record(a, scope, &project_path))
                .and_then(|r| r.get("version").and_then(Value::as_str))
                .filter(|v| !v.is_empty());
            match version {
                Some(v) => format!("  {id} ({v})"),
                None => format!("  {id}"),
            }
        })
        .collect();
    let listing = format!(
        "{} auto-installed {} no longer needed at {label} scope:\n{}",
        result.orphans.len(),
        plural(result.orphans.len(), "plugin", "plugins"),
        lines.join("\n"),
    );

    if dry_run {
        return Ok(format!("{listing}\n(dry run \u{2014} nothing removed)"));
    }

    if !yes {
        if !is_tty {
            let scope_flag = if scope == WritableScope::User {
                String::new()
            } else {
                format!(" --scope {label}")
            };
            return Ok(format!(
                "{listing}\nNot a TTY \u{2014} run `lingxi-cli plugin prune{scope_flag} -y` to remove."
            ));
        }
        print!("{listing}\nRemove? [y/N] ");
        let _ = std::io::stdout().flush();
        if !confirm() {
            return Ok("Aborted.".to_string());
        }
    }

    let result = async {
        if !result.unloadable.is_empty() {
            return Ok(format!(
            "Skipped \u{2014} cannot determine orphans: {} failed to load. Fix or uninstall, then retry.",
            result.unloadable.join(", ")
        ));
        }
        if result.orphans.is_empty() {
            if result.auto_count == 0 {
                return Ok(format!(
                    "Nothing to prune (no auto-installed plugins at {label} scope)."
                ));
            }
            return Ok(format!(
                "Nothing to prune ({} auto-installed {} at {label} scope, all still needed).",
                result.auto_count,
                plural(result.auto_count, "plugin", "plugins"),
            ));
        }

        let removed = result.orphans.clone();
        let removed_records =
            collect_scoped_orphan_records(installed_tx.document(), &removed, scope, &project_path);
        drop_orphan_records(installed_tx.document_mut(), &removed, scope, &project_path);
        installed_tx.persist()?;
        for id in &removed {
            super::plugin_install::edit_enabled_strict(scope, home, cwd, id, None)?;
        }

        let orphaned_paths = removed_records
            .iter()
            .map(|(_, record)| record.clone())
            .collect::<Vec<_>>();
        for path in super::plugin_install::unreferenced_removed_record_paths(
            plugins_dir,
            installed_tx.document(),
            &orphaned_paths,
        ) {
            let _ = std::fs::remove_dir_all(path);
        }
        for id in &removed {
            if !super::plugin_install::plugin_has_records(installed_tx.document(), id) {
                if let Some(path) =
                    super::plugin_install::confined_plugin_data_path(plugins_dir, id)
                {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
        }

        crate::plugin_telemetry::emit_plugin_prune_cli(analytics_bus, scope_label(scope), removed.len() as u64)
            .await;

        Ok(format!(
            "Removed {} auto-installed {}: {}",
            removed.len(),
            plural(removed.len(), "plugin", "plugins"),
            removed
                .iter()
                .map(|id| name_of(id).to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ))
    }
    .await;
    if result.is_err() {
        super::plugin_install::restore_file(&settings_path, previous_settings.as_deref());
        super::plugin_install::rollback_installed_registry(&installed_tx);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        cwd: PathBuf,
        plugins: PathBuf,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("proj");
        let plugins = home.join("plugins");
        for d in [&home, &cwd, &plugins] {
            std::fs::create_dir_all(d).unwrap();
        }
        Env {
            _tmp: tmp,
            home,
            cwd,
            plugins,
        }
    }

    /// Materialize a plugin's installPath manifest (`.lingxi-plugin/plugin.json`)
    /// with the given name/version/dependencies, and return the installPath.
    fn materialize(e: &Env, id: &str, version: &str, deps: &[&str]) -> String {
        let path = e.plugins.join("cache").join(id.replace('@', "__"));
        std::fs::create_dir_all(path.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        let manifest = serde_json::json!({
            "name": name_of(id),
            "version": version,
            "dependencies": deps,
        });
        std::fs::write(
            path.join(branding::PLUGIN_MANIFEST_DIR).join("plugin.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
        path.display().to_string()
    }

    /// Write a v2 installed DB from `(id, version, installPath, auto)` records
    /// (all at `user` scope).
    fn write_db(e: &Env, records: &[(&str, &str, &str, bool)]) {
        let mut plugins = serde_json::Map::new();
        for (id, version, install_path, auto) in records {
            plugins.insert(
                (*id).to_string(),
                serde_json::json!([{
                    "scope": "user",
                    "installPath": install_path,
                    "version": version,
                    "installedAt": "2026-01-01T00:00:00.000Z",
                    "lastUpdated": "2026-01-01T00:00:00.000Z",
                    "auto": auto,
                }]),
            );
        }
        std::fs::write(
            installed_path(&e.plugins),
            serde_json::to_string_pretty(&serde_json::json!({"version": 2, "plugins": plugins}))
                .unwrap(),
        )
        .unwrap();
    }

    fn db_json(e: &Env) -> Value {
        serde_json::from_str(&std::fs::read_to_string(installed_path(&e.plugins)).unwrap()).unwrap()
    }

    fn write_legacy_only_db(e: &Env, doc: &Value) {
        std::fs::write(
            e.plugins.join("installed_plugins_v2.json"),
            serde_json::to_string(doc).unwrap(),
        )
        .unwrap();
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn empty_case_no_auto_installed() {
        let e = env();
        let msg = run_prune_inner(
            false, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "Nothing to prune (no auto-installed plugins at user scope)."
        );
    }

    #[test]
    fn prune_never_deletes_install_paths_outside_plugin_cache() {
        let e = env();
        let outside = e._tmp.path().join("must-survive");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), "keep").unwrap();
        write_db(
            &e,
            &[("forged@mkt", "1.0.0", outside.to_str().unwrap(), true)],
        );

        run_prune_inner(
            true, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();

        assert!(outside.join("sentinel").exists());
    }

    #[cfg(unix)]
    #[test]
    fn prune_rejects_a_symlinked_plugin_cache_root() {
        use std::os::unix::fs::symlink;

        let e = env();
        let outside = e._tmp.path().join("outside-cache");
        let forged = outside.join("forged");
        std::fs::create_dir_all(&forged).unwrap();
        std::fs::write(forged.join("sentinel"), "keep").unwrap();
        symlink(&outside, e.plugins.join("cache")).unwrap();
        write_db(
            &e,
            &[(
                "forged@mkt",
                "1.0.0",
                e.plugins.join("cache/forged").to_str().unwrap(),
                true,
            )],
        );

        run_prune_inner(
            true, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();

        assert!(forged.join("sentinel").exists());
    }

    #[test]
    fn empty_case_dry_run_and_scope_wording() {
        let e = env();
        let msg = run_prune_inner(
            true, false, "project", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "Nothing to prune (no auto-installed plugins at project scope)."
        );
    }

    #[test]
    fn invalid_scope() {
        let e = env();
        let err = run_prune_inner(
            false, false, "bogus", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap_err();
        assert_eq!(
            err,
            "Invalid scope: bogus. Must be one of: user, project, local."
        );
    }

    #[test]
    fn all_still_needed_when_reachable() {
        let e = env();
        // Manual `app@mkt` depends on auto `dep@mkt`; dep is reachable ⇒ kept.
        let app = materialize(&e, "app@mkt", "2.0.0", &["dep"]);
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(
            &e,
            &[
                ("app@mkt", "2.0.0", &app, false),
                ("dep@mkt", "1.0.0", &dep, true),
            ],
        );
        let msg = run_prune_inner(
            false, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "Nothing to prune (1 auto-installed plugin at user scope, all still needed)."
        );
    }

    #[test]
    fn typed_dependency_and_auto_installed_marker_are_supported() {
        let e = env();
        let app_path = e.plugins.join("cache").join("app");
        std::fs::create_dir_all(app_path.join(branding::PLUGIN_MANIFEST_DIR)).unwrap();
        std::fs::write(
            app_path
                .join(branding::PLUGIN_MANIFEST_DIR)
                .join("plugin.json"),
            r#"{"name":"app","dependencies":[{"name":"dep","version":"^1"}]}"#,
        )
        .unwrap();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        std::fs::write(
            installed_path(&e.plugins),
            serde_json::to_vec(&serde_json::json!({"version":2,"plugins":{
                "app@mkt":[{"scope":"user","installPath":app_path}],
                "dep@mkt":[{"scope":"user","installPath":dep,"autoInstalled":true}]
            }}))
            .unwrap(),
        )
        .unwrap();

        let msg = run_prune_inner(
            false, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert!(msg.contains("all still needed"), "{msg}");
    }

    #[test]
    fn dry_run_lists_orphan_without_removing() {
        let e = env();
        // Auto `dep@mkt` with no manual dependant ⇒ orphan.
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(&e, &[("dep@mkt", "1.0.0", &dep, true)]);
        let msg = run_prune_inner(
            true, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "1 auto-installed plugin no longer needed at user scope:\n  dep@mkt (1.0.0)\n(dry run \u{2014} nothing removed)"
        );
        // Nothing removed.
        assert!(db_json(&e)["plugins"].get("dep@mkt").is_some());
        assert!(Path::new(&dep).exists());
    }

    #[test]
    fn non_tty_without_yes_reports_how_to_remove() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(&e, &[("dep@mkt", "1.0.0", &dep, true)]);
        let msg = run_prune_inner(
            false, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "1 auto-installed plugin no longer needed at user scope:\n  dep@mkt (1.0.0)\nNot a TTY \u{2014} run `lingxi-cli plugin prune -y` to remove."
        );
        // Nothing removed.
        assert!(db_json(&e)["plugins"].get("dep@mkt").is_some());
    }

    #[test]
    fn unloadable_blocks_prune() {
        let e = env();
        // installPath manifest missing ⇒ failed to load.
        write_db(&e, &[("dep@mkt", "1.0.0", "/nonexistent/dep", true)]);
        let msg = run_prune_inner(
            false, false, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(
            msg,
            "Skipped \u{2014} cannot determine orphans: dep@mkt failed to load. Fix or uninstall, then retry."
        );
    }

    #[test]
    fn yes_removes_orphan_and_cleans_state() {
        let e = env();
        // Seed enabledPlugins so we can assert the key is dropped.
        std::fs::write(
            e.home.join("settings.json"),
            r#"{"enabledPlugins":{"dep@mkt":true,"keep@mkt":true}}"#,
        )
        .unwrap();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(&e, &[("dep@mkt", "1.0.0", &dep, true)]);

        let msg = run_prune_inner(
            false, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();
        assert_eq!(msg, "Removed 1 auto-installed plugin: dep");
        // Record dropped, cache deleted, enabledPlugins key removed (others kept).
        assert!(db_json(&e)["plugins"].get("dep@mkt").is_none());
        assert!(!Path::new(&dep).exists());
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(e.home.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(
            settings["enabledPlugins"],
            serde_json::json!({"keep@mkt": true})
        );
    }

    #[test]
    fn prune_keeps_shared_cache_and_data_when_another_scope_still_references_it() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        let data_dir = e.plugins.join("data").join("dep-mkt");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("sentinel"), "keep").unwrap();
        std::fs::write(
            installed_path(&e.plugins),
            serde_json::to_string_pretty(&serde_json::json!({
                "version": 2,
                "plugins": {
                    "dep@mkt": [
                        {
                            "scope": "user",
                            "installPath": dep,
                            "version": "1.0.0",
                            "installedAt": "2026-01-01T00:00:00.000Z",
                            "lastUpdated": "2026-01-01T00:00:00.000Z",
                            "auto": true
                        },
                        {
                            "scope": "project",
                            "projectPath": e.cwd.display().to_string(),
                            "installPath": dep,
                            "version": "1.0.0",
                            "installedAt": "2026-01-01T00:00:00.000Z",
                            "lastUpdated": "2026-01-01T00:00:00.000Z",
                            "auto": true
                        }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        run_prune_inner(
            false, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap();

        let records = db_json(&e)["plugins"]["dep@mkt"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["scope"], "project");
        assert!(Path::new(&dep).exists());
        assert!(data_dir.join("sentinel").exists());
    }

    #[test]
    fn prune_settings_failure_restores_registry_and_skips_cleanup() {
        let e = env();
        std::fs::write(
            e.home.join("settings.json"),
            r#"{"enabledPlugins":{"dep@mkt":true}}"#,
        )
        .unwrap();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        let data_dir = e.plugins.join("data").join("dep-mkt");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join("sentinel"), "keep").unwrap();
        write_db(&e, &[("dep@mkt", "1.0.0", &dep, true)]);
        std::fs::remove_file(e.home.join("settings.json")).unwrap();
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();

        let err = run_prune_inner(
            false, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap_err();

        assert!(!err.is_empty());
        assert!(db_json(&e)["plugins"].get("dep@mkt").is_some());
        assert!(Path::new(&dep).exists());
        assert!(data_dir.join("sentinel").exists());
    }

    #[test]
    fn prune_failure_restores_legacy_only_registry() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        let legacy = serde_json::json!({
            "version": 2,
            "plugins": {
                "dep@mkt": [{
                    "scope": "user",
                    "installPath": dep,
                    "version": "1.0.0",
                    "installedAt": "2026-01-01T00:00:00.000Z",
                    "lastUpdated": "2026-01-01T00:00:00.000Z",
                    "auto": true
                }]
            }
        });
        write_legacy_only_db(&e, &legacy);
        std::fs::create_dir_all(e.home.join("settings.json")).unwrap();

        let err = run_prune_inner(
            false, true, "user", &e.plugins, &e.home, &e.cwd, false, &mut never,
        )
        .unwrap_err();

        assert!(!err.is_empty());
        assert!(!installed_path(&e.plugins).exists());
        assert_eq!(
            serde_json::from_str::<Value>(
                &std::fs::read_to_string(e.plugins.join("installed_plugins_v2.json")).unwrap()
            )
            .unwrap(),
            legacy
        );
    }

    #[test]
    fn interactive_decline_aborts() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(&e, &[("dep@mkt", "1.0.0", &dep, true)]);
        let mut declined = || false;
        let msg = run_prune_inner(
            false,
            false,
            "user",
            &e.plugins,
            &e.home,
            &e.cwd,
            true, // TTY ⇒ prompt path
            &mut declined,
        )
        .unwrap();
        assert_eq!(msg, "Aborted.");
        assert!(db_json(&e)["plugins"].get("dep@mkt").is_some());
    }

    // --- §22: the proactive `plugin uninstall` orphan notice --------------

    #[test]
    fn orphan_notice_is_empty_when_there_are_no_orphans() {
        assert_eq!(orphan_notice(&[], "user"), "");
    }

    /// Oracle `QWn`: singular noun, no `--scope` suffix at the default
    /// (`user`) scope.
    #[test]
    fn orphan_notice_singular_at_user_scope() {
        assert_eq!(
            orphan_notice(&["dep@mkt".to_string()], "user"),
            "\n1 auto-installed dependency no longer needed: dep. Run `lingxi-cli plugin prune` \
             to remove."
        );
    }

    /// Plural noun, up to 5 names listed, `--scope` suffix for a non-`user`
    /// scope.
    #[test]
    fn orphan_notice_plural_with_scope_suffix() {
        let orphans = vec!["a@m".to_string(), "b@m".to_string()];
        assert_eq!(
            orphan_notice(&orphans, "project"),
            "\n2 auto-installed dependencies no longer needed: a, b. Run `lingxi-cli plugin \
             prune --scope project` to remove."
        );
    }

    /// More than 5 orphans truncate to 5 names plus a `, …` ellipsis (oracle
    /// `r.slice(0,o).join(", "), …`).
    #[test]
    fn orphan_notice_truncates_past_five_names() {
        let orphans: Vec<String> = (1..=6).map(|n| format!("dep{n}@m")).collect();
        let msg = orphan_notice(&orphans, "user");
        assert_eq!(
            msg,
            "\n6 auto-installed dependencies no longer needed: dep1, dep2, dep3, dep4, dep5, \
             \u{2026}. Run `lingxi-cli plugin prune` to remove."
        );
    }

    /// [`scan_orphans`] surfaces the same orphan set `run_prune` would remove.
    #[test]
    fn scan_orphans_matches_a_real_orphan() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        let app = materialize(&e, "app@mkt", "1.0.0", &[]);
        write_db(
            &e,
            &[
                ("app@mkt", "1.0.0", &app, false),
                ("dep@mkt", "1.0.0", &dep, true),
            ],
        );
        let db = load_installed(&e.plugins);
        let orphans = scan_orphans(&db, WritableScope::User, &None);
        assert_eq!(orphans, vec!["dep@mkt".to_string()]);
    }

    /// An unresolvable graph (a manual plugin's manifest failed to load)
    /// degrades to an empty orphan list rather than surfacing `run_prune`'s
    /// "cannot determine orphans" error inline after a successful uninstall.
    #[test]
    fn scan_orphans_is_empty_when_the_graph_is_unresolvable() {
        let e = env();
        let dep = materialize(&e, "dep@mkt", "1.0.0", &[]);
        write_db(
            &e,
            &[
                ("app@mkt", "1.0.0", "/does/not/exist", false),
                ("dep@mkt", "1.0.0", &dep, true),
            ],
        );
        let db = load_installed(&e.plugins);
        assert_eq!(
            scan_orphans(&db, WritableScope::User, &None),
            Vec::<String>::new()
        );
    }
}
