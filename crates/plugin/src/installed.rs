//! The current durable installed-plugin registry is
//! `<plugins>/installed_plugins.json` in V2 form:
//! `{ version: 2, plugins: { "<plugin>@<marketplace>": [records] } }`.
//!
//! Reads and writes share `.installed_plugins.lock`. Registry transactions
//! retain the exact current-file bytes for rollback; unrelated or obsolete
//! filenames are left untouched.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use lingxi_core::host::rooted_fs::{RootedFileLock, PRIVATE_DIR_MODE, PRIVATE_FILE_MODE};
use serde_json::Value;

const CURRENT_FILE: &str = "installed_plugins.json";
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

/// Read the current V2 installed-plugin registry.
///
/// A missing or non-V2 registry returns `None`. The read path does not rename,
/// migrate, or remove other files.
#[must_use]
pub fn load_normalized(install_dir: &Path) -> Option<Value> {
    let lock = acquire_lock(install_dir, false).ok()??;
    let snapshot = RegistrySnapshot::capture(install_dir).ok()?;
    let loaded = load_current_locked(&snapshot);
    drop(lock);
    loaded
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
        let doc = load_current_locked(&snapshot).unwrap_or_else(empty_installed_doc);
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

    /// Restore the current registry file to its exact pre-transaction state.
    pub fn restore_previous(&self) -> Result<(), String> {
        self.snapshot
            .current
            .restore(&self.install_dir, CURRENT_FILE)?;
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
    let file_lock = lingxi_core::host::rooted_fs::lock_exclusive(
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

fn load_current_locked(snapshot: &RegistrySnapshot) -> Option<Value> {
    snapshot.current.current_v2()
}

fn persist_normalized(install_dir: &Path, doc: &Value, overwrite: bool) -> Result<(), String> {
    let serialized = serde_json::to_string_pretty(doc).map_err(|error| error.to_string())?;
    let mut options = lingxi_core::host::AtomicWriteOptions::default();
    options.overwrite = overwrite;
    lingxi_core::host::rooted_fs::atomic_write(
        install_dir,
        Path::new(CURRENT_FILE),
        serialized.as_bytes(),
        options,
    )
    .map_err(|error| error.to_string())
}

fn empty_installed_doc() -> Value {
    serde_json::json!({"version": 2, "plugins": {}})
}

fn read_relative_bytes(install_dir: &Path, relative: &str) -> Result<FileSnapshot, String> {
    let full_path = lingxi_core::host::rooted_fs::checked_join(install_dir, Path::new(relative))
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
    let mut options = lingxi_core::host::AtomicWriteOptions::default();
    options.overwrite = true;
    lingxi_core::host::rooted_fs::atomic_write(install_dir, Path::new(relative), bytes, options)
        .map_err(|error| error.to_string())
}

fn remove_relative_file(install_dir: &Path, relative: &str) -> Result<(), String> {
    match lingxi_core::host::rooted_fs::remove_file(install_dir, Path::new(relative)) {
        Ok(()) => Ok(()),
        Err(lingxi_core::host::FsError::NotFound(_)) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_current_v2(value: Value) -> Option<Value> {
    if value.get("version").and_then(Value::as_u64) != Some(2) {
        return None;
    }
    let plugins = value.get("plugins")?.as_object()?;
    for records in plugins.values() {
        let records = records.as_array()?;
        if records.iter().any(|record| !record.is_object()) {
            return None;
        }
    }
    Some(value)
}

impl RegistrySnapshot {
    fn capture(install_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            current: read_relative_bytes(install_dir, CURRENT_FILE)?,
        })
    }
}

impl FileSnapshot {
    fn current_v2(&self) -> Option<Value> {
        let Self::File(bytes) = self else {
            return None;
        };
        validate_current_v2(serde_json::from_slice::<Value>(bytes).ok()?)
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
