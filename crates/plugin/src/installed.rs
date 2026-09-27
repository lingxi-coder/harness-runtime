//! Path to the durable record of installed plugins
//! (`<plugins>/installed_plugins.json`).
//!
//! The production CLI writes the canonical V2 shape
//! (`{ version: 2, plugins: { "<plugin>@<marketplace>":
//! [{scope, installPath, version, installedAt, lastUpdated}] } }`) to
//! `installed_plugins.json`. Oracle 2.1.252 still carries a once-per-session
//! migration seam for older durable state:
//!
//! - a stale `installed_plugins_v2.json` filename may still exist;
//! - the durable payload may still use the V1
//!   `{ plugins: { "<plugin>@<marketplace>": {version, installedAt, ...} } }`
//!   shape;
//! - older LingXi builds also accepted the nested
//!   `{ plugins: { <marketplace>: { <plugin>: {version, added} } } }` shape,
//!   which remains readable during normalization.
//!
//! This module keeps discovery and CLI mutation paths aligned across those
//! variants by:
//!
//! 1. serializing access on `.installed_plugins.lock`;
//! 2. reading either filename;
//! 3. normalizing any legacy/mixed payload to canonical V2;
//! 4. durably persisting that normalization to `installed_plugins.json`;
//! 5. restoring both filenames exactly on transaction rollback.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use platform_api::rooted_fs::{RootedFileLock, PRIVATE_DIR_MODE, PRIVATE_FILE_MODE};
use serde_json::{Map, Value};

const CURRENT_FILE: &str = "installed_plugins.json";
const LEGACY_FILE: &str = "installed_plugins_v2.json";
const LOCK_FILE: &str = ".installed_plugins.lock";
static PROCESS_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug)]
enum FileSnapshot {
    Missing,
    File(Vec<u8>),
    Other,
}

#[derive(Debug)]
struct RegistrySnapshot {
    current: FileSnapshot,
    legacy: FileSnapshot,
}

struct InstalledRegistryLock {
    _process_lock: MutexGuard<'static, ()>,
    _file_lock: RootedFileLock,
}

/// A locked installed-plugin registry transaction.
///
/// This captures the pre-transaction durable state so callers can restore it if
/// later settings or cache updates fail after the registry was rewritten.
pub struct InstalledRegistryTransaction {
    install_dir: PathBuf,
    snapshot: RegistrySnapshot,
    doc: Value,
    _lock: InstalledRegistryLock,
}

/// Path to `installed_plugins.json` under the plugins root.
#[must_use]
pub fn path(install_dir: &Path) -> PathBuf {
    install_dir.join(CURRENT_FILE)
}

/// Read installed-plugin state from the current or legacy filename, normalize
/// any V1/mixed payload to canonical V2, and best-effort persist the normalized
/// bytes back to `installed_plugins.json`.
#[must_use]
pub fn load_normalized(install_dir: &Path) -> Option<Value> {
    let lock = acquire_lock(install_dir, false).ok()??;
    let snapshot = RegistrySnapshot::capture(install_dir).ok()?;
    let loaded = load_normalized_locked(install_dir, &snapshot);
    drop(lock);
    match loaded {
        Ok(doc) => doc,
        Err(error) => {
            tracing::warn!(%error, "failed to load installed plugin registry");
            None
        }
    }
}

impl InstalledRegistryTransaction {
    /// Begin a locked registry transaction, creating the plugins directory if
    /// needed. Missing or malformed state yields an empty canonical V2 doc.
    pub fn begin(install_dir: &Path) -> Result<Self, String> {
        let lock = acquire_lock(install_dir, true)?.ok_or_else(|| {
            format!(
                "failed to lock installed plugin registry at {}",
                install_dir.display()
            )
        })?;
        let snapshot = RegistrySnapshot::capture(install_dir)?;
        let doc =
            load_normalized_locked(install_dir, &snapshot)?.unwrap_or_else(empty_installed_doc);
        Ok(Self {
            install_dir: install_dir.to_path_buf(),
            snapshot,
            doc,
            _lock: lock,
        })
    }

    /// The current canonical V2 document.
    #[must_use]
    pub fn document(&self) -> &Value {
        &self.doc
    }

    /// Mutable access to the current canonical V2 document.
    pub fn document_mut(&mut self) -> &mut Value {
        &mut self.doc
    }

    /// Persist the current in-memory document to `installed_plugins.json`.
    pub fn persist(&self) -> Result<(), String> {
        persist_normalized(&self.install_dir, &self.doc, true)
    }

    /// Restore both durable registry filenames to their exact pre-transaction
    /// state.
    pub fn restore_previous(&self) -> Result<(), String> {
        self.snapshot
            .current
            .restore(&self.install_dir, CURRENT_FILE)?;
        self.snapshot
            .legacy
            .restore(&self.install_dir, LEGACY_FILE)?;
        Ok(())
    }
}

