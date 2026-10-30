//! `updateSettingsForSource` / `getSettingsForSource` port
//! (`utils/settings/settings.ts` L416/L459 semantics) for the two sources
//! the migrations write. Operates on raw JSON maps — unknown keys in the
//! user's real settings.json are preserved verbatim.
//!
//! Same error contract as the proven `commands/core/effort.rs` port:
//! missing/empty file merges into an empty object; syntactically broken JSON
//! bails WITHOUT overwriting. (`effort.rs`/`permission::persist`/tools-meta carry
//! private copies of this logic; consolidating them here is a noted follow-up,
//! out of scope for this batch.)

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde_json::{Map, Value};

/// Which settings file to address.
///
/// `User` → `<claude-config-home>/settings.json`, `Project` →
/// `<project>/.lingxi/settings.json`, `Local` →
/// `<project>/.lingxi/settings.local.json`; [`settings_path`] resolves them.
pub use lingxi_core::types::WritableScope;

/// Resolve the file path for a source (TS `getSettingsFilePathForSource`).
#[must_use]
pub fn settings_path(source: WritableScope, lingxi_home: &Path, project_dir: &Path) -> PathBuf {
    match source {
        WritableScope::User => lingxi_home.join("settings.json"),
        WritableScope::Project => project_dir.join(branding::DOT_DIR).join("settings.json"),
        WritableScope::Local => project_dir
            .join(branding::DOT_DIR)
            .join("settings.local.json"),
    }
}

/// Raw read of a settings file. Missing / blank ⇒ empty map; broken JSON ⇒
/// `Err` (caller decides; in TS a broken file just yields `settings: null`
/// from `getSettingsForSource`, so migration callers generally treat `Err`
/// as "no settings" or warn-and-continue).
pub fn read_settings_map(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => Ok(Map::new()),
        Ok(content) => serde_json::from_str(&content)
            .map_err(|_| format!("Invalid JSON syntax in settings file at {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!(
            "Failed to read raw settings from {}: {e}",
            path.display()
        )),
    }
}

/// Read a settings file as its exact source JSON object. Unlike
/// [`read_settings_map`], this preserves JavaScript UTF-16 strings and object
/// keys—including unpaired surrogates—without constructing `serde_json::Value`.
/// Missing or blank files are an empty object, matching the existing settings
/// writer's read contract.
pub fn read_settings_source(path: &Path) -> Result<Utf16JsonProjection, String> {
    match std::fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => empty_settings_source(),
        Ok(content) => {
            let source = Utf16JsonProjection::parse(&content).map_err(|_| {
                format!("Invalid JSON syntax in settings file at {}", path.display())
            })?;
            if source.object_entries().is_none() {
                return Err(format!(
                    "Invalid JSON syntax in settings file at {}",
                    path.display()
                ));
            }
            Ok(source)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => empty_settings_source(),
        Err(error) => Err(format!(
            "Failed to read raw settings from {}: {error}",
            path.display()
        )),
    }
}

fn empty_settings_source() -> Result<Utf16JsonProjection, String> {
    Utf16JsonProjection::object(Vec::new()).map_err(|error| error.to_string())
}

/// `updateSettingsForSource`: apply top-level key updates. `Some(v)` sets the
/// key, `None` deletes it (TS `mergeWith` treats `undefined` as delete).
/// All other exact source values and UTF-16 keys are preserved; output is
/// pretty-printed with a trailing newline.
///
/// CALLER CONTRACT — top-level REPLACE, not deep-merge: TS uses a lodash
/// `mergeWith` (deep; arrays replace), but every migration pre-builds its
/// nested values (e.g. the spread-merged `env` map), so top-level replace
/// coincides for all current callers. A future caller passing a nested
/// partial would silently diverge — pre-merge at the call site.
///
/// `bridge_server::settings_bridge::apply_patch` delegates to the
/// `update_settings_with_before_publish` transaction below so it can call
/// `permission::mark_internal_write` immediately before the write without
/// duplicating this lock and publication implementation. `migrations` remains
/// independent of `permission`.
///
/// Error contract: the TS original NEVER throws — every failure path returns
/// `{error: Error}` (settings.ts:416-523), and every migration discards that
/// return. So in the migration ports `Err` from this function maps to
/// warn-and-continue; only `global_config::save_map` failures (the
/// `saveGlobalConfig` analog, which CAN throw in TS) map to a TS catch path.
pub fn update_settings(path: &Path, updates: Vec<(String, Option<Value>)>) -> Result<(), String> {
    update_settings_with_before_publish(path, updates, || {})
}

/// Apply top-level updates while invoking `before_publish` after the new JSON
/// has been read, merged, and serialized, but immediately before the atomic
/// settings replacement starts. The callback runs while this path's
/// process-local read-modify-write lock is held, so callers can attach a
/// process-local side effect (for example, a filesystem-watcher suppression
/// marker) without widening the lock scope or duplicating the writer.
pub fn update_settings_with_before_publish<F>(
    path: &Path,
    updates: Vec<(String, Option<Value>)>,
    before_publish: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    let source_updates = updates
        .into_iter()
        .map(|(key, value)| {
            let value = value
                .map(|value| {
                    Utf16JsonProjection::from_serde_value(&value).map_err(|error| {
                        format!("Failed to update settings field `{key}`: {error}")
                    })
                })
                .transpose()?;
            Ok((key, value))
        })
        .collect::<Result<Vec<_>, String>>()?;
    update_settings_source_with_before_publish(path, source_updates, before_publish)
}

