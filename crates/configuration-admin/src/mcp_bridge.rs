//! MCP server definition read/write.
//!
//! MCP does **not** use the settings-layer machinery in
//! [`crate::settings_bridge`] — it has its own three storage locations
//! (`mcp/src/json_config.rs` is the parser for all of them):
//!
//! | scope     | location                                          |
//! |-----------|----------------------------------------------------|
//! | `User`    | `~/.lingxi.json`, top-level `mcpServers`            |
//! | `Local`   | `~/.lingxi.json`, under `projects[<cwd>].mcpServers`|
//! | `Project` | `<project>/.mcp.json`                               |
//!
//! (plus read-only `Dynamic` (plugins) and `Enterprise` (managed policy) —
//! [`WritableScopeDto`] omits both since nothing user-initiated ever writes them.)
//!
//! The read side (`RefreshListings{Mcp}` → `ClientEvent::McpServers`) is
//! already wired through the live `McpRegistry` snapshot
//! (`router.rs`'s `ClientCommand::RefreshListings` arm calling
//! `handle.list_mcp_servers()`); this module is the WRITE side only —
//! [`upsert_server`] / [`remove_server`], routed from
//! [`client::protocol::commands::ClientCommand::UpsertMcpServer`] /
//! `RemoveMcpServer`.
//!
//! `User` and `Local` both live inside the SAME `~/.lingxi.json` file, which
//! also carries dozens of unrelated keys (`numStartups`, `oauthAccount`,
//! `projects.<key>.allowedTools`, …). Rather than reimplementing that file's
//! locked, atomic, unknown-key-preserving read-modify-write here, this module
//! reuses the SAME canonical writer the rest of the port already uses for it —
//! [`migrations::global_config::save_map`] (top-level keys) and
//! [`migrations::global_config::save_project_config`] (the
//! `projects[<key>]` sub-object) — rather than growing a second,
//! divergent implementation of that file's write contract. `Project` has no
//! such shared writer (`.mcp.json` is a plain, unlocked file, matching
//! `settings_bridge::apply_patch`'s own documented non-atomic-write
//! divergence for the settings files), so it gets a small bespoke
//! read/merge/write pair below.
//!
//! ## Internal-write marking
//!
//! `settings_bridge::apply_patch` calls `permission::mark_internal_write`
//! before writing because `runtime/src/desktop/settings_watch.rs` watches
//! `<lingxi_home>/settings.json` / `<project>/.lingxi/settings.json` /
//! `settings.local.json` and would otherwise mistake the desktop's own save
//! for an external edit. **Neither `~/.lingxi.json` nor `<project>/.mcp.json`
//! is watched by that (or any other) watcher in this workspace** — confirmed
//! by reading `settings_watch.rs`'s path→source mapping and grepping the repo
//! for any other `notify`/`FileSystem::watch` consumer of either path. No
//! `mark_internal_write` call would do anything here, so none is made; if a
//! watcher is ever added for these files, this comment is the reminder to
//! wire it.

use std::path::{Path, PathBuf};

use client::protocol::commands::WritableScopeDto;
use mcp::{ConfigScope, McpRegistry};
use migrations::global_config::{
    get_project_config, project_path_for_config, read_map as read_global_map, save_map,
    save_project_config,
};
use serde_json::{Map, Value};

/// The two roots every writable MCP scope's file path is resolved from.
pub struct McpPaths {
    /// The current project's root directory — holds `<project_dir>/.mcp.json`
    /// (`Project` scope) and supplies the canonical project key for `Local`
    /// scope (`projects[<key>]` in the global config).
    pub project_dir: PathBuf,
    /// `~/.lingxi.json` (or its env-relocated / legacy equivalent — see
    /// [`migrations::global_config::global_config_path`]). Backs both `User`
    /// and `Local` scope.
    pub global_config_path: PathBuf,
}

/// `<project_dir>/.mcp.json` — the `Project`-scope file.
fn project_mcp_path(project_dir: &Path) -> PathBuf {
    project_dir.join(".mcp.json")
}