fn acquire_lock(
    install_dir: &Path,
    create_dir: bool,
) -> Result<Option<InstalledRegistryLock>, String> {
    if create_dir {
        std::fs::create_dir_all(install_dir).map_err(|error| error.to_string())?;
    } else if !std::fs::symlink_metadata(install_dir)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false)
    {
        return Ok(None);
    }

    let process_lock = PROCESS_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "installed plugin registry process lock poisoned".to_string())?;
    let file_lock = platform_api::rooted_fs::lock_exclusive(
        install_dir,
        Path::new(LOCK_FILE),
        PRIVATE_DIR_MODE,
        PRIVATE_FILE_MODE,
    )
    .map_err(|error| error.to_string())?;

    Ok(Some(InstalledRegistryLock {
        _process_lock: process_lock,
        _file_lock: file_lock,
    }))
}

fn load_normalized_locked(
    install_dir: &Path,
    snapshot: &RegistrySnapshot,
) -> Result<Option<Value>, String> {
    if let Some((doc, needs_persist)) = snapshot.current.normalized(install_dir) {
        if needs_persist {
            persist_normalized(install_dir, &doc, true)?;
        }
        return Ok(Some(doc));
    }

    let Some((doc, _)) = snapshot.legacy.normalized(install_dir) else {
        return Ok(None);
    };

    let overwrite_current = snapshot.current.exists();
    match persist_normalized(install_dir, &doc, overwrite_current) {
        Ok(()) => {
            if !overwrite_current {
                remove_legacy_file(install_dir)?;
            }
        }
        Err(error) => {
            tracing::warn!(%error, "failed to persist migrated installed plugin registry");
        }
    }
    Ok(Some(doc))
}

fn persist_normalized(install_dir: &Path, doc: &Value, overwrite: bool) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(doc).map_err(|error| error.to_string())?;
    let mut options = platform_api::AtomicWriteOptions::default();
    options.overwrite = overwrite;
    platform_api::rooted_fs::atomic_write(
        install_dir,
        Path::new(CURRENT_FILE),
        serialized.as_bytes(),
        options,
    )
    .map_err(|error| error.to_string())
}