/// Apply shallow top-level updates to a settings file without converting
/// arbitrary source JSON through `serde_json::Value`. Existing file values and
/// exact UTF-16 keys are retained; `None` deletes the named key. The path lock,
/// watcher callback, atomic publication, mode preservation, and fallback are
/// shared with [`update_settings_with_before_publish`].
pub fn update_settings_source_with_before_publish<F>(
    path: &Path,
    updates: Vec<(String, Option<Utf16JsonProjection>)>,
    before_publish: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    with_settings_file_lock(path, |target| {
        let mut source = read_settings_source(target)?;
        for (key, value) in updates {
            match value {
                Some(value) => source
                    .set_field(&key, value)
                    .map_err(|error| format!("Failed to update settings field `{key}`: {error}"))?,
                None => {
                    source = remove_settings_source_field(&source, &key).map_err(|error| {
                        format!("Failed to remove settings field `{key}`: {error}")
                    })?;
                }
            }
        }
        let serialized = source.to_pretty_json_string().map_err(|error| {
            format!(
                "Failed to serialize settings for {}: {error}",
                target.display()
            )
        })?;
        before_publish();
        write_settings_atomic(target, (serialized + "\n").as_bytes())
            .map_err(|error| format!("Failed to write settings to {}: {error}", target.display()))
    })
}

/// Receipt for one source-preserving settings mutation. Rollback restores the
/// exact prior bytes if this mutation is still the latest write; otherwise it
/// applies a compare-and-swap reverse delta to the current raw JSON so later
/// writers' values survive.
#[derive(Debug, Clone)]
pub struct SettingsUpdateReceipt {
    target: PathBuf,
    before_bytes: Option<Vec<u8>>,
    after_bytes: Vec<u8>,
    before_source: Utf16JsonProjection,
    after_source: Utf16JsonProjection,
}

/// Transform the latest raw settings source under the shared settings locks
/// and return a rollback receipt for the exact mutation made by `transform`.
/// A no-op transform does not write the file and returns `None`.
pub fn transform_settings_source_with_receipt<F>(
    path: &Path,
    transform: F,
) -> Result<Option<SettingsUpdateReceipt>, String>
where
    F: FnOnce(&mut Utf16JsonProjection) -> Result<(), String>,
{
    with_settings_file_lock(path, |target| {
        let before_bytes = match std::fs::read(target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "Failed to read raw settings from {}: {error}",
                    target.display()
                ));
            }
        };
        let mut after_source = read_settings_source(target)?;
        let before_source = after_source.clone();
        transform(&mut after_source)?;
        if source_values_equal(&before_source, &after_source) {
            return Ok(None);
        }
        let serialized = after_source.to_pretty_json_string().map_err(|error| {
            format!(
                "Failed to serialize settings for {}: {error}",
                target.display()
            )
        })?;
        let after_bytes = (serialized + "\n").into_bytes();
        write_settings_atomic(target, &after_bytes).map_err(|error| {
            format!("Failed to write settings to {}: {error}", target.display())
        })?;
        Ok(Some(SettingsUpdateReceipt {
            target: target.to_path_buf(),
            before_bytes,
            after_bytes,
            before_source,
            after_source,
        }))
    })
}

/// Roll back a mutation receipt under the same process and file locks as
/// settings writers. If later writes changed the file, restore only values
/// that still match this receipt's after-state, merging object deltas through
/// the raw UTF-16 projection.
pub fn rollback_settings_update(receipt: &SettingsUpdateReceipt) -> Result<bool, String> {
    with_settings_file_lock(&receipt.target, |target| {
        let current_bytes = match std::fs::read(target) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(format!(
                    "Failed to read raw settings from {}: {error}",
                    target.display()
                ));
            }
        };
        if current_bytes.as_deref() == Some(receipt.after_bytes.as_slice()) {
            match &receipt.before_bytes {
                Some(bytes) => write_settings_atomic(target, bytes).map_err(|error| {
                    format!(
                        "Failed to restore settings at {}: {error}",
                        target.display()
                    )
                })?,
                None => match std::fs::remove_file(target) {
                    Ok(()) => {
                        if let Some(parent) = target.parent() {
                            sync_parent_if_supported(parent).map_err(|error| {
                                format!(
                                    "Failed to restore settings at {}: {error}",
                                    target.display()
                                )
                            })?;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
                    Err(error) => {
                        return Err(format!(
                            "Failed to restore settings at {}: {error}",
                            target.display()
                        ));
                    }
                },
            }
            return Ok(true);
        }

        // A missing file is a later writer's full-file deletion. Preserve it.
        if current_bytes.is_none() {
            return Ok(false);
        }
        let current_source = read_settings_source(target)?;
        let restored = rollback_source_delta(
            Some(&current_source),
            Some(&receipt.before_source),
            Some(&receipt.after_source),
        )?;
        let Some(restored) = restored else {
            return Ok(false);
        };
        if source_values_equal(&current_source, &restored) {
            return Ok(false);
        }
        let serialized = restored.to_pretty_json_string().map_err(|error| {
            format!(
                "Failed to serialize restored settings for {}: {error}",
                target.display()
            )
        })?;
        write_settings_atomic(target, (serialized + "\n").as_bytes()).map_err(|error| {
            format!(
                "Failed to restore settings at {}: {error}",
                target.display()
            )
        })?;
        Ok(true)
    })
}