/// Read a JSON object file for the `Project` scope. Missing file ⇒ empty
/// object (nothing written yet). Empty-but-existing file ⇒ empty object
/// (matches `migrations::global_config::read_map`'s own empty-is-absent
/// convention). A file that exists but does not parse as a JSON object is
/// refused — the caller must never overwrite a broken file.
fn read_json_object(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) if raw.trim().is_empty() => Ok(Map::new()),
        Ok(raw) => serde_json::from_str::<Value>(&raw)
            .map_err(|e| format!("{} is not valid JSON: {e}; not overwriting", path.display()))?
            .as_object()
            .cloned()
            .ok_or_else(|| format!("{} is not a JSON object; not overwriting", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}

/// Write a JSON object file for the `Project` scope. Plain
/// `std::fs::write` (not tmp+rename) — the same DOCUMENTED non-atomic-write
/// divergence `settings_bridge::apply_patch` carries for the settings files;
/// `.mcp.json` has no cross-process lock either, matching that sibling.
fn write_json_object(path: &Path, map: Map<String, Value>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let text = serde_json::to_string_pretty(&Value::Object(map))
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    std::fs::write(path, text + "\n")
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
}

/// Refuse to touch a `mcpServers` value that already exists but is not a JSON
/// object — coercing it (e.g. replacing a string with an object) would
/// silently destroy whatever was actually there. An ABSENT key is fine (it
/// will be created as an object on first write).
fn ensure_servers_key_is_object_or_absent(map: &Map<String, Value>) -> Result<(), String> {
    match map.get("mcpServers") {
        None | Some(Value::Object(_)) => Ok(()),
        Some(_) => Err("`mcpServers` is not a JSON object; not overwriting".to_string()),
    }
}

/// `.mcp.json` supports a legacy bare `{name: entry}` shape with NO
/// `mcpServers` wrapper (`mcp::json_config::parse_mcp_json_string`'s
/// `parsed.mcpServers || parsed` precedence: the wrapper, when present, wins
/// OUTRIGHT — sibling top-level keys are never merged with it). Adding a
/// `mcpServers` key to such a file would make every existing bare entry
/// invisible to every reader — the bytes stay on disk but no reader ever
/// looks at them again. An empty object (new/blank file) or one that already
/// carries a `mcpServers` key is safe; anything else is refused rather than
/// guessed at.
fn ensure_project_file_is_wrapped_or_empty(map: &Map<String, Value>) -> Result<(), String> {
    if map.is_empty() || map.contains_key("mcpServers") {
        return Ok(());
    }
    Err(
        "this .mcp.json has no `mcpServers` wrapper (legacy bare-map format); wrap its \
         existing servers under a `mcpServers` key before editing it here"
            .to_string(),
    )
}

/// Post-write check for `User`/`Local` scope: confirm `name` actually landed
/// in `map`'s `mcpServers` with exactly `config`.
///
/// This exists because the shared writers ([`save_map`] /
/// [`save_project_config`]) can report a successful write that changed
/// NOTHING relevant, in two ways this module cannot prevent up front without
/// re-introducing a check-then-write race:
/// - a concurrent edit (another process) replaces `mcpServers` with a
///   non-object BETWEEN this call's own read and the mutator's locked
///   re-read; [`upsert_into`]'s `if let Value::Object` then silently no-ops,
///   and the mutator's output is byte-identical to its input, so the shared
///   writer sees "nothing changed" and returns `Ok(false)` — indistinguishable
///   from "these exact bytes were already there".
/// - [`migrations::global_config::save_project_config`] has its own documented
///   behaviour when the global config's `projects` key exists but is not an
///   object: the top-level merge-back silently fails to insert, yet the
///   function still returns `Ok(true)`. This module cannot change that shared
///   writer, so it detects the condition here instead.
///
/// Reading the ACTUAL post-write state back and checking it is the only way
/// to tell the caller the truth in either case.
fn verify_server_present(
    map: &Map<String, Value>,
    name: &str,
    config: &Value,
) -> Result<(), String> {
    match map
        .get("mcpServers")
        .and_then(Value::as_object)
        .and_then(|s| s.get(name))
    {
        Some(actual) if actual == config => Ok(()),
        _ => Err(format!(
            "the write appeared to succeed but `mcpServers.{name}` was not found afterward \
             with the expected config (a concurrent edit, or a malformed `mcpServers`/\
             `projects` value, likely made the write silently no-op); refusing to report \
             success"
        )),
    }
}

/// The removal counterpart of [`verify_server_present`]: confirm `name` is
/// actually absent from `map`'s `mcpServers` afterward. A missing
/// `mcpServers` key, or an object without `name`, is success — that is the
/// wanted post-condition. `mcpServers` still containing `name` means the
/// removal silently no-op'd (the race case above). `mcpServers` present but
/// NOT an object means the post-write state cannot be confirmed either way,
/// so this refuses to claim success over a shape it cannot read.
fn verify_server_absent(map: &Map<String, Value>, name: &str) -> Result<(), String> {
    match map.get("mcpServers") {
        None => Ok(()),
        Some(Value::Object(obj)) if !obj.contains_key(name) => Ok(()),
        Some(Value::Object(_)) => Err(format!(
            "the removal appeared to succeed but `mcpServers.{name}` is still present \
             afterward (a concurrent edit likely made the removal silently no-op); refusing \
             to report success"
        )),
        Some(_) => Err(
            "`mcpServers` is not a JSON object after the removal attempt; refusing to \
             report success"
                .to_string(),
        ),
    }
}

/// Insert/replace `name` in `map`'s `mcpServers` object. Callers must have
/// already confirmed (via [`ensure_servers_key_is_object_or_absent`]) that an
/// existing `mcpServers` value is an object, so the `entry`/`if let` below
/// can never silently no-op.
fn upsert_into(map: &mut Map<String, Value>, name: &str, config: Value) {
    let servers = map
        .entry("mcpServers".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(obj) = servers {
        obj.insert(name.to_string(), config);
    }
}

/// Remove `name` from `map`'s `mcpServers` object, if present. A missing
/// `mcpServers` key or a missing `name` within it is a no-op, not an error —
/// removal is idempotent.
fn remove_from(map: &mut Map<String, Value>, name: &str) {
    if let Some(Value::Object(obj)) = map.get_mut("mcpServers") {
        obj.remove(name);
    }
}

/// Add or replace one MCP server definition in `scope`. `config` must already
/// be validated as a JSON object by the caller (the router decodes and
/// validates the wire `config_json` string before calling in — see
/// `router::parse_mcp_config_json`); this function does not re-validate its
/// shape beyond what `mcp/src/json_config.rs` itself tolerates (an entry
/// missing both `command` and `url` parses fine here and is simply skipped by
/// the reader, matching claude-code's own per-entry `safeParse` skip).
///
/// # Errors
/// The destination file exists but fails to parse as a JSON object (never
/// overwritten), an existing `mcpServers` value is not an object (never
/// coerced), a `Project`-scope file is a legacy bare-map `.mcp.json` (never
/// silently double-shaped), the underlying write fails, or — `User`/`Local`
/// only — a post-write read-back shows `name` did not actually land (see
/// [`verify_server_present`]).
pub fn upsert_server(
    paths: &McpPaths,
    scope: WritableScopeDto,
    name: &str,
    config: Value,
) -> Result<(), String> {
    match scope {
        WritableScopeDto::Project => {
            let path = project_mcp_path(&paths.project_dir);
            let root = read_json_object(&path)?;
            ensure_project_file_is_wrapped_or_empty(&root)?;
            ensure_servers_key_is_object_or_absent(&root)?;
            let mut next = root.clone();
            upsert_into(&mut next, name, config);
            if next == root {
                // Nothing actually changed (e.g. re-upserting byte-identical
                // config) — skip the write, mirroring `save_map` /
                // `save_project_config`'s own unchanged-skip, rather than
                // rewriting the file for no reason.
                return Ok(());
            }
            write_json_object(&path, next)
        }
        WritableScopeDto::User => {
            // The domain-specific `mcpServers`-is-an-object check does NOT run
            // as a pre-check here (a pre-check race that `save_map`'s own
            // locked re-read can invalidate is exactly the Minor-2 bug this
            // shape closes) — it is enforced by verifying the ACTUAL
            // post-write state below, which catches both a pre-existing
            // malformed value and one introduced concurrently between any
            // two reads.
            let expected = config.clone();
            save_map(&paths.global_config_path, move |mut map| {
                upsert_into(&mut map, name, config);
                map
            })
            .map_err(|e| e.to_string())?;
            let after = read_global_map(&paths.global_config_path).map_err(|e| e.to_string())?;
            verify_server_present(&after, name, &expected)
        }
        WritableScopeDto::Local => {
            let key = project_path_for_config(&paths.project_dir);
            let expected = config.clone();
            save_project_config(&paths.global_config_path, &key, move |mut proj| {
                upsert_into(&mut proj, name, config);
                proj
            })
            .map_err(|e| e.to_string())?;
            let after_proj =
                get_project_config(&paths.global_config_path, &key).map_err(|e| e.to_string())?;
            verify_server_present(&after_proj, name, &expected)
        }
    }
}

/// Remove one MCP server definition from `scope`. Idempotent: removing an
/// already-absent `name` is not an error and performs no write.
///
/// # Errors
/// Same file-safety conditions as [`upsert_server`] (never overwrites a
/// broken file, never coerces a non-object `mcpServers`, never touches a
/// legacy bare-map `Project` file), plus the underlying write failing, or —
/// `User`/`Local` only — a post-write read-back shows `name` is still
/// present (see [`verify_server_absent`]).
pub fn remove_server(paths: &McpPaths, scope: WritableScopeDto, name: &str) -> Result<(), String> {
    match scope {
        WritableScopeDto::Project => {
            let path = project_mcp_path(&paths.project_dir);
            let root = read_json_object(&path)?;
            ensure_project_file_is_wrapped_or_empty(&root)?;
            ensure_servers_key_is_object_or_absent(&root)?;
            let mut next = root.clone();
            remove_from(&mut next, name);
            if next == root {
                // Nothing to remove (already absent, or no `mcpServers` key
                // at all) — skip the write. Without this, removing a name
                // from a project that has no `.mcp.json` yet would create one
                // containing `{}` as the side effect of a pure no-op.
                return Ok(());
            }
            write_json_object(&path, next)
        }
        WritableScopeDto::User => {
            save_map(&paths.global_config_path, move |mut map| {
                remove_from(&mut map, name);
                map
            })
            .map_err(|e| e.to_string())?;
            let after = read_global_map(&paths.global_config_path).map_err(|e| e.to_string())?;
            verify_server_absent(&after, name)
        }
        WritableScopeDto::Local => {
            let key = project_path_for_config(&paths.project_dir);
            save_project_config(&paths.global_config_path, &key, move |mut proj| {
                remove_from(&mut proj, name);
                proj
            })
            .map_err(|e| e.to_string())?;
            let after_proj =
                get_project_config(&paths.global_config_path, &key).map_err(|e| e.to_string())?;
            verify_server_absent(&after_proj, name)
        }
    }
}

/// Reconcile the live registry against the three writable on-disk MCP scopes.
///
/// Plugin/managed/agent registrations are preserved by scope and are never
/// removed here.
pub async fn reconcile_writable_servers(
    registry: &McpRegistry,
    paths: &McpPaths,
) -> Result<(), String> {
    let desired = mcp::load_mcp_servers(
        &project_mcp_path(&paths.project_dir),
        &paths.global_config_path,
        &paths.project_dir,
    );
    let desired_names: std::collections::BTreeSet<String> =
        desired.iter().map(|config| config.name.clone()).collect();

    let mut failures = Vec::new();
    for config in desired {
        if let Some(current) = registry.get_config(&config.name).await {
            if !matches!(
                current.scope,
                ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
                    | ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)
                    | ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
            ) {
                // Plugin/managed/agent/dynamic servers own their registry slot.
                // A writable definition with the same name stays on disk but
                // cannot displace the runtime-injected owner.
                continue;
            }
        }
        if let Err(error) = registry.replace_config_atomically(config.clone()).await {
            failures.push(format!("{}: {error}", config.name));
        }
    }
    if !failures.is_empty() {
        return Err(format!(
            "failed to build replacement MCP server state: {}",
            failures.join("; ")
        ));
    }

    for name in registry.server_names().await {
        let Some(current) = registry.get_config(&name).await else {
            continue;
        };
        if matches!(
            current.scope,
            ConfigScope::Settings(lingxi_core::types::SettingsScope::User)
                | ConfigScope::Settings(lingxi_core::types::SettingsScope::Local)
                | ConfigScope::Settings(lingxi_core::types::SettingsScope::Project)
        ) && !desired_names.contains(&name)
        {
            registry
                .remove_without_revoking_auth_if_config(&name, &current)
                .await
                .map_err(|error| format!("failed to retire MCP server `{name}`: {error}"))?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn paths(dir: &Path) -> McpPaths {
        McpPaths {
            project_dir: dir.to_path_buf(),
            global_config_path: dir.join(".lingxi.json"),
        }
    }

    /// Brief's canonical Project-scope round-trip: write lands in
    /// `<project>/.mcp.json` under `mcpServers`, and removal deletes the
    /// entry. Also confirms the REAL `mcp::json_config` parser (not this
    /// module's own reader) accepts what was written and resolves the same
    /// `command`/`args` — a round-trip through only this module's own reader
    /// would prove the two halves agree with each other, not that either
    /// matches what the reader oracle expects.
    #[test]
    fn project_scope_servers_round_trip_through_dot_mcp_json() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        upsert_server(
            &p,
            WritableScopeDto::Project,
            "linear",
            json!({ "command": "npx", "args": ["-y", "linear-mcp"] }),
        )
        .unwrap();

        let raw = std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            parsed["mcpServers"]["linear"]["command"], "npx",
            "a project-scope server must land in <project>/.mcp.json under mcpServers"
        );

        let cfgs = mcp::parse_mcp_json_string(
            &raw,
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Project),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "linear");
        match &cfgs[0].spec {
            lingxi_core::host::McpTransportSpec::Stdio { command, args, .. } => {
                assert_eq!(command, "npx");
                assert_eq!(args, &vec!["-y".to_string(), "linear-mcp".to_string()]);
            }
            other => panic!("expected Stdio, got {other:?}"),
        }

        remove_server(&p, WritableScopeDto::Project, "linear").unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap())
                .unwrap();
        assert!(
            after["mcpServers"].get("linear").is_none(),
            "removal must delete the entry, got: {after}"
        );
    }

    /// `User` scope lands in `~/.lingxi.json`'s TOP-LEVEL `mcpServers`, and a
    /// pre-existing unrelated key (`numStartups`) — the exact shape a real
    /// global config carries — survives verbatim. Also confirmed against the
    /// real global-config-only parser (`parse_global_config_mcp_servers`,
    /// which has NO bare-map fallback).
    #[test]
    fn user_scope_servers_round_trip_through_global_config_and_preserves_siblings() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(
            &p.global_config_path,
            r#"{"numStartups":7,"oauthAccount":{"emailAddress":"x@y.z"}}"#,
        )
        .unwrap();

        upsert_server(
            &p,
            WritableScopeDto::User,
            "mem",
            json!({ "command": "mcp-memory" }),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&p.global_config_path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["mcpServers"]["mem"]["command"], "mcp-memory");
        assert_eq!(
            parsed["numStartups"], 7,
            "an unrelated top-level key must survive the write verbatim"
        );
        assert_eq!(parsed["oauthAccount"]["emailAddress"], "x@y.z");

        let cfgs = mcp::parse_global_config_mcp_servers(
            &raw,
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::User),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "mem");

        remove_server(&p, WritableScopeDto::User, "mem").unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&p.global_config_path).unwrap()).unwrap();
        assert!(after["mcpServers"].get("mem").is_none());
        assert_eq!(
            after["numStartups"], 7,
            "removal must also leave unrelated keys untouched"
        );
    }

    /// `Local` scope lands under `projects[<canonical-key>].mcpServers` in
    /// `~/.lingxi.json` — NOT the top level (a scope-mapping bug — e.g. Local
    /// writing to the User location — is invisible to a test that only
    /// exercises one scope, so this scope is asserted on its own, at its own
    /// path within the file).
    #[test]
    fn local_scope_servers_round_trip_under_projects_key() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());

        upsert_server(
            &p,
            WritableScopeDto::Local,
            "loc",
            json!({ "command": "loc-cmd" }),
        )
        .unwrap();

        let raw = std::fs::read_to_string(&p.global_config_path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let key = project_path_for_config(dir.path());
        assert_eq!(
            parsed["projects"][&key]["mcpServers"]["loc"]["command"], "loc-cmd",
            "a Local-scope server must land under projects[<key>].mcpServers, got: {parsed}"
        );
        assert!(
            parsed.get("mcpServers").is_none(),
            "Local scope must NOT write the top-level mcpServers (that's User's location)"
        );

        let cfgs = mcp::parse_local_config_mcp_servers(
            &raw,
            &key,
            mcp::ConfigScope::Settings(lingxi_core::types::SettingsScope::Local),
        )
        .unwrap();
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].name, "loc");

        remove_server(&p, WritableScopeDto::Local, "loc").unwrap();
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&p.global_config_path).unwrap()).unwrap();
        assert!(after["projects"][&key]["mcpServers"].get("loc").is_none());
    }

    /// The scope-mapping regression this suite exists to catch: writing the
    /// SAME server name to all three scopes must produce three DISTINCT
    /// on-disk locations, never collapsing two of them together. A swapped
    /// `Project`/`User` (or any other pairwise swap) branch would make one of
    /// these three assertions fail.
    #[test]
    fn all_three_scopes_land_at_distinct_paths_for_the_same_name() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        for (scope, marker) in [
            (WritableScopeDto::User, "user-cmd"),
            (WritableScopeDto::Local, "local-cmd"),
            (WritableScopeDto::Project, "project-cmd"),
        ] {
            upsert_server(&p, scope, "shared-name", json!({ "command": marker })).unwrap();
        }

        let project_raw = std::fs::read_to_string(dir.path().join(".mcp.json")).unwrap();
        let project: Value = serde_json::from_str(&project_raw).unwrap();
        assert_eq!(
            project["mcpServers"]["shared-name"]["command"],
            "project-cmd"
        );

        let global_raw = std::fs::read_to_string(&p.global_config_path).unwrap();
        let global: Value = serde_json::from_str(&global_raw).unwrap();
        assert_eq!(
            global["mcpServers"]["shared-name"]["command"], "user-cmd",
            "User scope must be the global config's TOP LEVEL"
        );
        let key = project_path_for_config(dir.path());
        assert_eq!(
            global["projects"][&key]["mcpServers"]["shared-name"]["command"], "local-cmd",
            "Local scope must be the global config's projects[<key>] sub-object"
        );
    }

    /// Global constraint: a non-empty, invalid-JSON target file must never be
    /// overwritten by any of the three scopes.
    #[test]
    fn a_broken_target_file_is_never_overwritten_in_any_scope() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());

        std::fs::write(project_mcp_path(dir.path()), "{ not json").unwrap();
        std::fs::write(&p.global_config_path, "{ not json").unwrap();

        for scope in [
            WritableScopeDto::Project,
            WritableScopeDto::User,
            WritableScopeDto::Local,
        ] {
            let err = upsert_server(&p, scope, "x", json!({ "command": "x" })).unwrap_err();
            assert!(
                err.contains("not valid JSON") || err.to_lowercase().contains("invalid"),
                "expected a broken-JSON refusal for {scope:?}, got: {err}"
            );
        }

        assert_eq!(
            std::fs::read_to_string(project_mcp_path(dir.path())).unwrap(),
            "{ not json",
            "the broken project file must survive byte-for-byte"
        );
        assert_eq!(
            std::fs::read_to_string(&p.global_config_path).unwrap(),
            "{ not json",
            "the broken global config file must survive byte-for-byte"
        );
    }

    /// Removing a name that was never present is a successful no-op, not an
    /// error — matching the brief's idempotent-removal contract. Critically,
    /// "no-op" must mean NO FILE IS CREATED: a naive implementation that
    /// unconditionally writes back the (unchanged) in-memory map after a
    /// no-op removal would materialize `<project>/.mcp.json` — or
    /// `~/.lingxi.json` — purely as a side effect of doing nothing, which is
    /// exactly the Minor-1 regression this test now pins.
    #[test]
    fn removing_an_absent_server_is_not_an_error_and_creates_no_file() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        remove_server(&p, WritableScopeDto::Project, "never-existed").unwrap();
        remove_server(&p, WritableScopeDto::User, "never-existed").unwrap();
        remove_server(&p, WritableScopeDto::Local, "never-existed").unwrap();

        assert!(
            !project_mcp_path(dir.path()).exists(),
            "a no-op Project removal must not create .mcp.json"
        );
        assert!(
            !p.global_config_path.exists(),
            "a no-op User/Local removal must not create ~/.lingxi.json"
        );
    }

    /// The removal counterpart: removing a name that IS present, from a file
    /// that already existed with unrelated content, must still write (the
    /// no-write skip above must not swallow a REAL removal too).
    #[test]
    fn removing_a_present_server_still_writes() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        upsert_server(
            &p,
            WritableScopeDto::Project,
            "linear",
            json!({ "command": "npx" }),
        )
        .unwrap();
        remove_server(&p, WritableScopeDto::Project, "linear").unwrap();
        let raw = std::fs::read_to_string(project_mcp_path(dir.path())).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert!(parsed["mcpServers"].get("linear").is_none());
    }

    /// Minor-2 (race proxy): if `mcpServers` is a non-object at write time —
    /// whether it was already malformed, or a concurrent writer replaced it
    /// between two reads — the shared `save_map` writer's mutator silently
    /// no-ops and reports `Ok(false)` (indistinguishable from "already had
    /// these bytes"). The post-write verification must turn that into an
    /// Err rather than letting the caller believe the server was added, AND
    /// the malformed value must survive untouched (never coerced).
    #[test]
    fn user_scope_non_object_mcp_servers_value_is_never_reported_as_success() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(&p.global_config_path, r#"{"mcpServers":"oops"}"#).unwrap();

        let err = upsert_server(&p, WritableScopeDto::User, "new", json!({ "command": "n" }))
            .unwrap_err();
        assert!(
            err.contains("mcpServers") && err.contains("new"),
            "expected a refusal naming the key and server, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&p.global_config_path).unwrap(),
            r#"{"mcpServers":"oops"}"#,
            "the malformed value must never be coerced or overwritten"
        );
    }

    /// Minor-2b: `migrations::global_config::save_project_config` silently
    /// fails to insert (and still returns `Ok(true)`) when the global
    /// config's `projects` key exists but is not an object — this module
    /// cannot change that shared writer, so it must detect the condition
    /// itself via post-write verification and report failure rather than
    /// passing the shared writer's `Ok(true)` straight through.
    #[test]
    fn local_scope_reports_failure_when_projects_key_is_not_an_object() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(&p.global_config_path, r#"{"projects":"oops"}"#).unwrap();

        let err = upsert_server(
            &p,
            WritableScopeDto::Local,
            "new",
            json!({ "command": "n" }),
        )
        .unwrap_err();
        assert!(
            err.contains("new") || err.contains("mcpServers"),
            "expected a refusal naming what could not be confirmed, got: {err}"
        );
    }

    /// Same Minor-2 race-proxy as the `User`-scope test above, but for
    /// `Local` scope's `projects[<key>].mcpServers`.
    #[test]
    fn local_scope_non_object_mcp_servers_value_is_never_reported_as_success() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        let key = project_path_for_config(dir.path());
        let mut projects = Map::new();
        projects.insert(key, json!({ "mcpServers": "oops" }));
        let mut global = Map::new();
        global.insert("projects".to_string(), Value::Object(projects));
        std::fs::write(
            &p.global_config_path,
            serde_json::to_string(&Value::Object(global)).unwrap(),
        )
        .unwrap();

        let err = upsert_server(
            &p,
            WritableScopeDto::Local,
            "new",
            json!({ "command": "n" }),
        )
        .unwrap_err();
        assert!(
            err.contains("mcpServers") && err.contains("new"),
            "expected a refusal naming the key and server, got: {err}"
        );
    }

    /// A legacy bare-map `.mcp.json` (no `mcpServers` wrapper) must be
    /// refused rather than silently shadowed: `json_config`'s
    /// `parsed.mcpServers || parsed` precedence means adding a `mcpServers`
    /// key would make the existing bare entry invisible to every reader.
    #[test]
    fn bare_map_project_file_is_refused_not_silently_shadowed() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(
            project_mcp_path(dir.path()),
            r#"{"legacy":{"command":"legacy-cmd"}}"#,
        )
        .unwrap();

        let err = upsert_server(
            &p,
            WritableScopeDto::Project,
            "new",
            json!({ "command": "n" }),
        )
        .unwrap_err();
        assert!(
            err.contains("bare-map"),
            "expected a bare-map refusal, got: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(project_mcp_path(dir.path())).unwrap(),
            r#"{"legacy":{"command":"legacy-cmd"}}"#,
            "the bare-map file must survive untouched"
        );
    }

    /// A `mcpServers` value that exists but is not a JSON object (e.g. a
    /// string) must never be silently coerced into an object.
    #[test]
    fn non_object_mcp_servers_value_is_refused() {
        let dir = tempdir().unwrap();
        let p = paths(dir.path());
        std::fs::write(project_mcp_path(dir.path()), r#"{"mcpServers":"oops"}"#).unwrap();

        let err = upsert_server(
            &p,
            WritableScopeDto::Project,
            "new",
            json!({ "command": "n" }),
        )
        .unwrap_err();
        assert!(
            err.contains("not a JSON object"),
            "expected a non-object refusal, got: {err}"
        );
    }
}
