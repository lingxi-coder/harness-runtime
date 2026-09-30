//! Persisting a [`PermissionUpdate`] to a settings file (enforcement phase 3c).
//!
//! Ports claude-code `persistPermissionUpdate` (`PermissionUpdate.ts`) →
//! `addPermissionRulesToSettings` (`permissionsLoader.ts`) → `updateSettingsForSource`
//! for rule, mode, and workspace-directory updates. The flow:
//!
//! 1. Only DESTINATIONS that support persistence are written — `localSettings`
//!    / `userSettings` / `projectSettings` (claude-code `supportsPersistence`).
//!    `Session` / `CliArg` are no-ops (return `Ok(false)`).
//! 2. Read the destination settings file (missing / empty → empty object;
//!    syntactically broken JSON → bail WITHOUT overwriting, matching the TS
//!    `updateSettingsForSource` error contract).
//! 3. Append the rule's string form to `permissions.{allow|deny|ask}` (keyed by
//!    the rule's behavior), de-duplicating against existing entries normalized
//!    via a parse→serialize round-trip (so a legacy alias already on disk
//!    matches its canonical form). Existing entries are preserved verbatim and
//!    ALL other settings keys are preserved (the merge is on a `serde_json::Value`
//!    so unrecognized keys survive — mirroring TS `{ ...settingsData }`).
//! 4. Hold a destination-scoped exclusive lock, mark the target as an internal
//!    write for the settings watcher, and atomically replace it through the
//!    root-confined no-follow filesystem primitive.

use crate::result::PermissionUpdateDestination;
use crate::rule::{PermissionBehavior, PermissionRule, PermissionRuleValue};
use crate::update::PermissionUpdate;
use lingxi_core::host::rooted_fs::{self, AtomicWriteOptions, PRIVATE_DIR_MODE, PRIVATE_FILE_MODE};
use lingxi_core::host::FsError;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Filesystem roots used to resolve a [`PermissionUpdateDestination`] to a
/// concrete settings file (the persistence analogue of [`crate::FsRoots`]).
#[derive(Debug, Clone)]
pub struct PermissionPaths {
    /// Claude config home (`~/.claude`) — holds `userSettings`.
    pub lingxi_home: PathBuf,
    /// Project working directory — holds `.lingxi/settings.json` (project) and
    /// `.lingxi/settings.local.json` (local).
    pub cwd: PathBuf,
}

impl PermissionPaths {
    /// Resolve a destination to its settings file, or `None` when the
    /// destination is not persistable (`Session` / `CliArg`) — claude-code
    /// `supportsPersistence` + `getSettingsFilePathForSource`.
    #[must_use]
    pub fn destination_path(&self, dest: PermissionUpdateDestination) -> Option<PathBuf> {
        match dest {
            PermissionUpdateDestination::UserSettings => {
                Some(self.lingxi_home.join("settings.json"))
            }
            PermissionUpdateDestination::ProjectSettings => {
                Some(self.cwd.join(branding::DOT_DIR).join("settings.json"))
            }
            PermissionUpdateDestination::LocalSettings => {
                Some(self.cwd.join(branding::DOT_DIR).join("settings.local.json"))
            }
            PermissionUpdateDestination::Session | PermissionUpdateDestination::CliArg => None,
        }
    }
}

/// Error persisting a permission update.
#[derive(Debug, thiserror::Error)]
pub enum PersistError {
    /// The destination file cannot be merged into safely — either not valid
    /// JSON, or valid JSON whose `permissions`/`permissions.{behavior}` is the
    /// wrong type (a string/object where an object/array is expected). In every
    /// case the file is left UNTOUCHED (claude-code's `updateSettingsForSource`
    /// "Invalid JSON syntax" + the `.map()`-throws-on-non-array contract).
    #[error(
        "settings file at {0} cannot be merged (invalid or wrong-typed JSON); not overwriting"
    )]
    BrokenJson(PathBuf),
    /// A root-confined read, lock, or atomic replacement failed. This includes
    /// symlink/reparse-point traversal attempts, which are deliberately rejected.
    /// Merging the proposed `autoMode` block with the one already in the file
    /// would produce an invalid result, so nothing was written
    /// (`invalid_merged`). Carries the validator's reason.
    #[error("auto-mode merge produced an invalid result: {0}")]
    InvalidMerged(String),
    /// A root-confined read, lock, or atomic replacement failed. This includes
    /// symlink/reparse-point traversal attempts, which are deliberately rejected.
    #[error("confined settings operation failed on {path}: {source}")]
    Confined {
        /// The logical destination settings file.
        path: PathBuf,
        /// The hardened filesystem error.
        source: FsError,
    },
}

#[derive(Debug, Clone)]
struct ConfinedSettingsPath {
    root: PathBuf,
    relative: PathBuf,
    display: PathBuf,
}

fn confined_settings_path(
    paths: &PermissionPaths,
    dest: PermissionUpdateDestination,
) -> Result<Option<ConfinedSettingsPath>, PersistError> {
    let Some(display) = paths.destination_path(dest) else {
        return Ok(None);
    };
    // The confinement ROOT is the settings DIRECTORY itself (`~/.lingxi` or
    // `<cwd>/.lingxi`), and `relative` is only the FINAL file name. This mirrors
    // the 2.1.218 oracle's hardened atomic write, which opens the STAGING DIR
    // with `O_RDONLY|O_DIRECTORY|O_NOFOLLOW` (after intermediate components — a
    // symlinked `.lingxi`, a common dotfiles pattern — resolve normally) and
    // keeps `O_NOFOLLOW` only on the final file + staging temp. `rooted_fs`
    // canonicalizes the root before its no-follow open, so a symlinked settings
    // DIR is ACCEPTED while a symlinked settings FILE is still rejected.
    let (root, relative) = match dest {
        PermissionUpdateDestination::UserSettings => {
            (paths.lingxi_home.clone(), PathBuf::from("settings.json"))
        }
        PermissionUpdateDestination::ProjectSettings => (
            paths.cwd.join(branding::DOT_DIR),
            PathBuf::from("settings.json"),
        ),
        PermissionUpdateDestination::LocalSettings => (
            paths.cwd.join(branding::DOT_DIR),
            PathBuf::from("settings.local.json"),
        ),
        PermissionUpdateDestination::Session | PermissionUpdateDestination::CliArg => {
            return Ok(None);
        }
    };
    Ok(Some(ConfinedSettingsPath {
        root,
        relative,
        display,
    }))
}