fn remove_legacy_file(install_dir: &Path) -> Result<(), String> {
    match platform_api::rooted_fs::remove_file(install_dir, Path::new(LEGACY_FILE)) {
        Ok(()) => Ok(()),
        Err(platform_api::FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn empty_installed_doc() -> Value {
    serde_json::json!({"version": 2, "plugins": {}})
}

fn read_relative_bytes(install_dir: &Path, relative: &str) -> Result<FileSnapshot, String> {
    let full_path = platform_api::rooted_fs::checked_join(install_dir, Path::new(relative))
        .map_err(|error| error.to_string())?;
    let metadata = match std::fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(FileSnapshot::Missing);
        }
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.file_type().is_file() {
        return Ok(FileSnapshot::Other);
    }
    let bytes = std::fs::read(full_path).map_err(|error| error.to_string())?;
    Ok(FileSnapshot::File(bytes))
}

fn restore_relative_bytes(install_dir: &Path, relative: &str, bytes: &[u8]) -> Result<(), String> {
    let mut options = platform_api::AtomicWriteOptions::default();
    options.overwrite = true;
    platform_api::rooted_fs::atomic_write(install_dir, Path::new(relative), bytes, options)
        .map_err(|error| error.to_string())
}

fn remove_relative_file(install_dir: &Path, relative: &str) -> Result<(), String> {
    match platform_api::rooted_fs::remove_file(install_dir, Path::new(relative)) {
        Ok(()) => Ok(()),
        Err(platform_api::FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn normalize_value(install_dir: &Path, value: Value) -> Option<(Value, bool)> {
    let plugins = value.get("plugins")?.as_object()?;
    let mut normalized_plugins = Map::new();
    let mut changed = value.get("version").and_then(Value::as_u64) != Some(2);

    for (key, entry) in plugins {
        match entry {
            Value::Array(records) => {
                let mut normalized_records = Vec::with_capacity(records.len());
                for record in records {
                    let Some(record) = record.as_object() else {
                        changed = true;
                        continue;
                    };
                    normalized_records.push(Value::Object(record.clone()));
                }
                normalized_plugins.insert(key.clone(), Value::Array(normalized_records));
            }
            Value::Object(record_or_plugins) => {
                changed = true;
                if is_flat_v1_record(record_or_plugins) {
                    push_normalized_record(
                        &mut normalized_plugins,
                        key,
                        normalize_flat_v1_record(install_dir, key, record_or_plugins),
                    );
                } else {
                    for (name, record) in record_or_plugins {
                        let Some(record) = record.as_object() else {
                            continue;
                        };
                        let identifier = format!("{name}@{key}");
                        push_normalized_record(
                            &mut normalized_plugins,
                            &identifier,
                            normalize_nested_legacy_record(record.clone()),
                        );
                    }
                }
            }
            _ => {
                changed = true;
            }
        }
    }

    Some((
        Value::Object(Map::from_iter([
            ("version".to_string(), Value::from(2_u64)),
            ("plugins".to_string(), Value::Object(normalized_plugins)),
        ])),
        changed,
    ))
}

fn is_flat_v1_record(record: &Map<String, Value>) -> bool {
    const RECORD_FIELDS: [&str; 8] = [
        "version",
        "installPath",
        "scope",
        "installedAt",
        "lastUpdated",
        "gitCommitSha",
        "added",
        "projectPath",
    ];

    RECORD_FIELDS.iter().any(|field| {
        record
            .get(*field)
            .is_some_and(|value| !value.is_object() && !value.is_array())
    })
}

fn normalize_flat_v1_record(
    install_dir: &Path,
    identifier: &str,
    record: &Map<String, Value>,
) -> Map<String, Value> {
    let mut normalized = Map::new();
    normalized.insert("scope".to_string(), Value::String("user".to_string()));

    let version = record.get("version").cloned();
    let derived_install_path = version
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|version| flat_v1_cache_path(install_dir, identifier, version));
    if let Some(install_path) = derived_install_path {
        normalized.insert("installPath".to_string(), Value::String(install_path));
    } else if let Some(install_path) = record.get("installPath") {
        normalized.insert("installPath".to_string(), install_path.clone());
    }

    if let Some(version) = version {
        normalized.insert("version".to_string(), version);
    }
    for field in ["installedAt", "lastUpdated", "gitCommitSha"] {
        if let Some(value) = record.get(field) {
            normalized.insert(field.to_string(), value.clone());
        }
    }
    if let Some(added) = record.get("added") {
        normalized
            .entry("installedAt".to_string())
            .or_insert_with(|| added.clone());
        normalized
            .entry("lastUpdated".to_string())
            .or_insert_with(|| added.clone());
    }
    normalized
}

fn flat_v1_cache_path(install_dir: &Path, identifier: &str, version: &str) -> Option<String> {
    let (name, marketplace) = identifier.split_once('@')?;
    let marketplace = marketplace.split('@').next().unwrap_or(marketplace);
    if name.is_empty() || marketplace.is_empty() || version.is_empty() {
        return None;
    }
    Some(
        install_dir
            .join("cache")
            .join(crate::discovery::sanitize_segment(marketplace, false))
            .join(crate::discovery::sanitize_segment(name, false))
            .join(crate::discovery::sanitize_segment(version, true))
            .display()
            .to_string(),
    )
}

fn push_normalized_record(
    normalized_plugins: &mut Map<String, Value>,
    identifier: &str,
    record: Map<String, Value>,
) {
    if let Some(Value::Array(existing)) = normalized_plugins.get_mut(identifier) {
        existing.push(Value::Object(record));
    } else {
        normalized_plugins.insert(
            identifier.to_string(),
            Value::Array(vec![Value::Object(record)]),
        );
    }
}

fn normalize_nested_legacy_record(mut record: Map<String, Value>) -> Map<String, Value> {
    if !record.contains_key("scope") {
        record.insert("scope".to_string(), Value::String("user".to_string()));
    }
    if let Some(added) = record.remove("added") {
        if !record.contains_key("installedAt") {
            record.insert("installedAt".to_string(), added.clone());
        }
        if !record.contains_key("lastUpdated") {
            record.insert("lastUpdated".to_string(), added);
        }
    }
    record
}

impl RegistrySnapshot {
    fn capture(install_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            current: read_relative_bytes(install_dir, CURRENT_FILE)?,
            legacy: read_relative_bytes(install_dir, LEGACY_FILE)?,
        })
    }
}

impl FileSnapshot {
    fn exists(&self) -> bool {
        !matches!(self, Self::Missing)
    }

    fn normalized(&self, install_dir: &Path) -> Option<(Value, bool)> {
        let Self::File(bytes) = self else {
            return None;
        };
        normalize_value(install_dir, serde_json::from_slice::<Value>(bytes).ok()?)
    }

    fn restore(&self, install_dir: &Path, relative: &str) -> Result<(), String> {
        match self {
            Self::Missing => remove_relative_file(install_dir, relative),
            Self::File(bytes) => restore_relative_bytes(install_dir, relative, bytes),
            Self::Other => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "installed_tests.rs"]
mod tests;