fn source_values_equal(left: &Utf16JsonProjection, right: &Utf16JsonProjection) -> bool {
    match (left.to_json_string(), right.to_json_string()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn rollback_source_delta<'a>(
    current: Option<&'a Utf16JsonProjection>,
    before: Option<&'a Utf16JsonProjection>,
    after: Option<&'a Utf16JsonProjection>,
) -> Result<Option<Utf16JsonProjection>, String> {
    if optional_source_values_equal(before, after) {
        return Ok(current.cloned());
    }
    if optional_source_values_equal(current, after) {
        return Ok(before.cloned());
    }
    // A later writer may remove a parent object after this receipt's update.
    // Keep that deletion instead of rebuilding the old subtree from `before`.
    if current.is_none() && after.is_some() {
        return Ok(None);
    }
    // An explicit empty object is also a later clear when it differs from
    // this receipt's after-state (including an absent after-state). Do not
    // resurrect fields deleted afterward.
    if current
        .and_then(Utf16JsonProjection::object_entries)
        .is_some_and(|entries| entries.is_empty())
    {
        return Ok(current.cloned());
    }

    let before_entries = before.and_then(Utf16JsonProjection::object_entries);
    let after_entries = after.and_then(Utf16JsonProjection::object_entries);
    let current_entries = current.and_then(Utf16JsonProjection::object_entries);
    let can_merge_objects = (before.is_none() || before_entries.is_some())
        && (after.is_none() || after_entries.is_some())
        && (current.is_none() || current_entries.is_some())
        && (before_entries.is_some() || after_entries.is_some());
    if !can_merge_objects {
        return Ok(current.cloned());
    }

    let mut keys: Vec<Vec<u16>> = Vec::new();
    // Keep the latest writer's key order, adding only restored missing keys.
    for entries in [current_entries, before_entries, after_entries]
        .into_iter()
        .flatten()
    {
        for entry in entries {
            if !keys.iter().any(|key| key == entry.code_units) {
                keys.push(entry.code_units.to_vec());
            }
        }
    }
    let mut restored_entries = Vec::new();
    for key in keys {
        let before_value = source_object_value(before, &key)?;
        let after_value = source_object_value(after, &key)?;
        let current_value = source_object_value(current, &key)?;
        if let Some(value) = rollback_source_delta(
            current_value.as_ref(),
            before_value.as_ref(),
            after_value.as_ref(),
        )? {
            restored_entries.push((key, value));
        }
    }
    if restored_entries.is_empty() && before.is_none() {
        return Ok(None);
    }
    Utf16JsonProjection::object_units(restored_entries)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn source_object_value(
    source: Option<&Utf16JsonProjection>,
    key: &[u16],
) -> Result<Option<Utf16JsonProjection>, String> {
    let Some(source) = source else {
        return Ok(None);
    };
    if !source
        .object_entries()
        .is_some_and(|entries| entries.iter().any(|entry| entry.code_units == key))
    {
        return Ok(None);
    }
    source
        .subprojection_property_units(key)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn optional_source_values_equal(
    left: Option<&Utf16JsonProjection>,
    right: Option<&Utf16JsonProjection>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => source_values_equal(left, right),
        _ => false,
    }
}

fn with_settings_file_lock<T, F>(path: &Path, operation: F) -> Result<T, String>
where
    F: FnOnce(&Path) -> Result<T, String>,
{
    let target = crate::global_config::resolve_write_target(path);
    let path_lock = settings_path_lock(&target);
    let _guard = path_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    }
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(
        target
            .file_name()
            .ok_or("Settings path needs a file name")?,
    );
    lock_name.push(".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(target.with_file_name(lock_name))
        .map_err(|error| {
            format!(
                "Failed to open settings lock for {}: {error}",
                target.display()
            )
        })?;
    fs2::FileExt::lock_exclusive(&lock_file)
        .map_err(|error| format!("Failed to lock settings {}: {error}", target.display()))?;
    operation(&target)
}

fn remove_settings_source_field(
    source: &Utf16JsonProjection,
    field: &str,
) -> Result<Utf16JsonProjection, String> {
    let entries = source
        .object_entries()
        .ok_or_else(|| "settings root must be an object".to_owned())?;
    let field_units: Vec<u16> = field.encode_utf16().collect();
    let mut retained = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.code_units == field_units {
            continue;
        }
        let value = source
            .subprojection_property_units(entry.code_units)
            .map_err(|error| error.to_string())?;
        retained.push((entry.code_units.to_vec(), value));
    }
    Utf16JsonProjection::object_units(retained).map_err(|error| error.to_string())
}

/// A process-local lock per resolved settings path, matching the upstream
/// promise queue. Publication also holds the adjacent process-wide file lock.
static SETTINGS_PATH_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[cfg(test)]
static FORCED_RENAME_FAILURES: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();

/// Install a path-scoped deterministic rename failure for migration caller
/// tests. The guard removes the fault on drop, including during unwinding.
#[cfg(test)]
pub(crate) fn force_rename_failure_for_test(path: &Path) -> ForcedRenameFailure {
    let key = normalized_lock_key(&crate::global_config::resolve_write_target(path));
    let failures = FORCED_RENAME_FAILURES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut failures = failures.lock().unwrap_or_else(|poison| poison.into_inner());
    *failures.entry(key.clone()).or_default() += 1;
    ForcedRenameFailure { key }
}

#[cfg(test)]
pub(crate) struct ForcedRenameFailure {
    key: PathBuf,
}

#[cfg(test)]
impl Drop for ForcedRenameFailure {
    fn drop(&mut self) {
        let failures = FORCED_RENAME_FAILURES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut failures = failures.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(count) = failures.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                failures.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
fn update_settings_after_read<F>(
    path: &Path,
    updates: Vec<(String, Option<Value>)>,
    after_read: F,
) -> Result<(), String>
where
    F: FnOnce(),
{
    update_settings_with_hooks(path, updates, after_read, || {})
}

#[cfg(test)]
fn update_settings_with_hooks<A, B>(
    path: &Path,
    updates: Vec<(String, Option<Value>)>,
    after_read: A,
    before_publish: B,
) -> Result<(), String>
where
    A: FnOnce(),
    B: FnOnce(),
{
    transform_settings_with_before_publish(
        path,
        |map| {
            after_read();
            for (key, value) in updates {
                match value {
                    Some(value) => {
                        map.insert(key, value);
                    }
                    None => {
                        map.remove(&key);
                    }
                }
            }
            Ok(())
        },
        before_publish,
    )
}

/// Mutate the latest raw settings map while holding the shared path lock.
/// Revision checks belong inside `transform`, so another writer cannot pass
/// the same expected revision and overwrite a completed update.
pub fn transform_settings_with_before_publish<T, B>(
    path: &Path,
    transform: T,
    before_publish: B,
) -> Result<(), String>
where
    T: FnOnce(&mut Map<String, Value>) -> Result<(), String>,
    B: FnOnce(),
{
    let target = crate::global_config::resolve_write_target(path);
    let path_lock = settings_path_lock(&target);
    let _guard = path_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    // Share the permission writer's stable `.settings.json.lock` sibling.
    // Locking the settings inode would stop protecting it when atomic
    // publication replaces it; a different sibling would let the two writers
    // race and overwrite unrelated keys from their read-modify-write snapshots.
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(
        target
            .file_name()
            .ok_or("Settings path needs a file name")?,
    );
    lock_name.push(".lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(target.with_file_name(lock_name))
        .map_err(|error| {
            format!(
                "Failed to open settings lock for {}: {error}",
                target.display()
            )
        })?;
    fs2::FileExt::lock_exclusive(&lock_file)
        .map_err(|error| format!("Failed to lock settings {}: {error}", target.display()))?;
    let mut map = read_settings_map(&target)?;
    transform(&mut map)?;
    let serialized = serde_json::to_string_pretty(&Value::Object(map))
        .map_err(|e| format!("Failed to serialize settings for {}: {e}", target.display()))?;
    // NO explicit mode: the TS settings write (`settings.ts:500-503`) passes
    // no `mode` option, so new files get umask-default permissions (same as
    // the workspace's `commands/core/effort.rs` writer). Only the
    // global-config path uses 0o600. Trailing `\n` IS settings-specific
    // (`+ '\n'`, `settings.ts:502`).
    before_publish();
    write_settings_atomic(&target, (serialized + "\n").as_bytes())
        .map_err(|e| format!("Failed to write settings to {}: {e}", target.display()))
}

fn settings_path_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = normalized_lock_key(path);
    let locks = SETTINGS_PATH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = locks.lock().unwrap_or_else(|poison| poison.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    lock
}

fn normalized_lock_key(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if let (Ok(parent), Some(file_name)) = (std::fs::canonicalize(parent), path.file_name()) {
        return parent.join(file_name);
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

const TEMP_CREATE_RETRIES: usize = 3;

fn write_settings_atomic(target: &Path, bytes: &[u8]) -> io::Result<()> {
    write_settings_atomic_with(target, bytes, || Ok(()), rename_settings_temp)
}

fn rename_settings_temp(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    {
        let key = normalized_lock_key(to);
        let failures = FORCED_RENAME_FAILURES.get_or_init(|| Mutex::new(HashMap::new()));
        if failures
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(&key)
        {
            return Err(io::Error::other("injected settings rename failure"));
        }
    }
    std::fs::rename(from, to)
}

fn write_settings_atomic_with<B, R>(
    target: &Path,
    bytes: &[u8],
    before_write: B,
    rename: R,
) -> io::Result<()>
where
    B: FnOnce() -> io::Result<()>,
    R: FnOnce(&Path, &Path) -> io::Result<()>,
{
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let existing_mode = existing_mode(target)?;
    let (temp_path, mut temp_file) = create_exclusive_temp(target)?;
    let mut cleanup = TempCleanup::new(temp_path.clone());

    before_write()?;
    temp_file.write_all(bytes)?;
    temp_file.sync_all()?;
    apply_existing_mode(&temp_path, existing_mode)?;
    // Ensure chmod metadata is included in the durable staged file.
    temp_file.sync_all()?;
    drop(temp_file);

    match rename(&temp_path, target) {
        Ok(()) => {
            cleanup.disarm();
            sync_parent_if_supported(parent)
        }
        Err(rename_error) if eligible_for_in_place_fallback(&rename_error) => {
            // The upstream recovery arm keeps the complete staged contents
            // when its in-place fallback fails, so a caller can recover them.
            cleanup.disarm();
            write_settings_in_place(target, bytes, existing_mode)?;
            let _ = std::fs::remove_file(&temp_path);
            sync_parent_if_supported(parent)
        }
        Err(rename_error) => Err(rename_error),
    }
}

fn create_exclusive_temp(target: &Path) -> io::Result<(PathBuf, File)> {
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_default();
    let mut last_collision = None;
    for _ in 0..TEMP_CREATE_RETRIES {
        let random = uuid::Uuid::new_v4().simple().to_string();
        let path = parent.join(format!("{file_name}.tmp.{}", &random[..8]));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o666);
        }
        match options.open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
            }
            Err(error) => return Err(error),
        }
    }
    Err(last_collision.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate an exclusive settings staging file",
        )
    }))
}