fn confined_error(path: &ConfinedSettingsPath, source: FsError) -> PersistError {
    PersistError::Confined {
        path: path.display.clone(),
        source,
    }
}

fn lock_relative_path(relative: &Path) -> Result<PathBuf, FsError> {
    let Some(file_name) = relative.file_name().and_then(|name| name.to_str()) else {
        return Err(FsError::OutsideWorkspace(relative.display().to_string()));
    };
    let mut lock = relative.to_path_buf();
    lock.set_file_name(format!(".{file_name}.lock"));
    Ok(lock)
}

/// Best-effort mirror of claude-code `a9n` (`gitignore.ts`): ensure the local
/// settings file is git-ignored by appending a `**/<relpath>` rule to the user's
/// GLOBAL git excludes file — NOT the project `.gitignore`.
///
/// The 2.1.218 oracle NEVER modifies a tracked file for this: it (a) bails unless
/// the cwd is inside a git work tree, (b) skips if `git check-ignore` already
/// covers the path, then (c) resolves the global excludes file (`git config
/// --global core.excludesfile` → `$XDG_CONFIG_HOME/git/ignore` →
/// `~/.config/git/ignore`) and appends the rule there. The whole thing is
/// fire-and-forget — every failure is swallowed. Consequently NO `.gitignore` or
/// `.gitignore.lock` artifacts are left in the user's repository (the previous
/// port wrote `/.lingxi/settings.local.json` into `<cwd>/.gitignore` under a
/// confinement lock, which the oracle does not do).
fn ensure_local_settings_ignored(paths: &PermissionPaths) {
    // (a9n) Do nothing outside a git work tree.
    if !is_inside_git_work_tree(&paths.cwd) {
        return;
    }
    let relpath = format!("{}/settings.local.json", branding::DOT_DIR);
    // Already ignored (by the project `.gitignore`, `.git/info/exclude`, or the
    // global excludes) → nothing to do.
    if git_check_ignore(&relpath, &paths.cwd) {
        return;
    }
    // Oracle rule is a repo-anchored glob: `**/<relpath>`.
    let rule = format!("**/{relpath}");
    let excludes = resolve_global_git_excludes(&paths.cwd);
    if let Some(dir) = excludes.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match std::fs::read_to_string(&excludes) {
        // Present already (but check-ignore said not-ignored) → the oracle warns
        // and leaves it; do NOT duplicate the line.
        Ok(existing) if existing.contains(&rule) => {}
        // Append `\n<rule>\n` (oracle `appendFile(i, `\n${n}\n`)`).
        Ok(_) => {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&excludes) {
                let _ = write!(f, "\n{rule}\n");
            }
        }
        // Missing → create with `<rule>\n` (oracle ENOENT `writeFile(i, `${n}\n`)`).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let _ = std::fs::write(&excludes, format!("{rule}\n"));
        }
        Err(_) => {}
    }
}

/// `git rev-parse --is-inside-work-tree` in `cwd` (claude-code `hDi`): true only
/// when the directory is inside a git work tree. Best-effort — any spawn failure
/// (git absent, not a repo) is treated as "not a repo".
fn is_inside_git_work_tree(cwd: &Path) -> bool {
    std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success() && o.stdout.starts_with(b"true"))
        .unwrap_or(false)
}

/// `git check-ignore -- <pathspec>` in `cwd` (claude-code `s9n`): exit 0 ⇒ the
/// path is already ignored.
fn git_check_ignore(pathspec: &str, cwd: &Path) -> bool {
    std::process::Command::new("git")
        .args(["check-ignore", "--", pathspec])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve the user's global git excludes file (claude-code `uEh`): prefer
/// `git config --global core.excludesfile`, then `$XDG_CONFIG_HOME/git/ignore`,
/// then `~/.config/git/ignore`.
fn resolve_global_git_excludes(cwd: &Path) -> PathBuf {
    let configured = std::process::Command::new("git")
        .args(["config", "--global", "--get", "core.excludesfile"])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    excludes_path_from(
        configured.as_deref(),
        &std::env::var("HOME").unwrap_or_default(),
        std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
    )
}

/// Pure resolution of `uEh` given the raw inputs (extracted so the `~`/XDG/HOME
/// fallback chain is hermetically testable without spawning git or mutating the
/// process environment).
fn excludes_path_from(configured: Option<&str>, home: &str, xdg: Option<&str>) -> PathBuf {
    if let Some(n) = configured {
        let n = n.trim();
        if !n.is_empty() {
            // `~` ⇒ home; `~/x` ⇒ home/x (oracle `join(homedir(), n.slice(2))`).
            if n == "~" || n.starts_with("~/") {
                return PathBuf::from(home).join(n.get(2..).unwrap_or(""));
            }
            let p = PathBuf::from(n);
            if p.is_absolute() {
                return p;
            }
        }
    }
    if let Some(xdg) = xdg {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return p.join("git").join("ignore");
        }
    }
    PathBuf::from(home)
        .join(".config")
        .join("git")
        .join("ignore")
}

fn mutate_settings_file<F>(
    paths: &PermissionPaths,
    dest: PermissionUpdateDestination,
    missing_is_empty: bool,
    mutate: F,
) -> Result<bool, PersistError>
where
    F: FnOnce(&str) -> Result<Option<String>, ()>,
{
    let Some(mut path) = confined_settings_path(paths, dest)? else {
        return Ok(false);
    };
    // Ensure the settings DIR exists (oracle mkdir -p of the staging dir). This
    // resolves symlinks normally, so a symlinked `.lingxi` is followed to its
    // real location and ACCEPTED — the root-confined ops below still keep the
    // final file + staging no-follow. Any failure here is surfaced by the
    // subsequent `lock_exclusive`/`open_root` as a `Confined` error.
    let _ = std::fs::create_dir_all(&path.root);
    // This boundary intentionally permits a symlinked settings directory
    // (for dotfile layouts), while rooted_fs still rejects symlinks below the
    // resolved directory and on the final settings file.
    if let Ok(canonical_root) = std::fs::canonicalize(&path.root) {
        path.root = canonical_root;
    }
    let lock_relative =
        lock_relative_path(&path.relative).map_err(|source| confined_error(&path, source))?;
    let _lock = rooted_fs::lock_exclusive(
        &path.root,
        &lock_relative,
        PRIVATE_DIR_MODE,
        PRIVATE_FILE_MODE,
    )
    .map_err(|source| confined_error(&path, source))?;

    let raw = match rooted_fs::read_to_string(&path.root, &path.relative) {
        Ok(raw) => raw,
        Err(FsError::NotFound(_)) if missing_is_empty => String::new(),
        Err(FsError::NotFound(_)) => return Ok(false),
        Err(source) => return Err(confined_error(&path, source)),
    };
    let Some(updated) =
        mutate(&raw).map_err(|()| PersistError::BrokenJson(path.display.clone()))?
    else {
        return Ok(false);
    };
    if dest == PermissionUpdateDestination::LocalSettings {
        // Fire-and-forget, like the oracle — this writes the user's GLOBAL git
        // excludes (never the project `.gitignore`) and can never abort the
        // settings write below.
        ensure_local_settings_ignored(paths);
    }
    // Mark AFTER a successful write. Marking before (the prior order) left a
    // stale suppression mark on a FAILED write — the settings watcher would then
    // consume it on the next event and silently swallow one genuine external
    // settings change as if it were our own write.
    rooted_fs::atomic_write(
        &path.root,
        &path.relative,
        updated.as_bytes(),
        AtomicWriteOptions::default(),
    )
    .map_err(|source| confined_error(&path, source))?;
    crate::mark_internal_write(&path.display);
    Ok(true)
}

