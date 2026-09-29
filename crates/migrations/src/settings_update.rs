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

/// `updateSettingsForSource`: apply top-level key updates. `Some(v)` sets the
/// key, `None` deletes it (TS `mergeWith` treats `undefined` as delete).
/// All other keys preserved; pretty-printed + trailing newline.
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
    update_settings_with_hooks(path, updates, || {}, before_publish)
}

/// A process-local lock per resolved settings path, matching the upstream
/// promise queue. It deliberately does not claim cross-process exclusion.
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
    let target = crate::global_config::resolve_write_target(path);
    let path_lock = settings_path_lock(&target);
    let _guard = path_lock
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    }
    let mut map = read_settings_map(&target)?;
    after_read();
    for (key, value) in updates {
        match value {
            Some(v) => {
                map.insert(key, v);
            }
            None => {
                map.remove(&key);
            }
        }
    }
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
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "injected settings rename failure",
            ));
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
            |_, _| {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected rename failure",
                ))
            },
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