#[cfg(unix)]
fn existing_mode(target: &Path) -> io::Result<Option<u32>> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(target) {
        Ok(metadata) => Ok(Some(metadata.permissions().mode() & 0o7777)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn existing_mode(_target: &Path) -> io::Result<Option<u32>> {
    Ok(None)
}

#[cfg(unix)]
fn apply_existing_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(mode) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_existing_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    Ok(())
}

fn eligible_for_in_place_fallback(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        // EXDEV, EPERM, EEXIST, EBUSY. Check raw errno values so EACCES,
        // which Rust also classifies as PermissionDenied, is not widened into
        // the upstream fallback set.
        matches!(error.raw_os_error(), Some(18 | 1 | 17 | 16))
    }
    #[cfg(windows)]
    {
        // ERROR_NOT_SAME_DEVICE, ERROR_SHARING_VIOLATION,
        // ERROR_FILE_EXISTS, ERROR_ALREADY_EXISTS. ERROR_ACCESS_DENIED maps to
        // EACCES in libuv and is intentionally excluded.
        return matches!(error.raw_os_error(), Some(17 | 32 | 80 | 183));
    }
    #[cfg(not(any(unix, windows)))]
    {
        error.kind() == io::ErrorKind::AlreadyExists
    }
}

fn write_settings_in_place(
    target: &Path,
    bytes: &[u8],
    existing_mode: Option<u32>,
) -> io::Result<()> {
    write_settings_in_place_with(target, bytes, existing_mode, |file, bytes| {
        file.write_all(bytes)
    })
}