/// `permissions.{allow|deny|ask}` key for a behavior.
fn behavior_key(behavior: PermissionBehavior) -> &'static str {
    match behavior {
        PermissionBehavior::Allow => "allow",
        PermissionBehavior::Deny => "deny",
        PermissionBehavior::Ask => "ask",
    }
}

/// Merge `rule`'s string form into `permissions.{behavior}` of `raw` settings
/// JSON (the pure core of `addPermissionRulesToSettings`). Returns:
/// - `Ok(Some(new_json))` — the rule was new; `new_json` is the pretty-printed
///   updated settings (with a trailing newline) preserving all other keys.
/// - `Ok(None)` — the rule (normalized) is already present; no write needed.
/// - `Err(())` — `raw` is non-empty and not a JSON object (caller maps to
///   [`PersistError::BrokenJson`] and must NOT overwrite).
///
/// Existing entries are preserved verbatim; only de-duplication normalizes via
/// [`PermissionRuleValue::from_rule_string`]→[`PermissionRuleValue::to_rule_string`].
fn apply_rule_to_settings_json(raw: &str, rule: &PermissionRule) -> Result<Option<String>, ()> {
    mutate_rules_in_settings_json(raw, std::slice::from_ref(rule), true)
}

fn mutate_rules_in_settings_json(
    raw: &str,
    rules: &[PermissionRule],
    add: bool,
) -> Result<Option<String>, ()> {
    let Some(first) = rules.first() else {
        return Ok(None);
    };
    if rules.iter().any(|rule| rule.behavior != first.behavior) {
        return Err(());
    }
    if !add && raw.trim().is_empty() {
        return Ok(None);
    }
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|_| ())?
    };
    let obj = root.as_object_mut().ok_or(())?;

    let key = behavior_key(first.behavior);
    let targets: Vec<String> = rules
        .iter()
        .map(|rule| rule.value.to_rule_string())
        .collect();
    let mut changed = false;
    if add {
        let perms = obj.entry("permissions").or_insert_with(|| json!({}));
        let perms_obj = perms.as_object_mut().ok_or(())?;
        let arr = perms_obj.entry(key).or_insert_with(|| json!([]));
        let arr_vec = arr.as_array_mut().ok_or(())?;
        for target in targets {
            let exists = arr_vec.iter().filter_map(Value::as_str).any(|existing| {
                PermissionRuleValue::from_rule_string(existing).to_rule_string() == target
            });
            if !exists {
                arr_vec.push(json!(target));
                changed = true;
            }
        }
    } else {
        let Some(perms) = obj.get_mut("permissions") else {
            return Ok(None);
        };
        let perms_obj = perms.as_object_mut().ok_or(())?;
        let Some(arr) = perms_obj.get_mut(key) else {
            return Ok(None);
        };
        let arr_vec = arr.as_array_mut().ok_or(())?;
        let before = arr_vec.len();
        arr_vec.retain(|existing| {
            existing.as_str().is_none_or(|value| {
                let normalized = PermissionRuleValue::from_rule_string(value).to_rule_string();
                !targets.iter().any(|target| target == &normalized)
            })
        });
        changed = arr_vec.len() != before;
    }
    if !changed {
        return Ok(None);
    }
    let serialized = serde_json::to_string_pretty(&root).map_err(|_| ())?;
    Ok(Some(serialized + "\n"))
}

/// Persist `update` to its destination settings file (3c). Best-effort and
/// idempotent: returns `Ok(true)` when a rule was written, `Ok(false)` when the
/// destination is not persistable or the rule is already present.
///
/// # Errors
/// [`PersistError::BrokenJson`] if the destination file is not valid JSON (it is
/// left untouched); [`PersistError::Confined`] on a hardened filesystem failure.
pub async fn persist_permission_update(
    update: &PermissionUpdate,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    mutate_settings_file(paths, update.destination, true, |raw| {
        apply_rule_to_settings_json(raw, &update.rule)
    })
}

/// Add or remove a same-behavior rule set in one locked atomic transaction.
pub async fn persist_permission_rule_set(
    rules: &[PermissionRule],
    add: bool,
    destination: PermissionUpdateDestination,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    if rules.is_empty() {
        return Ok(false);
    }
    mutate_settings_file(paths, destination, add, |raw| {
        mutate_rules_in_settings_json(raw, rules, add)
    })
}

/// Remove `rule`'s string form from `permissions.{behavior}` of `raw` settings
/// JSON (the inverse of [`apply_rule_to_settings_json`]). Returns:
/// - `Ok(Some(new_json))` — at least one matching entry was removed; `new_json`
///   is the pretty-printed updated settings (trailing newline).
/// - `Ok(None)` — no matching entry (nothing to write).
/// - `Err(())` — `raw` is non-empty and not a JSON object (caller maps to
///   [`PersistError::BrokenJson`] and must NOT overwrite).
///
/// Matching is canonical (both sides normalized via `PermissionRuleValue`), so a
/// legacy on-disk alias of the same rule is removed too.
fn remove_rule_from_settings_json(raw: &str, rule: &PermissionRule) -> Result<Option<String>, ()> {
    mutate_rules_in_settings_json(raw, std::slice::from_ref(rule), false)
}

fn replace_rules_in_settings_json(
    raw: &str,
    behavior: PermissionBehavior,
    rules: &[PermissionRule],
) -> Result<Option<String>, ()> {
    // Keep the public replacement primitive fail-closed just like the
    // add/remove batch path. Production wire parsing constructs homogeneous
    // rules, but accepting a mismatched caller here would silently serialize a
    // deny/ask rule into the selected allow bucket (or vice versa).
    if rules.iter().any(|rule| rule.behavior != behavior) {
        return Err(());
    }
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|_| ())?
    };
    let obj = root.as_object_mut().ok_or(())?;
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    let perms_obj = perms.as_object_mut().ok_or(())?;
    let key = behavior_key(behavior);
    let replacement: Vec<Value> = rules
        .iter()
        .map(|rule| json!(rule.value.to_rule_string()))
        .collect();
    if perms_obj.get(key).and_then(Value::as_array) == Some(&replacement) {
        return Ok(None);
    }
    if let Some(existing) = perms_obj.get(key) {
        if !existing.is_array() {
            return Err(());
        }
    }
    perms_obj.insert(key.to_string(), Value::Array(replacement));
    let serialized = serde_json::to_string_pretty(&root).map_err(|_| ())?;
    Ok(Some(serialized + "\n"))
}

fn set_default_mode_in_settings_json(raw: &str, mode: &str) -> Result<Option<String>, ()> {
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|_| ())?
    };
    let obj = root.as_object_mut().ok_or(())?;
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    let perms_obj = perms.as_object_mut().ok_or(())?;
    if perms_obj.get("defaultMode").and_then(Value::as_str) == Some(mode) {
        return Ok(None);
    }
    if let Some(existing) = perms_obj.get("defaultMode") {
        if !existing.is_string() {
            return Err(());
        }
    }
    perms_obj.insert("defaultMode".to_string(), json!(mode));
    let serialized = serde_json::to_string_pretty(&root).map_err(|_| ())?;
    Ok(Some(serialized + "\n"))
}

/// Remove `update.rule` from its destination settings file — the inverse of
/// [`persist_permission_update`], for the interactive `/permissions` delete
/// (PERM-1). Best-effort + idempotent: `Ok(true)` when an entry was removed,
/// `Ok(false)` when the destination is not persistable or no entry matched.
///
/// # Errors
/// [`PersistError::BrokenJson`] if the destination is non-empty and not valid
/// JSON (left untouched); [`PersistError::Confined`] on a hardened filesystem failure.
pub async fn remove_permission_update(
    update: &PermissionUpdate,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    mutate_settings_file(paths, update.destination, false, |raw| {
        remove_rule_from_settings_json(raw, &update.rule)
    })
}

/// Replace every rule in one behavior bucket at `destination` atomically.
/// Session and CLI destinations are live-only and therefore no-op here.
pub async fn replace_permission_rules(
    behavior: PermissionBehavior,
    rules: &[PermissionRule],
    destination: PermissionUpdateDestination,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    mutate_settings_file(paths, destination, true, |raw| {
        replace_rules_in_settings_json(raw, behavior, rules)
    })
}

/// The outcome of an [`persist_auto_mode_save`] write.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoModeSaveOutcome {
    /// `true` when the settings file was written (the `autoMode` block or a
    /// removal changed something); `false` for a no-op or a non-persistable
    /// destination.
    pub wrote: bool,
    /// How many `permissions.allow` entries the removal set actually filtered
    /// out (`permissionsAllowRemoved`).
    pub removed_count: usize,
    /// How many requested removals matched nothing (`permissionsAllowNotFound`).
    pub not_found_count: usize,
    /// `permissions.allow` was absent or not an array, so the removals were
    /// skipped rather than applied (`permissionsAllowSkipped`).
    pub permissions_allow_skipped: bool,
    /// How many pre-existing `environment` entries the merge preserved
    /// (`environmentEntriesPreserved`).
    pub environment_entries_preserved: usize,
    /// The `autoMode` keys written (`autoModeKeysWritten`).
    pub auto_mode_keys_written: Vec<String>,
    /// Post-write size advisories.
    pub warnings: Vec<String>,
}

/// Persist a WIZARD-06 auto-mode save (oracle `rFt`): set the top-level
/// `autoMode` block and remove the offered `removeFromPermissionsAllow` rules
/// from `permissions.allow`, in the ONE `destination` settings file, atomically
/// (one exclusive lock, one atomic write). The caller has already run
/// [`crate::auto_mode_setup::validate_auto_mode_save`], so an empty/"nothing to
/// save" payload never reaches here.
///
/// A missing settings file is treated as empty (the write creates it), matching
/// the wizard's "harden BEFORE enabling auto mode" first-run flow. Session and
/// CLI destinations are live-only and no-op (`wrote: false`).
///
/// # Errors
/// [`PersistError::BrokenJson`] if the destination file is non-empty and not a
/// JSON object (left untouched); [`PersistError::Confined`] on a hardened
/// filesystem failure.
pub async fn persist_auto_mode_save(
    auto_mode_block: Option<&Value>,
    remove: &[String],
    destination: PermissionUpdateDestination,
    paths: &PermissionPaths,
    mode: crate::auto_mode_setup::AutoModeSaveMode,
) -> Result<AutoModeSaveOutcome, PersistError> {
    use crate::auto_mode_setup::AutoModeSaveError;

    // The transform runs inside the closure, under the same exclusive lock as
    // the write, so the merge always sees the bytes it is about to replace.
    let captured: std::cell::RefCell<Option<AutoModeSaveResultParts>> =
        std::cell::RefCell::new(None);
    let invalid_merged: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);

    let wrote = mutate_settings_file(paths, destination, true, |raw| {
        match crate::auto_mode_setup::apply_auto_mode_save_to_settings_json(
            raw,
            auto_mode_block,
            remove,
            mode,
        ) {
            Ok(Some(result)) => {
                *captured.borrow_mut() = Some(AutoModeSaveResultParts {
                    removed_count: result.removed_count,
                    not_found_count: result.not_found_count,
                    permissions_allow_skipped: result.permissions_allow_skipped,
                    environment_entries_preserved: result.environment_entries_preserved,
                    auto_mode_keys_written: result.auto_mode_keys_written,
                    warnings: result.warnings,
                });
                Ok(Some(result.json))
            }
            Ok(None) => Ok(None),
            Err(AutoModeSaveError::InvalidMerged(reason)) => {
                *invalid_merged.borrow_mut() = Some(reason);
                Err(())
            }
            Err(AutoModeSaveError::BrokenSettings) => Err(()),
        }
    });

    // An invalid MERGE is not a broken settings file: report it as its own
    // error so the caller can emit `invalid_merged` rather than `write_failed`.
    if let Some(reason) = invalid_merged.into_inner() {
        return Err(PersistError::InvalidMerged(reason));
    }
    let wrote = wrote?;

    let parts = captured.into_inner().unwrap_or_default();
    Ok(AutoModeSaveOutcome {
        wrote,
        removed_count: parts.removed_count,
        not_found_count: parts.not_found_count,
        permissions_allow_skipped: parts.permissions_allow_skipped,
        environment_entries_preserved: parts.environment_entries_preserved,
        auto_mode_keys_written: parts.auto_mode_keys_written,
        warnings: parts.warnings,
    })
}