fn write_settings_in_place_with<W>(
    target: &Path,
    bytes: &[u8],
    existing_mode: Option<u32>,
    write: W,
) -> io::Result<()>
where
    W: FnOnce(&mut File, &[u8]) -> io::Result<()>,
{
    let snapshot = TargetSnapshot::capture(target)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(existing_mode.unwrap_or(0o666));
    }
    let mut file = options.open(target)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "refusing the settings fallback on a non-regular target",
        ));
    }
    let result = (|| {
        file.set_len(0)?;
        write(&mut file, bytes)?;
        apply_existing_mode(target, existing_mode)?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = result {
        if let Err(restore_error) = snapshot.restore(target) {
            return Err(io::Error::new(
                error.kind(),
                format!("{error}; failed to restore original settings: {restore_error}"),
            ));
        }
        return Err(error);
    }
    Ok(())
}

enum TargetSnapshot {
    Absent,
    Present { bytes: Vec<u8>, mode: Option<u32> },
}

impl TargetSnapshot {
    fn capture(target: &Path) -> io::Result<Self> {
        match std::fs::read(target) {
            Ok(bytes) => Ok(Self::Present {
                bytes,
                mode: existing_mode(target)?,
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::Absent),
            Err(error) => Err(error),
        }
    }

    fn restore(self, target: &Path) -> io::Result<()> {
        match self {
            Self::Absent => match std::fs::remove_file(target) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
            Self::Present { bytes, mode } => {
                let mut options = OpenOptions::new();
                options.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(mode.unwrap_or(0o666));
                }
                let mut file = options.open(target)?;
                file.write_all(&bytes)?;
                apply_existing_mode(target, mode)?;
                file.sync_all()
            }
        }
    }
}

#[cfg(unix)]
fn sync_parent_if_supported(parent: &Path) -> io::Result<()> {
    match File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::Unsupported
                || matches!(error.raw_os_error(), Some(22 | 45 | 95)) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn sync_parent_if_supported(_parent: &Path) -> io::Result<()> {
    Ok(())
}

struct TempCleanup {
    path: PathBuf,
    armed: bool,
}

impl TempCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::temp_config;
    use std::sync::mpsc;
    use std::time::Duration;

    fn source_field(source: &Utf16JsonProjection, field: &str) -> Utf16JsonProjection {
        source
            .subprojection_property_units(&field.encode_utf16().collect::<Vec<_>>())
            .expect("fixture source field")
    }

    fn remove_secret_option_receipt(path: &Path, plugin: &str) -> SettingsUpdateReceipt {
        transform_settings_source_with_receipt(path, |source| {
            let mut configs = source_field(source, "pluginConfigs");
            let mut plugin_config = source_field(&configs, plugin);
            let mut options = source_field(&plugin_config, "options");
            options
                .remove_field("TOKEN")
                .map_err(|error| error.to_string())?;
            plugin_config
                .set_field("options", options)
                .map_err(|error| error.to_string())?;
            configs
                .set_field(plugin, plugin_config)
                .map_err(|error| error.to_string())?;
            source
                .set_field("pluginConfigs", configs)
                .map_err(|error| error.to_string())
        })
        .unwrap()
        .expect("secret option removal receipt")
    }

    #[test]
    fn paths_for_sources() {
        let t = temp_config();
        assert_eq!(
            settings_path(WritableScope::User, &t.home, &t.project),
            t.home.join("settings.json")
        );
        assert_eq!(
            settings_path(WritableScope::Local, &t.home, &t.project),
            t.project.join(".lingxi").join("settings.local.json")
        );
    }

    #[test]
    fn update_creates_file_and_merges_and_deletes() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        update_settings(
            &path,
            vec![("model".into(), Some(serde_json::json!("opus")))],
        )
        .unwrap();
        update_settings(&path, vec![("other".into(), Some(serde_json::json!(1)))]).unwrap();
        let map = read_settings_map(&path).unwrap();
        assert_eq!(map["model"], serde_json::json!("opus"));
        assert_eq!(map["other"], serde_json::json!(1));

        update_settings(&path, vec![("model".into(), None)]).unwrap();
        let map = read_settings_map(&path).unwrap();
        assert!(map.get("model").is_none());
        assert_eq!(map["other"], serde_json::json!(1));
    }

    #[test]
    fn settings_writer_shares_the_permission_writers_lock_and_reads_after_unlock() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(&t.home).unwrap();
        std::fs::write(&path, "{}").unwrap();
        let permission_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(t.home.join(".settings.json.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&permission_lock).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (read_tx, read_rx) = mpsc::channel();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            transform_settings_with_before_publish(
                &writer_path,
                |map| {
                    read_tx.send(()).unwrap();
                    map.insert("fusion".into(), serde_json::json!({"strategies":[]}));
                    Ok(())
                },
                || {},
            )
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(read_rx.recv_timeout(Duration::from_millis(100)).is_err());
        // The permission writer publishes while its lock is held. Fusion must
        // see and preserve this value after obtaining the same lock.
        std::fs::write(&path, r#"{"permissions":{"allow":["Read"]}}"#).unwrap();
        fs2::FileExt::unlock(&permission_lock).unwrap();
        read_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        writer.join().unwrap().unwrap();
        let map = read_settings_map(&path).unwrap();
        assert_eq!(map["permissions"], serde_json::json!({"allow":["Read"]}));
        assert_eq!(map["fusion"], serde_json::json!({"strategies":[]}));
    }

    #[test]
    fn update_bails_on_broken_json_without_overwriting() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ broken").unwrap();
        let res = update_settings(&path, vec![("x".into(), Some(serde_json::json!(1)))]);
        assert!(res.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ broken");
    }

    #[test]
    fn read_settings_map_missing_and_empty_are_empty() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        assert!(read_settings_map(&path).unwrap().is_empty());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "   \n").unwrap();
        assert!(read_settings_map(&path).unwrap().is_empty());
    }

    #[test]
    fn source_update_keeps_utf16_values_and_uses_the_existing_publish_seam() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"hooks":{"input":{"value":"\ud800"}},"opaque":{"value":"\udfff"}}"#,
        )
        .unwrap();

        let replacement = Utf16JsonProjection::parse(r#"{"input":{"value":"\ud800"}}"#)
            .expect("exact source replacement");
        let published = std::sync::atomic::AtomicBool::new(false);
        update_settings_source_with_before_publish(
            &path,
            vec![("hooks".into(), Some(replacement))],
            || published.store(true, std::sync::atomic::Ordering::SeqCst),
        )
        .unwrap();
        assert!(published.load(std::sync::atomic::Ordering::SeqCst));

        let source = read_settings_source(&path).unwrap();
        assert_eq!(
            source.string_units("/hooks/input/value"),
            Some(vec![0xD800])
        );
        assert_eq!(source.string_units("/opaque/value"), Some(vec![0xDFFF]));
        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(serialized.contains(r#""value": "\ud800""#));
        assert!(serialized.contains(r#""value": "\udfff""#));

        update_settings_source_with_before_publish(&path, vec![("hooks".into(), None)], || {})
            .unwrap();
        let source = read_settings_source(&path).unwrap();
        assert!(source.get_property("hooks").is_none());
        assert_eq!(source.string_units("/opaque/value"), Some(vec![0xDFFF]));
    }

    #[test]
    fn settings_receipt_restores_exact_bytes_when_uncontended() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b" {\"opaque\":{\"value\":\"\\ud800\"},\"enabledPlugins\":{}}\n";
        std::fs::write(&path, original).unwrap();

        let receipt = transform_settings_source_with_receipt(&path, |source| {
            let mut enabled = source_field(source, "enabledPlugins");
            enabled
                .set_field("secure@acme", Utf16JsonProjection::parse("true").unwrap())
                .map_err(|error| error.to_string())?;
            source
                .set_field("enabledPlugins", enabled)
                .map_err(|error| error.to_string())
        })
        .unwrap()
        .expect("mutation receipt");

        assert_ne!(std::fs::read(&path).unwrap(), original);
        assert!(rollback_settings_update(&receipt).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn settings_receipt_merge_preserves_interleaved_nested_writes() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"enabledPlugins":{"secure@acme":true,"other@acme":true},"pluginConfigs":{"secure@acme":{"options":{"TOKEN":"old-token","KEEP":"old-value"},"mcpServers":{"server":{"url":"old-url"}}},"other@acme":{"options":{"REGION":"old-region"}}},"opaque":{"value":"\ud800"}}"#,
        )
        .unwrap();

        let receipt = transform_settings_source_with_receipt(&path, |source| {
            let mut enabled = source_field(source, "enabledPlugins");
            enabled
                .remove_field("secure@acme")
                .map_err(|error| error.to_string())?;
            source
                .set_field("enabledPlugins", enabled)
                .map_err(|error| error.to_string())?;
            let mut configs = source_field(source, "pluginConfigs");
            configs
                .remove_field("secure@acme")
                .map_err(|error| error.to_string())?;
            source
                .set_field("pluginConfigs", configs)
                .map_err(|error| error.to_string())
        })
        .unwrap()
        .expect("uninstall settings receipt");

        // A separate settings writer commits while the plugin operation is
        // pending. It adds a new option under the same plugin and updates a
        // different plugin's settings after the uninstall's snapshot.
        let mut latest = read_settings_source(&path).unwrap();
        let mut enabled = source_field(&latest, "enabledPlugins");
        enabled
            .set_field("other@acme", Utf16JsonProjection::parse("false").unwrap())
            .unwrap();
        latest.set_field("enabledPlugins", enabled).unwrap();
        let mut configs = source_field(&latest, "pluginConfigs");
        let secure =
            Utf16JsonProjection::parse(r#"{"options":{"NEW_TOKEN":"concurrent-token"}}"#).unwrap();
        let mut other = source_field(&configs, "other@acme");
        let mut other_options = source_field(&other, "options");
        other_options
            .set_field(
                "REGION",
                Utf16JsonProjection::parse(r#""concurrent-region""#).unwrap(),
            )
            .unwrap();
        other.set_field("options", other_options).unwrap();
        configs.set_field("secure@acme", secure).unwrap();
        configs.set_field("other@acme", other).unwrap();
        latest.set_field("pluginConfigs", configs).unwrap();
        update_settings_source_with_before_publish(
            &path,
            vec![
                (
                    "enabledPlugins".to_string(),
                    Some(source_field(&latest, "enabledPlugins")),
                ),
                (
                    "pluginConfigs".to_string(),
                    Some(source_field(&latest, "pluginConfigs")),
                ),
            ],
            || {},
        )
        .unwrap();

        assert!(rollback_settings_update(&receipt).unwrap());
        let restored = read_settings_source(&path).unwrap();
        assert_eq!(
            restored
                .at_pointer("/enabledPlugins/secure@acme")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            restored
                .at_pointer("/enabledPlugins/other@acme")
                .unwrap()
                .as_bool(),
            Some(false)
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/secure@acme/options/TOKEN"),
            Some("old-token")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/secure@acme/options/KEEP"),
            Some("old-value")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/secure@acme/options/NEW_TOKEN"),
            Some("concurrent-token")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/secure@acme/mcpServers/server/url"),
            Some("old-url")
        );
        assert_eq!(
            restored.string_at("/pluginConfigs/other@acme/options/REGION"),
            Some("concurrent-region")
        );
        assert_eq!(restored.string_units("/opaque/value"), Some(vec![0xd800]));
    }

    #[test]
    fn settings_receipt_does_not_recreate_plugin_config_deleted_later() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"pluginConfigs":{"p":{"options":{"TOKEN":"old-secret","KEEP":"keep"}},"other":{"options":{"REGION":"stay"}}}}"#,
        )
        .unwrap();
        let receipt = remove_secret_option_receipt(&path, "p");

        let mut latest = read_settings_source(&path).unwrap();
        let mut configs = source_field(&latest, "pluginConfigs");
        configs.remove_field("p").unwrap();
        latest.set_field("pluginConfigs", configs).unwrap();
        update_settings_source_with_before_publish(
            &path,
            vec![(
                "pluginConfigs".to_string(),
                Some(source_field(&latest, "pluginConfigs")),
            )],
            || {},
        )
        .unwrap();

        let latest_bytes = std::fs::read(&path).unwrap();
        assert!(!rollback_settings_update(&receipt).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), latest_bytes);
        let restored = read_settings_source(&path).unwrap();
        assert!(restored.at_pointer("/pluginConfigs/p").is_none());
        assert_eq!(
            restored.string_at("/pluginConfigs/other/options/REGION"),
            Some("stay")
        );
    }

    #[test]
    fn settings_receipt_preserves_empty_plugin_config_recreated_after_removal() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"pluginConfigs":{"p":{"options":{"TOKEN":"old-secret","KEEP":"keep"}},"other":{"options":{"REGION":"stay"}}}}"#,
        )
        .unwrap();
        let receipt = transform_settings_source_with_receipt(&path, |source| {
            let mut configs = source_field(source, "pluginConfigs");
            configs
                .remove_field("p")
                .map_err(|error| error.to_string())?;
            source
                .set_field("pluginConfigs", configs)
                .map_err(|error| error.to_string())
        })
        .unwrap()
        .expect("whole plugin configuration removal receipt");

        let mut latest = read_settings_source(&path).unwrap();
        let mut configs = source_field(&latest, "pluginConfigs");
        configs
            .set_field("p", Utf16JsonProjection::object(Vec::new()).unwrap())
            .unwrap();
        latest.set_field("pluginConfigs", configs).unwrap();
        update_settings_source_with_before_publish(
            &path,
            vec![(
                "pluginConfigs".to_string(),
                Some(source_field(&latest, "pluginConfigs")),
            )],
            || {},
        )
        .unwrap();

        let latest_bytes = std::fs::read(&path).unwrap();
        assert!(!rollback_settings_update(&receipt).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), latest_bytes);
        let restored = read_settings_source(&path).unwrap();
        assert_eq!(
            restored
                .subprojection("/pluginConfigs/p")
                .unwrap()
                .object_entries()
                .unwrap()
                .len(),
            0
        );
        assert!(restored
            .at_pointer("/pluginConfigs/p/options/TOKEN")
            .is_none());
        assert_eq!(
            restored.string_at("/pluginConfigs/other/options/REGION"),
            Some("stay")
        );
    }

    #[test]
    fn settings_receipt_does_not_recreate_deleted_or_cleared_options() {
        for clear_to_empty_object in [false, true] {
            let t = temp_config();
            let path = settings_path(WritableScope::User, &t.home, &t.project);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                r#"{"pluginConfigs":{"p":{"options":{"TOKEN":"old-secret","KEEP":"keep"},"mcpServers":{"server":{"url":"stay"}}}}}"#,
            )
            .unwrap();
            let receipt = remove_secret_option_receipt(&path, "p");

            let mut latest = read_settings_source(&path).unwrap();
            let mut configs = source_field(&latest, "pluginConfigs");
            let mut plugin_config = source_field(&configs, "p");
            if clear_to_empty_object {
                plugin_config
                    .set_field("options", Utf16JsonProjection::object(Vec::new()).unwrap())
                    .unwrap();
            } else {
                plugin_config.remove_field("options").unwrap();
            }
            configs.set_field("p", plugin_config).unwrap();
            latest.set_field("pluginConfigs", configs).unwrap();
            update_settings_source_with_before_publish(
                &path,
                vec![(
                    "pluginConfigs".to_string(),
                    Some(source_field(&latest, "pluginConfigs")),
                )],
                || {},
            )
            .unwrap();

            let latest_bytes = std::fs::read(&path).unwrap();
            assert!(!rollback_settings_update(&receipt).unwrap());
            assert_eq!(std::fs::read(&path).unwrap(), latest_bytes);
            let restored = read_settings_source(&path).unwrap();
            assert!(restored
                .at_pointer("/pluginConfigs/p/options/TOKEN")
                .is_none());
            assert_eq!(
                restored.string_at("/pluginConfigs/p/mcpServers/server/url"),
                Some("stay")
            );
            if clear_to_empty_object {
                assert_eq!(
                    restored
                        .subprojection("/pluginConfigs/p/options")
                        .unwrap()
                        .object_entries()
                        .unwrap()
                        .len(),
                    0
                );
            } else {
                assert!(restored.at_pointer("/pluginConfigs/p/options").is_none());
            }
        }
    }

    #[test]
    fn typed_update_preserves_existing_exact_source_values() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"hooks":{"input":{"value":"\ud800"}},"opaque":{"value":"\udfff"}}"#,
        )
        .unwrap();

        update_settings(
            &path,
            vec![("model".into(), Some(Value::String("opus".into())))],
        )
        .unwrap();

        let source = read_settings_source(&path).unwrap();
        assert_eq!(source.string_at("/model"), Some("opus"));
        assert_eq!(
            source.string_units("/hooks/input/value"),
            Some(vec![0xD800])
        );
        assert_eq!(source.string_units("/opaque/value"), Some(vec![0xDFFF]));
    }

    #[test]
    fn project_source_resolves_to_project_settings_json() {
        let home = std::path::Path::new("/home/u/.lingxi");
        let project = std::path::Path::new("/work/repo");
        let path = settings_path(WritableScope::Project, home, project);
        assert_eq!(
            path,
            std::path::Path::new("/work/repo")
                .join(branding::DOT_DIR)
                .join("settings.json"),
            "Project source must resolve to <project>/<DOT_DIR>/settings.json, got {}",
            path.display()
        );
    }

    #[test]
    fn concurrent_updates_to_one_path_preserve_both_keys() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        update_settings(
            &path,
            vec![("original".into(), Some(serde_json::json!(true)))],
        )
        .unwrap();

        let (first_read_tx, first_read_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_path = path.clone();
        let first = std::thread::spawn(move || {
            update_settings_after_read(
                &first_path,
                vec![("first".into(), Some(serde_json::json!(1)))],
                move || {
                    first_read_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
            )
        });
        first_read_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (second_started_tx, second_started_rx) = mpsc::channel();
        let (second_read_tx, second_read_rx) = mpsc::channel();
        let second_path = path.clone();
        let second = std::thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            update_settings_after_read(
                &second_path,
                vec![("second".into(), Some(serde_json::json!(2)))],
                move || second_read_tx.send(()).unwrap(),
            )
        });
        second_started_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(
            second_read_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "the second update reached its read while the first RMW held the path lock"
        );
        release_tx.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();

        let map = read_settings_map(&path).unwrap();
        assert_eq!(map["original"], serde_json::json!(true));
        assert_eq!(map["first"], serde_json::json!(1));
        assert_eq!(map["second"], serde_json::json!(2));
    }

    #[test]
    fn staging_write_failure_preserves_source_and_cleans_temp() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old bytes\n").unwrap();
        let error = write_settings_atomic_with(
            &path,
            b"new bytes\n",
            || {
                Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "injected write failure",
                ))
            },
            |from, to| std::fs::rename(from, to),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(std::fs::read(&path).unwrap(), b"old bytes\n");
        assert!(settings_temps(&path).is_empty());
    }

    #[test]
    fn non_fallback_rename_failure_preserves_source_and_cleans_temp() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old bytes\n").unwrap();
        let error = write_settings_atomic_with(
            &path,
            b"new bytes\n",
            || Ok(()),
            |_, _| Err(io::Error::other("injected rename failure")),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(std::fs::read(&path).unwrap(), b"old bytes\n");
        assert!(settings_temps(&path).is_empty());
    }

    #[test]
    fn eligible_rename_failure_uses_bounded_in_place_fallback() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old bytes\n").unwrap();
        write_settings_atomic_with(
            &path,
            b"new bytes\n",
            || Ok(()),
            |_, _| Err(fallback_eligible_rename_error()),
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new bytes\n");
        assert!(settings_temps(&path).is_empty());
    }

    #[test]
    fn failed_in_place_fallback_restores_source() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"old bytes\n").unwrap();
        let error = write_settings_in_place_with(&path, b"new bytes\n", None, |file, _| {
            file.write_all(b"partial")?;
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "injected fallback write failure",
            ))
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WriteZero);
        assert_eq!(std::fs::read(&path).unwrap(), b"old bytes\n");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_update_preserves_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{}\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        update_settings(&path, vec![("mode".into(), Some(serde_json::json!(true)))]).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn serialized_settings_have_one_trailing_newline() {
        let t = temp_config();
        let path = settings_path(WritableScope::User, &t.home, &t.project);
        update_settings(&path, vec![("x".into(), Some(serde_json::json!(1)))]).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(!bytes.ends_with(b"\n\n"));
    }

    fn settings_temps(path: &Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}.tmp.",
            path.file_name()
                .map(|name| name.to_string_lossy())
                .unwrap_or_default()
        );
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect()
    }

    #[cfg(unix)]
    fn fallback_eligible_rename_error() -> io::Error {
        io::Error::from_raw_os_error(18)
    }

    #[cfg(windows)]
    fn fallback_eligible_rename_error() -> io::Error {
        io::Error::from_raw_os_error(17)
    }

    #[cfg(not(any(unix, windows)))]
    fn fallback_eligible_rename_error() -> io::Error {
        io::Error::new(io::ErrorKind::AlreadyExists, "injected rename failure")
    }
}