/// The parts of an [`crate::auto_mode_setup::AutoModeSaveResult`] threaded out
/// of the mutation closure.
#[derive(Default)]
struct AutoModeSaveResultParts {
    removed_count: usize,
    not_found_count: usize,
    permissions_allow_skipped: bool,
    environment_entries_preserved: usize,
    auto_mode_keys_written: Vec<String>,
    warnings: Vec<String>,
}

/// Persist a `setMode` permission update as `permissions.defaultMode`.
///
/// The 2.1.218 oracle's persist function `eSe` guards its TOP with a dedicated
/// session-scoped early-return: `if(e.type==="setMode"&&e.mode==="bypassPermissions")
/// {C("setMode:'bypassPermissions' is session-scoped; not persisting as
/// defaultMode…");return}`. So `bypassPermissions` is DELIBERATELY never written
/// to disk — persisting it would silently re-enter bypass mode on the next
/// session load (a persistent privilege escalation, since the load path gates
/// only on the `bypass_disabled` kill switch, not the runtime availability
/// re-check). Every OTHER recognized mode IS persisted; an unrecognized mode
/// string is ignored (`Ok(false)`).
pub async fn persist_permission_mode(
    mode: &str,
    destination: PermissionUpdateDestination,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    // `eSe`'s session-scoped guard: bypassPermissions is never persisted.
    if mode == "bypassPermissions" {
        return Ok(false);
    }
    if !matches!(
        mode,
        "default" | "acceptEdits" | "plan" | "dontAsk" | "auto"
    ) {
        return Ok(false);
    }
    mutate_settings_file(paths, destination, true, |raw| {
        set_default_mode_in_settings_json(raw, mode)
    })
}

/// (PERM-1 Workspace tab) Add or remove `dir` in
/// `permissions.additionalDirectories` of `raw` settings JSON. Returns:
/// - `Ok(Some(new_json))` — the array changed; pretty-printed + trailing
///   newline, preserving every other key.
/// - `Ok(None)` — no change needed (add: already present; remove: absent).
/// - `Err(())` — `raw` is non-empty and not a JSON object, or
///   `permissions`/`additionalDirectories` is the wrong type (caller maps to
///   [`PersistError::BrokenJson`] and must NOT overwrite).
///
/// Directory entries are compared verbatim (claude-code stores the path
/// string as-typed; no canonicalization).
fn apply_directory_to_settings_json(raw: &str, dir: &str, add: bool) -> Result<Option<String>, ()> {
    mutate_directories_in_settings_json(raw, &[dir], add)
}

fn mutate_directories_in_settings_json(
    raw: &str,
    directories: &[&str],
    add: bool,
) -> Result<Option<String>, ()> {
    if directories.is_empty() {
        return Ok(None);
    }
    if !add && raw.trim().is_empty() {
        return Ok(None);
    }
    let mut root: Value = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|_| ())?
    };
    let obj = root.as_object_mut().ok_or(())?;

    if add {
        let perms = obj.entry("permissions").or_insert_with(|| json!({}));
        let perms_obj = perms.as_object_mut().ok_or(())?;
        let arr = perms_obj
            .entry("additionalDirectories")
            .or_insert_with(|| json!([]));
        let arr_vec = arr.as_array_mut().ok_or(())?;
        let mut changed = false;
        for directory in directories {
            if !arr_vec
                .iter()
                .filter_map(Value::as_str)
                .any(|existing| existing == *directory)
            {
                arr_vec.push(json!(directory));
                changed = true;
            }
        }
        if !changed {
            return Ok(None);
        }
    } else {
        let Some(perms) = obj.get_mut("permissions") else {
            return Ok(None);
        };
        let perms_obj = perms.as_object_mut().ok_or(())?;
        let Some(arr) = perms_obj.get_mut("additionalDirectories") else {
            return Ok(None);
        };
        let arr_vec = arr.as_array_mut().ok_or(())?;
        let before = arr_vec.len();
        arr_vec.retain(|entry| {
            entry
                .as_str()
                .is_none_or(|value| !directories.contains(&value))
        });
        if arr_vec.len() == before {
            return Ok(None); // nothing matched.
        }
    }
    let serialized = serde_json::to_string_pretty(&root).map_err(|_| ())?;
    Ok(Some(serialized + "\n"))
}

/// (PERM-1 Workspace tab) Add (`add = true`) or remove (`add = false`) a
/// workspace directory in the destination settings file's
/// `permissions.additionalDirectories` array. Best-effort + idempotent:
/// `Ok(true)` when the file changed, `Ok(false)` when the destination is not
/// persistable or no change was needed.
///
/// # Errors
/// [`PersistError::BrokenJson`] if the destination is non-empty and not valid
/// JSON (left untouched); [`PersistError::Confined`] on a hardened filesystem failure.
pub async fn persist_workspace_directory(
    dir: &str,
    add: bool,
    dest: PermissionUpdateDestination,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    mutate_settings_file(paths, dest, add, |raw| {
        apply_directory_to_settings_json(raw, dir, add)
    })
}

/// Add or remove multiple workspace directories in one locked atomic update.
pub async fn persist_workspace_directories(
    directories: &[String],
    add: bool,
    dest: PermissionUpdateDestination,
    paths: &PermissionPaths,
) -> Result<bool, PersistError> {
    let directories: Vec<&str> = directories.iter().map(String::as_str).collect();
    if directories.is_empty() {
        return Ok(false);
    }
    mutate_settings_file(paths, dest, add, |raw| {
        mutate_directories_in_settings_json(raw, &directories, add)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::PermissionRuleSource;

    fn allow_rule(spec: &str, dest: PermissionUpdateDestination) -> PermissionUpdate {
        PermissionUpdate {
            rule: PermissionRule {
                value: PermissionRuleValue::from_rule_string(spec),
                behavior: PermissionBehavior::Allow,
                source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
            },
            destination: dest,
        }
    }

    // ── pure merge core ──────────────────────────────────────────────────

    #[test]
    fn adds_rule_to_empty_settings() {
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        let out = apply_rule_to_settings_json("", &rule).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));
        assert!(out.ends_with('\n'), "trailing newline");
    }

    #[test]
    fn appends_to_existing_array_and_preserves_other_keys() {
        let raw = r#"{ "model": "claude-opus-4-7", "permissions": { "allow": ["Read"], "deny": ["Bash(rm:*)"] } }"#;
        let rule = allow_rule("Edit(src/**)", PermissionUpdateDestination::LocalSettings).rule;
        let out = apply_rule_to_settings_json(raw, &rule).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        // appended, existing allow + deny + unrelated key preserved.
        assert_eq!(v["permissions"]["allow"], json!(["Read", "Edit(src/**)"]));
        assert_eq!(v["permissions"]["deny"], json!(["Bash(rm:*)"]));
        assert_eq!(v["model"], json!("claude-opus-4-7"));
    }

    #[test]
    fn dedups_exact_and_legacy_normalized() {
        // exact duplicate → no change.
        let raw = r#"{ "permissions": { "allow": ["Bash"] } }"#;
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        assert!(apply_rule_to_settings_json(raw, &rule).unwrap().is_none());

        // legacy alias on disk ("Task") normalizes to "Agent"; adding "Agent"
        // (or "Task", which parses to Agent) is a no-op.
        let raw2 = r#"{ "permissions": { "allow": ["Task"] } }"#;
        let agent = allow_rule("Agent", PermissionUpdateDestination::LocalSettings).rule;
        assert!(apply_rule_to_settings_json(raw2, &agent).unwrap().is_none());
    }

    #[test]
    fn creates_permissions_object_when_absent() {
        let raw = r#"{ "model": "x" }"#;
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        let out = apply_rule_to_settings_json(raw, &rule).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));
        assert_eq!(v["model"], json!("x"));
    }

    // ── remove (PERM-1) ──────────────────────────────────────────────────

    #[test]
    fn removes_rule_preserving_others() {
        let raw = r#"{ "model": "x", "permissions": { "allow": ["Read", "Edit(src/**)"], "deny": ["Bash(rm:*)"] } }"#;
        let rule = allow_rule("Read", PermissionUpdateDestination::LocalSettings).rule;
        let out = remove_rule_from_settings_json(raw, &rule).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Edit(src/**)"]));
        assert_eq!(v["permissions"]["deny"], json!(["Bash(rm:*)"]));
        assert_eq!(v["model"], json!("x"));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn remove_is_noop_when_absent_or_empty() {
        // Rule not present → None.
        let raw = r#"{ "permissions": { "allow": ["Read"] } }"#;
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        assert!(remove_rule_from_settings_json(raw, &rule)
            .unwrap()
            .is_none());
        // Empty / missing permissions → None.
        assert!(remove_rule_from_settings_json("", &rule).unwrap().is_none());
        assert!(remove_rule_from_settings_json(r#"{ "model": "x" }"#, &rule)
            .unwrap()
            .is_none());
    }

    #[test]
    fn remove_matches_legacy_alias() {
        // On-disk "Task" normalizes to "Agent"; removing "Agent" removes it.
        let raw = r#"{ "permissions": { "allow": ["Task", "Read"] } }"#;
        let agent = allow_rule("Agent", PermissionUpdateDestination::LocalSettings).rule;
        let out = remove_rule_from_settings_json(raw, &agent)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Read"]));
    }

    #[test]
    fn remove_broken_json_errors_without_write() {
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        assert!(remove_rule_from_settings_json("{not json", &rule).is_err());
    }

    #[test]
    fn replace_rules_and_default_mode_preserve_unrelated_settings() {
        let raw =
            r#"{ "model": "x", "permissions": { "allow": ["Read"], "deny": ["Bash(rm:*)"] } }"#;
        let rules = [allow_rule("Edit(src/**)", PermissionUpdateDestination::ProjectSettings).rule];
        let replaced = replace_rules_in_settings_json(raw, PermissionBehavior::Allow, &rules)
            .unwrap()
            .unwrap();
        let with_mode = set_default_mode_in_settings_json(&replaced, "plan")
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&with_mode).unwrap();
        assert_eq!(value["model"], "x");
        assert_eq!(value["permissions"]["allow"], json!(["Edit(src/**)"]));
        assert_eq!(value["permissions"]["deny"], json!(["Bash(rm:*)"]));
        assert_eq!(value["permissions"]["defaultMode"], "plan");
    }

    #[test]
    fn replace_rules_rejects_behavior_mismatch() {
        let deny_rule = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Bash(rm:*)"),
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
        };
        assert!(replace_rules_in_settings_json(
            r#"{ "permissions": { "allow": ["Read"] } }"#,
            PermissionBehavior::Allow,
            &[deny_rule],
        )
        .is_err());
    }

    #[test]
    fn broken_json_errors() {
        let rule = allow_rule("Bash", PermissionUpdateDestination::LocalSettings).rule;
        assert!(apply_rule_to_settings_json("{not json", &rule).is_err());
        // a top-level array is not an object → error (don't clobber).
        assert!(apply_rule_to_settings_json("[]", &rule).is_err());
    }

    #[test]
    fn deny_rule_goes_to_deny_array() {
        let rule = PermissionRule {
            value: PermissionRuleValue::from_rule_string("Read(./secrets/**)"),
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local),
        };
        let out = apply_rule_to_settings_json("{}", &rule).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["deny"], json!(["Read(./secrets/**)"]));
    }

    // ── path resolution ──────────────────────────────────────────────────

    #[test]
    fn destination_paths() {
        let p = PermissionPaths {
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
            cwd: PathBuf::from("/proj"),
        };
        assert_eq!(
            p.destination_path(PermissionUpdateDestination::UserSettings),
            Some(PathBuf::from("/home/u/.lingxi/settings.json"))
        );
        assert_eq!(
            p.destination_path(PermissionUpdateDestination::ProjectSettings),
            Some(PathBuf::from("/proj/.lingxi/settings.json"))
        );
        assert_eq!(
            p.destination_path(PermissionUpdateDestination::LocalSettings),
            Some(PathBuf::from("/proj/.lingxi/settings.local.json"))
        );
        assert!(p
            .destination_path(PermissionUpdateDestination::Session)
            .is_none());
        assert!(p
            .destination_path(PermissionUpdateDestination::CliArg)
            .is_none());
    }

    // ── async I/O wrapper ────────────────────────────────────────────────

    #[tokio::test]
    async fn persist_creates_local_settings_and_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("lx-3c-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::LocalSettings);

        // First persist writes the file + creates .lingxi/.
        assert!(persist_permission_update(&update, &paths).await.unwrap());
        let path = tmp.join("proj/.lingxi/settings.local.json");
        let written = std::fs::read_to_string(&path).unwrap();
        let v: Value = serde_json::from_str(&written).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));
        // The oracle git-ignores the file via the GLOBAL git excludes, never the
        // project `.gitignore` — and only when the cwd is a git repo (this temp
        // dir is not). So no project `.gitignore` is ever created here.
        assert!(!tmp.join("proj/.gitignore").exists());

        // Second persist of the same rule is a no-op (idempotent).
        assert!(!persist_permission_update(&update, &paths).await.unwrap());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn persistent_variants_survive_reload_but_bypass_mode_does_not() {
        let tmp = std::env::temp_dir().join(format!("lx-persist-variants-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let destination = PermissionUpdateDestination::LocalSettings;
        let initial = [allow_rule("Read", destination).rule];
        assert!(
            replace_permission_rules(PermissionBehavior::Allow, &initial, destination, &paths)
                .await
                .unwrap()
        );
        assert!(persist_permission_mode("plan", destination, &paths)
            .await
            .unwrap());
        // SECURITY: `bypassPermissions` is session-scoped and is NEVER persisted
        // (oracle `eSe`'s top-of-function guard). The call is a no-op that
        // returns false and leaves the prior defaultMode untouched — persisting
        // it would silently re-enter bypass mode on the next session load.
        assert!(
            !persist_permission_mode("bypassPermissions", destination, &paths)
                .await
                .unwrap()
        );
        let body = std::fs::read_to_string(tmp.join("proj/.lingxi/settings.local.json")).unwrap();
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["permissions"]["allow"], json!(["Read"]));
        assert_eq!(
            value["permissions"]["defaultMode"], "plan",
            "the last non-bypass mode survives; bypassPermissions is not written"
        );
        // An unrecognized mode string is still ignored.
        assert!(!persist_permission_mode("nonsense", destination, &paths)
            .await
            .unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A symlinked settings DIRECTORY (`.lingxi` → elsewhere) is ACCEPTED — the
    /// common dotfiles pattern. The oracle opens the (resolved) staging dir with
    /// O_NOFOLLOW but resolves intermediate components normally, so the write
    /// lands in the symlink's target.
    #[cfg(unix)]
    #[tokio::test]
    async fn confined_persistence_accepts_symlinked_settings_directory() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("lx-persist-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        std::fs::create_dir_all(tmp.join("outside")).unwrap();
        symlink(tmp.join("outside"), tmp.join("proj/.lingxi")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::LocalSettings);
        assert!(persist_permission_update(&update, &paths).await.unwrap());
        // The write followed the symlink to its target.
        let body = std::fs::read_to_string(tmp.join("outside/settings.local.json")).unwrap();
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["permissions"]["allow"], json!(["Bash"]));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// SECURITY: a symlinked settings FILE (`settings.local.json` itself → an
    /// outside path) is STILL rejected — the final file (and its staging temp)
    /// keep O_NOFOLLOW, so the write can never be redirected out of the resolved
    /// settings dir by swapping the file for a symlink.
    #[cfg(unix)]
    #[tokio::test]
    async fn confined_persistence_still_rejects_symlinked_settings_file() {
        use std::os::unix::fs::symlink;

        let tmp = std::env::temp_dir().join(format!("lx-persist-flink-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj/.lingxi")).unwrap();
        std::fs::write(tmp.join("outside-secret"), "secret\n").unwrap();
        // The settings FILE is a symlink pointing outside the settings dir.
        symlink(
            tmp.join("outside-secret"),
            tmp.join("proj/.lingxi/settings.local.json"),
        )
        .unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::LocalSettings);
        let error = persist_permission_update(&update, &paths)
            .await
            .unwrap_err();
        assert!(matches!(error, PersistError::Confined { .. }));
        // The outside target was NOT written through the symlink.
        assert_eq!(
            std::fs::read_to_string(tmp.join("outside-secret")).unwrap(),
            "secret\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_gitignore_is_never_touched_by_persistence() {
        use std::os::unix::fs::symlink;

        let tmp =
            std::env::temp_dir().join(format!("lx-persist-gitignore-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        std::fs::write(tmp.join("outside-gitignore"), "outside\n").unwrap();
        // A hostile symlinked project `.gitignore` pointing outside the repo.
        symlink(tmp.join("outside-gitignore"), tmp.join("proj/.gitignore")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::LocalSettings);
        // Settings persistence succeeds…
        assert!(persist_permission_update(&update, &paths).await.unwrap());
        assert!(tmp.join("proj/.lingxi/settings.local.json").exists());
        // …and the oracle NEVER writes the project `.gitignore` (it uses the
        // global git excludes), so a symlinked project `.gitignore` can never be
        // followed out of the repo — the outside target is untouched.
        assert_eq!(
            std::fs::read_to_string(tmp.join("outside-gitignore")).unwrap(),
            "outside\n"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    // `uEh` is the oracle's own symbol name; lower-casing it would stop the
    // test naming what it pins.
    #[allow(non_snake_case)]
    fn excludes_path_resolution_matches_uEh_fallbacks() {
        use super::excludes_path_from;
        // `git config` gave an absolute path → used verbatim.
        assert_eq!(
            excludes_path_from(Some("/etc/gitignore_global"), "/home/u", None),
            PathBuf::from("/etc/gitignore_global")
        );
        // `~` ⇒ home; `~/x` ⇒ home/x.
        assert_eq!(
            excludes_path_from(Some("~"), "/home/u", None),
            PathBuf::from("/home/u")
        );
        assert_eq!(
            excludes_path_from(Some("~/gitignore"), "/home/u", None),
            PathBuf::from("/home/u/gitignore")
        );
        // Empty / relative config → fall through to XDG, then the ~/.config default.
        assert_eq!(
            excludes_path_from(Some(""), "/home/u", Some("/xdg")),
            PathBuf::from("/xdg/git/ignore")
        );
        assert_eq!(
            excludes_path_from(Some("relative/path"), "/home/u", None),
            PathBuf::from("/home/u/.config/git/ignore")
        );
        // A relative XDG is ignored (oracle `isAbsolute` guard).
        assert_eq!(
            excludes_path_from(None, "/home/u", Some("rel")),
            PathBuf::from("/home/u/.config/git/ignore")
        );
    }

    #[tokio::test]
    async fn persist_session_destination_is_noop() {
        let paths = PermissionPaths {
            lingxi_home: PathBuf::from("/nonexistent/.lingxi"),
            cwd: PathBuf::from("/nonexistent/proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::Session);
        // No file resolved → Ok(false), nothing written, no error.
        assert!(!persist_permission_update(&update, &paths).await.unwrap());
    }

    #[tokio::test]
    async fn persist_broken_json_errors_without_clobber() {
        let tmp = std::env::temp_dir().join(format!("lx-3c-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let dir = tmp.join("proj/.lingxi");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.local.json");
        std::fs::write(&path, "{ broken").unwrap();

        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let update = allow_rule("Bash", PermissionUpdateDestination::LocalSettings);
        let err = persist_permission_update(&update, &paths)
            .await
            .unwrap_err();
        assert!(matches!(err, PersistError::BrokenJson(_)));
        // File left untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── (PERM-1 Workspace tab) additionalDirectories add/remove ──────────

    #[test]
    fn adds_directory_to_empty_settings() {
        let out = apply_directory_to_settings_json("", "/extra/dir", true)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["permissions"]["additionalDirectories"],
            json!(["/extra/dir"])
        );
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn adds_directory_preserving_other_keys_and_dedups() {
        let raw = r#"{ "model": "x", "permissions": { "allow": ["Read"], "additionalDirectories": ["/a"] } }"#;
        let out = apply_directory_to_settings_json(raw, "/b", true)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["permissions"]["additionalDirectories"],
            json!(["/a", "/b"])
        );
        assert_eq!(v["permissions"]["allow"], json!(["Read"]));
        assert_eq!(v["model"], json!("x"));
        // Adding an already-present dir is a no-op.
        assert!(apply_directory_to_settings_json(raw, "/a", true)
            .unwrap()
            .is_none());
    }

    #[test]
    fn removes_directory_and_noop_when_absent() {
        let raw = r#"{ "permissions": { "additionalDirectories": ["/a", "/b"] } }"#;
        let out = apply_directory_to_settings_json(raw, "/a", false)
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["permissions"]["additionalDirectories"], json!(["/b"]));
        // Removing an absent dir is a no-op.
        assert!(apply_directory_to_settings_json(raw, "/nope", false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn directory_broken_json_errors_without_overwrite() {
        assert!(apply_directory_to_settings_json("{ broken", "/x", true).is_err());
    }

    #[tokio::test]
    async fn persist_workspace_directory_roundtrips_to_local_settings() {
        let tmp = std::env::temp_dir().join(format!("lx-ws-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        // Add → file created with the dir.
        let added = persist_workspace_directory(
            "/work/extra",
            true,
            PermissionUpdateDestination::LocalSettings,
            &paths,
        )
        .await
        .unwrap();
        assert!(added);
        let path = tmp.join("proj/.lingxi/settings.local.json");
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("/work/extra"));
        // Remove → gone.
        let removed = persist_workspace_directory(
            "/work/extra",
            false,
            PermissionUpdateDestination::LocalSettings,
            &paths,
        )
        .await
        .unwrap();
        assert!(removed);
        let body2 = std::fs::read_to_string(&path).unwrap();
        assert!(!body2.contains("/work/extra"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn auto_mode_save_writes_block_and_removes_allow_atomically() {
        let tmp = std::env::temp_dir().join(format!("lx-automode-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("proj")).unwrap();
        let paths = PermissionPaths {
            lingxi_home: tmp.join("home/.lingxi"),
            cwd: tmp.join("proj"),
        };
        let destination = PermissionUpdateDestination::LocalSettings;
        let path = tmp.join("proj/.lingxi/settings.local.json");

        // Seed an existing allow list with a destructive rule the offer removes.
        let seed = [
            allow_rule("Bash(rm:*)", destination).rule,
            allow_rule("Read", destination).rule,
        ];
        assert!(
            replace_permission_rules(PermissionBehavior::Allow, &seed, destination, &paths)
                .await
                .unwrap()
        );

        let block =
            json!({ "environment": ["Solo laptop"], "hard_deny": ["Bash(curl:*)", "$defaults"] });
        let outcome = persist_auto_mode_save(
            Some(&block),
            &["Bash(rm:*)".to_string()],
            destination,
            &paths,
            crate::auto_mode_setup::AutoModeSaveMode::Append,
        )
        .await
        .unwrap();
        assert!(outcome.wrote);
        assert_eq!(outcome.removed_count, 1);

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // Rule arrays are MERGED, so `$defaults` leads the written array.
        assert_eq!(
            v["autoMode"],
            json!({ "environment": ["Solo laptop"], "hard_deny": ["$defaults", "Bash(curl:*)"] })
        );
        assert_eq!(
            v["permissions"]["allow"],
            json!(["Read"]),
            "destructive rule removed"
        );

        // Re-applying the identical block with no fresh removal is a no-op.
        let again = persist_auto_mode_save(
            Some(&block),
            &[],
            destination,
            &paths,
            crate::auto_mode_setup::AutoModeSaveMode::Append,
        )
        .await
        .unwrap();
        assert!(!again.wrote, "unchanged block + no removal writes nothing");
        assert_eq!(again.removed_count, 0);

        // A requested removal that no longer matches → wrote:false, removed_count:0
        // (the oracle's permissions_allow_skipped telemetry branch).
        let skipped = persist_auto_mode_save(
            Some(&block),
            &["Bash(rm:*)".to_string()],
            destination,
            &paths,
            crate::auto_mode_setup::AutoModeSaveMode::Append,
        )
        .await
        .unwrap();
        assert!(!skipped.wrote);
        assert_eq!(skipped.removed_count, 0);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
