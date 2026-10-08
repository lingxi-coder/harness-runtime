//! Settings-file watcher → `ConfigChange` hook fire (parity: claude-code
//! `src/utils/settings/changeDetector.ts`).
//!
//! claude-code watches the settings files (user / project / local / policy)
//! and, on every detected change, fires the `ConfigChange` hook with the layer
//! `source` and the changed `file_path` BEFORE applying the change to the live
//! session (`changeDetector.ts:285-297` → `executeConfigChangeHooks`,
//! `utils/hooks.ts:4214`). The Rust port had no such watcher; this module adds
//! it at the desktop composition root, where the orchestrator (`orch.hooks`),
//! the `cwd`, the `lingxi_home`, and a `FileSystem` are all in scope.
//!
//! ## What this does (and does NOT do)
//! The watcher fires the hook first, then applies the narrow managed
//! `disableAutoMode` safety update to an attached live permission gate. It does
//! not reload ordinary user/project/local settings or rebuild the full policy;
//! those remain composition-root concerns. It watches the relevant `.claude`
//! (and managed) directories via the in-tree `fs_watch` primitive
//! ([`lingxi_core::host::FileSystem::watch`]), classifies each changed path to a
//! [`ConfigChangeSource`] layer, and fires
//! [`ConversationOrchestrator::fire_config_change`] best-effort.
//!
//! ## `fs_watch` reuse
//! The watcher is generic over `Arc<dyn FileSystem>` and drives it ONLY through
//! the trait's `watch(dir)` method — the in-tree `notify`-backed primitive
//! (`platforms/posix::watch_helper::watch_dir_with_debounce`, exposed via
//! `PosixFileSystem::watch`). No new external dependency is introduced; the
//! composition root injects whichever `FileSystem` it built.
//!
//! ## Path → source mapping (byte-faithful)
//! Mirrors `getSourceForPath` (`changeDetector.ts:361-375`) +
//! `getSettingsFilePathForSource` (`settings.ts:274-294`) +
//! `getManagedFilePath` (`managedPath.ts`):
//! - `<lingxi_home>/settings.json`            → `UserSettings`
//! - `<cwd>/.lingxi/settings.json`            → `ProjectSettings`
//! - `<cwd>/.lingxi/settings.local.json`      → `LocalSettings`
//! - `<managed_dir>/managed-settings.json`    → `PolicySettings`
//! - any `*.json` under `<managed_dir>/managed-settings.d/` → `PolicySettings`
//!
//! where `<managed_dir>` is the OS-specific managed root
//! (`/Library/Application Support/LingXi` on macOS,
//! `C:\Program Files\LingXi` on Windows, `/etc/lingxi` elsewhere).
//!
//! ## Lifecycle
//! [`SettingsWatcher::spawn`] starts one background task per watched directory
//! and returns a [`SettingsWatcherHandle`]. Dropping the handle aborts every
//! task (RAII) and the underlying `notify` watcher is released when the
//! [`lingxi_core::host::FileSystem::watch`] stream is dropped — a clean teardown with no
//! lingering OS handles. The task is started even when no ConfigChange hook is
//! configured so the managed safety callback cannot be disabled by hook setup.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use futures_core::Stream;
use hooks::events::ConfigChangeSource;
use lingxi_core::host::{FileEvent, FileSystem, PermissionGate};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;

/// Narrow fire seam: the watcher fires a `ConfigChange` without depending on
/// the full orchestrator surface. The composition root injects the live
/// orchestrator (whose blanket impl below forwards to
/// [`ConversationOrchestrator::fire_config_change`]); unit tests inject a
/// recording fake so the watcher logic is exercised without a real
/// orchestrator or any real filesystem timing.
#[async_trait]
pub trait ConfigChangeFirer: Send + Sync {
    /// Fire the `ConfigChange` hook for a changed settings path. Best-effort:
    /// implementors MUST NOT propagate failures (a failing hook never breaks
    /// the watcher loop).
    async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>);
}

#[async_trait]
impl ConfigChangeFirer for orchestrator::ConversationOrchestrator {
    async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>) {
        orchestrator::ConversationOrchestrator::fire_config_change(self, source, file_path).await;
    }
}

/// Env override that relocates the managed (policy) settings root. The real
/// managed path is an absolute, OS-protected directory that tests cannot write;
/// pointing this at a tempdir lets the managed-tier derivation tests exercise
/// the loader + precedence fold without root. Unset in production, where the
/// hardcoded OS path is used (faithful to claude-code's `getManagedFilePath`).
pub const MANAGED_DIR_ENV: &str = "LINGXI_MANAGED_DIR";

/// The OS-specific managed (policy) settings root, mirroring claude-code's
/// `getManagedFilePath` (`managedPath.ts`). Honors the [`MANAGED_DIR_ENV`]
/// override first (test-only relocation); otherwise returns the hardcoded
/// OS-specific path.
#[must_use]
pub fn managed_settings_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(MANAGED_DIR_ENV).filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "macos") {
        PathBuf::from(branding::MANAGED_DIR_MACOS)
    } else if cfg!(target_os = "windows") {
        PathBuf::from(branding::MANAGED_DIR_WINDOWS)
    } else {
        PathBuf::from(branding::MANAGED_DIR_UNIX)
    }
}

/// Read the file-based managed (policy) settings raw JSON tiers, in ASCENDING
/// merge priority, faithful to claude-code `loadManagedFileSettings`
/// (settings.ts:75-122): `managed-settings.json` first (base), then every
/// `*.json` under `managed-settings.d/` sorted alphabetically (drop-ins win,
/// later files override). Skips dotfiles. Best-effort: unreadable/absent files
/// and a missing drop-in dir are silently skipped (TS swallows ENOENT/ENOTDIR).
/// Returned strings are appended by the caller AFTER the user/project/local
/// (and flag, if any) tiers so policy wins.
#[must_use]
pub async fn managed_settings_raw_tiers() -> Vec<String> {
    read_managed_settings_snapshot(&managed_settings_dir())
        .await
        .raw_tiers
}

/// The model-policy gate also needs failures that a best-effort settings
/// consumer can skip. Missing files/directories remain an absent policy.
#[derive(Debug, Default)]
pub(super) struct ManagedSettingsSnapshot {
    pub raw_tiers: Vec<String>,
    pub read_failed: bool,
}

pub(super) async fn managed_settings_snapshot() -> ManagedSettingsSnapshot {
    read_managed_settings_snapshot(&managed_settings_dir()).await
}

pub(super) async fn read_managed_settings_snapshot(managed: &Path) -> ManagedSettingsSnapshot {
    use std::io::ErrorKind;
    let absent = |error: &std::io::Error| {
        matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory)
    };
    let mut snapshot = ManagedSettingsSnapshot::default();
    match tokio::fs::read_to_string(managed.join("managed-settings.json")).await {
        Ok(raw) => snapshot.raw_tiers.push(raw),
        Err(error) => snapshot.read_failed |= !absent(&error),
    }
    let drop_in = managed.join("managed-settings.d");
    match tokio::fs::read_dir(&drop_in).await {
        Ok(mut rd) => {
            let mut names: Vec<std::ffi::OsString> = Vec::new();
            loop {
                match rd.next_entry().await {
                    Ok(Some(entry)) => {
                        let name = entry.file_name();
                        let n = name.to_string_lossy();
                        if n.ends_with(".json") && !n.starts_with('.') {
                            names.push(name);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        snapshot.read_failed = true;
                        break;
                    }
                }
            }
            names.sort();
            for name in names {
                match tokio::fs::read_to_string(drop_in.join(name)).await {
                    Ok(raw) => snapshot.raw_tiers.push(raw),
                    Err(error) => snapshot.read_failed |= !absent(&error),
                }
            }
        }
        Err(error) => snapshot.read_failed |= !absent(&error),
    }
    snapshot
}

/// Fold the currently loaded managed policy tiers into the one live setting
/// owned by the permission gate. Managed tiers are already in ascending
/// precedence; `disableAutoMode` is a sticky admin restriction, so any tier
/// that says `"disable"` closes the gate.
#[must_use]
pub fn auto_mode_disabled_from_managed_tiers(tiers: &[String]) -> bool {
    tiers
        .iter()
        .any(|raw| permission::auto_mode_disabled_from_settings_json(raw))
}

/// The set of settings paths the watcher cares about, resolved from the
/// composition root's `lingxi_home` + `cwd`. Carries both the absolute file
/// paths and the parent directories to watch.
#[derive(Debug, Clone)]
pub struct SettingsPaths {
    /// `<lingxi_home>/settings.json`.
    pub user_settings: PathBuf,
    /// `<cwd>/.lingxi/settings.json`.
    pub project_settings: PathBuf,
    /// `<cwd>/.lingxi/settings.local.json`.
    pub local_settings: PathBuf,
    /// `<managed_dir>/managed-settings.json`.
    pub policy_settings: PathBuf,
    /// `<managed_dir>/managed-settings.d/` drop-in directory; any `*.json`
    /// inside maps to [`ConfigChangeSource::PolicySettings`].
    pub policy_drop_in_dir: PathBuf,
}

impl SettingsPaths {
    /// Resolve the watched settings paths from the composition root inputs.
    /// `lingxi_home` is the user-global config root (`~/.claude`); `cwd` is the
    /// project root. The managed/policy paths come from [`managed_settings_dir`].
    #[must_use]
    pub fn resolve(lingxi_home: &Path, cwd: &Path) -> Self {
        let managed = managed_settings_dir();
        Self {
            user_settings: lingxi_home.join("settings.json"),
            project_settings: cwd.join(branding::DOT_DIR).join("settings.json"),
            local_settings: cwd.join(branding::DOT_DIR).join("settings.local.json"),
            policy_settings: managed.join("managed-settings.json"),
            policy_drop_in_dir: managed.join("managed-settings.d"),
        }
    }

    /// Classify a changed path to its [`ConfigChangeSource`] layer, or `None`
    /// if the path is not a settings file we watch. Mirrors `getSourceForPath`
    /// (`changeDetector.ts:361-375`): exact-match the three user/project/local
    /// files and the policy file, and treat any path inside the
    /// `managed-settings.d/` drop-in directory as `PolicySettings`.
    #[must_use]
    pub fn classify(&self, path: &Path) -> Option<ConfigChangeSource> {
        // Drop-in directory check first (a `.json` fragment inside it).
        if path.starts_with(&self.policy_drop_in_dir) {
            // Only `.json` fragments are policy settings (TS watches the
            // dir's `.json` children); ignore editor temp files etc.
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("json"))
            {
                return Some(ConfigChangeSource::PolicySettings);
            }
            return None;
        }
        if path == self.policy_settings {
            return Some(ConfigChangeSource::PolicySettings);
        }
        if path == self.user_settings {
            return Some(ConfigChangeSource::UserSettings);
        }
        if path == self.project_settings {
            return Some(ConfigChangeSource::ProjectSettings);
        }
        if path == self.local_settings {
            return Some(ConfigChangeSource::LocalSettings);
        }
        None
    }

    /// The deduplicated parent directories that must be watched so a change to
    /// any of the settings files is observed. Mirrors `getWatchTargets`
    /// (`changeDetector.ts:181-249`): watch the *directories* (not the files)
    /// so files created after init are still detected.
    #[must_use]
    pub fn watch_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut push = |d: Option<&Path>| {
            if let Some(d) = d {
                let d = d.to_path_buf();
                if !dirs.contains(&d) {
                    dirs.push(d);
                }
            }
        };
        push(self.user_settings.parent());
        push(self.project_settings.parent());
        push(self.local_settings.parent());
        push(self.policy_settings.parent());
        // The drop-in directory itself is watched directly.
        if !dirs.contains(&self.policy_drop_in_dir) {
            dirs.push(self.policy_drop_in_dir.clone());
        }
        dirs
    }
}

/// Cloneable owner of the spawned watcher tasks. Hosts use
/// [`Self::shutdown_and_drain`] for an immediate join; dropping the last clone
/// retains the legacy abort-on-drop fallback.
#[derive(Debug, Clone)]
pub struct SettingsWatcherHandle {
    owner: Arc<SettingsWatcherOwner>,
}

#[derive(Debug)]
struct SettingsWatcherOwner {
    state: std::sync::Mutex<SettingsWatcherState>,
    drained: tokio::sync::Notify,
}

#[derive(Debug)]
enum SettingsWatcherState {
    Running(Vec<JoinHandle<()>>),
    Draining,
    Drained,
}

impl SettingsWatcherOwner {
    fn begin_drain(self: &Arc<Self>) {
        let tasks = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match &mut *state {
                SettingsWatcherState::Running(tasks) => {
                    let tasks = std::mem::take(tasks);
                    *state = SettingsWatcherState::Draining;
                    Some(tasks)
                }
                SettingsWatcherState::Draining | SettingsWatcherState::Drained => None,
            }
        };
        let Some(tasks) = tasks else {
            return;
        };
        for task in &tasks {
            task.abort();
        }
        if tasks.is_empty() {
            *self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SettingsWatcherState::Drained;
            self.drained.notify_waiters();
            return;
        }
        let owner = self.clone();
        tokio::spawn(async move {
            for task in tasks {
                let _ = task.await;
            }
            *owner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = SettingsWatcherState::Drained;
            owner.drained.notify_waiters();
        });
    }

    async fn wait_drained(&self) {
        loop {
            let notified = self.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if matches!(
                &*self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
                SettingsWatcherState::Drained
            ) {
                return;
            }
            notified.await;
        }
    }
}

impl Drop for SettingsWatcherOwner {
    fn drop(&mut self) {
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let SettingsWatcherState::Running(tasks) = state {
            for task in tasks.iter() {
                task.abort();
            }
        }
    }
}

impl SettingsWatcherHandle {
    /// An empty handle that owns no tasks (e.g. when no directory could be
    /// watched). Dropping it is a no-op.
    #[must_use]
    pub fn empty() -> Self {
        Self::from_tasks(Vec::new())
    }

    /// Number of live watch tasks (one per successfully-watched directory).
    #[must_use]
    pub fn task_count(&self) -> usize {
        match &*self
            .owner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            SettingsWatcherState::Running(tasks) => tasks.len(),
            SettingsWatcherState::Draining | SettingsWatcherState::Drained => 0,
        }
    }

    fn from_tasks(tasks: Vec<JoinHandle<()>>) -> Self {
        Self {
            owner: Arc::new(SettingsWatcherOwner {
                state: std::sync::Mutex::new(SettingsWatcherState::Running(tasks)),
                drained: tokio::sync::Notify::new(),
            }),
        }
    }

    /// Abort every watch job and wait for their futures to release the
    /// orchestrator/filesystem owners. The join is owned after the first call,
    /// so cancelling one waiter cannot make a concurrent shutdown return early.
    pub async fn shutdown_and_drain(&self) {
        self.owner.begin_drain();
        self.owner.wait_drained().await;
    }
}

/// The settings watcher. Owns the resolved paths + the fire seam and spawns the
/// per-directory watch loops.
pub struct SettingsWatcher {
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
    permission_gate: Option<Arc<dyn PermissionGate>>,
}

impl SettingsWatcher {
    /// Construct a watcher for the given `lingxi_home` / `cwd`, firing through
    /// `firer` (the live orchestrator in production).
    #[must_use]
    pub fn new(lingxi_home: &Path, cwd: &Path, firer: Arc<dyn ConfigChangeFirer>) -> Self {
        Self {
            paths: SettingsPaths::resolve(lingxi_home, cwd),
            firer,
            permission_gate: None,
        }
    }

    #[must_use]
    pub fn with_permission_gate(mut self, gate: Arc<dyn PermissionGate>) -> Self {
        self.permission_gate = Some(gate);
        self
    }

    /// The resolved settings paths (exposed for tests / diagnostics).
    #[must_use]
    pub fn paths(&self) -> &SettingsPaths {
        &self.paths
    }

    /// Spawn the background watch loops via the injected `FileSystem` and return
    /// the owning [`SettingsWatcherHandle`].
    ///
    /// Only directories that currently exist are watched (the in-tree
    /// `watch_dir_with_debounce` errors on a missing target); a directory that
    /// appears later is simply not observed until the next boot — matching
    /// claude-code's `dirsWithExistingFiles` init-time gate. Best-effort: a
    /// directory that fails to watch is logged and skipped, never fatal.
    pub async fn spawn(self, fs: Arc<dyn FileSystem>) -> SettingsWatcherHandle {
        let Self {
            paths,
            firer,
            permission_gate,
        } = self;
        // Own each task before awaiting the next watch setup. Cancellation of
        // construction must abort prior loops even before a handle is returned.
        let handle = SettingsWatcherHandle::empty();
        for dir in paths.watch_dirs() {
            if !dir.is_dir() {
                continue;
            }
            let dir_str = dir.to_string_lossy().into_owned();
            let stream = match fs.watch(&dir_str).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, dir = %dir_str, "settings watch failed");
                    continue;
                }
            };
            let paths = paths.clone();
            let firer = firer.clone();
            let permission_gate = permission_gate.clone();
            let mut state = handle
                .owner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let SettingsWatcherState::Running(tasks) = &mut *state else {
                unreachable!("construction has not published the watcher handle");
            };
            tasks.push(tokio::spawn(async move {
                run_watch_loop_with_permission_gate(stream, paths, firer, permission_gate).await;
            }));
        }
        handle
    }
}

/// Drive one directory's change stream, classifying + firing per event. Split
/// out so tests can drive it with a synthetic stream (no real `FSEvents`).
pub async fn run_watch_loop(
    stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
) {
    run_watch_loop_with_permission_gate(stream, paths, firer, None).await;
}

/// Drive one directory's change stream and apply managed policy changes after
/// the ConfigChange hook has completed.
pub async fn run_watch_loop_with_permission_gate(
    mut stream: std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
    paths: SettingsPaths,
    firer: Arc<dyn ConfigChangeFirer>,
    permission_gate: Option<Arc<dyn PermissionGate>>,
) {
    while let Some(event) = stream.next().await {
        handle_event_with_permission_gate(
            &event,
            &paths,
            firer.as_ref(),
            permission_gate.as_deref(),
        )
        .await;
    }
}

/// Classify a single [`FileEvent`] and fire the `ConfigChange` hook when it
/// maps to a watched settings layer. A non-settings path is silently ignored
/// (mirrors `handleChange` early-returning when `getSourceForPath` is
/// undefined). Exposed for deterministic unit tests.
pub async fn handle_event(event: &FileEvent, paths: &SettingsPaths, firer: &dyn ConfigChangeFirer) {
    handle_event_with_permission_gate(event, paths, firer, None).await;
}

pub async fn handle_event_with_permission_gate(
    event: &FileEvent,
    paths: &SettingsPaths,
    firer: &dyn ConfigChangeFirer,
    permission_gate: Option<&dyn PermissionGate>,
) {
    let Some(source) = paths.classify(&event.path) else {
        return;
    };
    if permission::consume_internal_write(&event.path, std::time::Duration::from_secs(5)) {
        return;
    }
    firer
        .fire_config_change(source, Some(event.path.clone()))
        .await;
    if source == ConfigChangeSource::PolicySettings {
        if let Some(gate) = permission_gate {
            let tiers = managed_settings_raw_tiers().await;
            gate.update_auto_mode_disabled(auto_mode_disabled_from_managed_tiers(&tiers));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::FileEventKind;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DropBarrier {
        entered: tokio::sync::Notify,
        released: Mutex<bool>,
        release: std::sync::Condvar,
    }

    struct BarrierGuard(Arc<DropBarrier>);

    impl Drop for BarrierGuard {
        fn drop(&mut self) {
            self.0.entered.notify_one();
            let released = self.0.released.lock().unwrap();
            let _ = self
                .0
                .release
                .wait_timeout_while(released, std::time::Duration::from_secs(5), |ready| !*ready)
                .unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 3)]
    async fn settings_drain_survives_cancelled_waiter_and_joins_before_concurrent_return() {
        let barrier = Arc::new(DropBarrier::default());
        let owner = Arc::new(());
        let weak = Arc::downgrade(&owner);
        let guard = BarrierGuard(barrier.clone());
        let task = tokio::spawn(async move {
            let retained_owner = owner;
            let _guard = guard;
            std::future::pending::<()>().await;
            drop(retained_owner);
        });
        let handle = SettingsWatcherHandle::from_tasks(vec![task]);
        let first_handle = handle.clone();
        let first = tokio::spawn(async move { first_handle.shutdown_and_drain().await });
        barrier.entered.notified().await;
        first.abort();
        let _ = first.await;
        let second_handle = handle.clone();
        let mut second = tokio::spawn(async move { second_handle.shutdown_and_drain().await });
        let premature = tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
            .await
            .is_ok();
        *barrier.released.lock().unwrap() = true;
        barrier.release.notify_all();
        if !premature {
            tokio::time::timeout(std::time::Duration::from_secs(2), second)
                .await
                .unwrap()
                .unwrap();
        }
        assert!(!premature);
        assert!(weak.upgrade().is_none());
        handle.shutdown_and_drain().await;
    }

    struct ConstructionFs {
        calls: std::sync::atomic::AtomicUsize,
        second_watch: tokio::sync::Notify,
        first_stream_dropped: Arc<tokio::sync::Notify>,
    }

    struct PendingStream(Arc<tokio::sync::Notify>);

    impl Stream for PendingStream {
        type Item = FileEvent;
        fn poll_next(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<FileEvent>> {
            std::task::Poll::Pending
        }
    }

    impl Drop for PendingStream {
        fn drop(&mut self) {
            self.0.notify_one();
        }
    }

    #[async_trait]
    impl FileSystem for ConstructionFs {
        async fn read_file(
            &self,
            _: &str,
            _: Option<u64>,
            _: Option<u64>,
        ) -> Result<lingxi_core::host::FileContent, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn write_file(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        fn is_within_workspace(&self, _: &str) -> bool {
            true
        }
        async fn watch(
            &self,
            _: &str,
        ) -> Result<
            std::pin::Pin<Box<dyn Stream<Item = FileEvent> + Send>>,
            lingxi_core::host::FsError,
        > {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Ok(Box::pin(PendingStream(self.first_stream_dropped.clone())));
            }
            self.second_watch.notify_one();
            std::future::pending().await
        }
        async fn append_file(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn truncate(&self, _: &str, _: u64) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn file_mtime(
            &self,
            _: &str,
        ) -> Result<std::time::SystemTime, lingxi_core::host::FsError> {
            Ok(std::time::SystemTime::UNIX_EPOCH)
        }
        async fn file_size(&self, _: &str) -> Result<u64, lingxi_core::host::FsError> {
            Ok(0)
        }
        async fn delete_file(&self, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn symlink(&self, _: &str, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
        async fn flock_exclusive(
            &self,
            _: &str,
        ) -> Result<Box<dyn lingxi_core::host::FlockGuard>, lingxi_core::host::FsError> {
            Err(lingxi_core::host::FsError::Io("unused".into()))
        }
        async fn fsync(&self, _: &str) -> Result<(), lingxi_core::host::FsError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancelled_settings_construction_aborts_already_spawned_directory_loops() {
        let root = tempfile::tempdir().unwrap();
        let user = root.path().join("user");
        let project = root.path().join("project");
        std::fs::create_dir_all(&user).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let firer = Arc::new(RecordingFirer::default());
        let weak = Arc::downgrade(&firer);
        let fs = Arc::new(ConstructionFs {
            calls: std::sync::atomic::AtomicUsize::new(0),
            second_watch: tokio::sync::Notify::new(),
            first_stream_dropped: Arc::new(tokio::sync::Notify::new()),
        });
        let watcher = SettingsWatcher {
            paths: SettingsPaths {
                user_settings: user.join("settings.json"),
                project_settings: project.join("settings.json"),
                local_settings: project.join("settings.local.json"),
                policy_settings: root.path().join("absent/policy.json"),
                policy_drop_in_dir: root.path().join("absent/dropins"),
            },
            firer,
            permission_gate: None,
        };
        let task_fs = fs.clone();
        let construction = tokio::spawn(async move { watcher.spawn(task_fs).await });
        fs.second_watch.notified().await;
        construction.abort();
        let _ = construction.await;
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            fs.first_stream_dropped.notified().await;
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled construction must release prior directory loops and firer");
    }

    /// Recording fake firer — captures every `(source, file_path)` the watcher
    /// fires so tests can assert deterministically without a real orchestrator.
    #[derive(Default)]
    struct RecordingFirer {
        fired: Mutex<Vec<(ConfigChangeSource, Option<PathBuf>)>>,
    }

    #[async_trait]
    impl ConfigChangeFirer for RecordingFirer {
        async fn fire_config_change(&self, source: ConfigChangeSource, file_path: Option<PathBuf>) {
            self.fired.lock().unwrap().push((source, file_path));
        }
    }

    fn paths() -> SettingsPaths {
        // Fixed roots so the mapping assertions are platform-independent for
        // the user/project/local layers (policy uses the real managed dir).
        SettingsPaths::resolve(Path::new("/home/u/.lingxi"), Path::new("/work/proj"))
    }

    /// [`paths`] rooted at a test-specific project dir.
    ///
    /// `permission::mark_internal_write` keeps its marks in a PROCESS-GLOBAL
    /// map keyed by path, so two tests that touch the same settings path race:
    /// one test's mark suppresses the other's event, or the other consumes the
    /// mark this one was about to rely on. Any test that marks (or expects to
    /// fire for) `settings.json` must therefore own a unique root.
    fn paths_rooted(project: &str) -> SettingsPaths {
        SettingsPaths::resolve(Path::new("/home/u/.lingxi"), Path::new(project))
    }

    fn ev(path: &str) -> FileEvent {
        FileEvent {
            path: PathBuf::from(path),
            kind: FileEventKind::Modified,
        }
    }

    #[test]
    fn classify_user_project_local() {
        let p = paths();
        assert_eq!(
            p.classify(Path::new("/home/u/.lingxi/settings.json")),
            Some(ConfigChangeSource::UserSettings)
        );
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/settings.json")),
            Some(ConfigChangeSource::ProjectSettings)
        );
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/settings.local.json")),
            Some(ConfigChangeSource::LocalSettings)
        );
    }

    #[test]
    fn classify_policy_file_and_dropin() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let managed = managed_settings_dir();
        let p = paths();
        assert_eq!(
            p.classify(&managed.join("managed-settings.json")),
            Some(ConfigChangeSource::PolicySettings)
        );
        // A `.json` fragment in the drop-in dir is policy settings.
        assert_eq!(
            p.classify(&managed.join("managed-settings.d").join("10-org.json")),
            Some(ConfigChangeSource::PolicySettings)
        );
        // A non-json file in the drop-in dir is ignored.
        assert_eq!(
            p.classify(&managed.join("managed-settings.d").join("README.md")),
            None
        );
    }

    #[test]
    fn classify_unrelated_path_is_none() {
        let p = paths();
        assert_eq!(
            p.classify(Path::new("/work/proj/.lingxi/agents/x.md")),
            None
        );
        assert_eq!(p.classify(Path::new("/work/proj/src/main.rs")), None);
        // A sibling json in the project .claude dir that is NOT a watched
        // settings file maps to nothing.
        assert_eq!(p.classify(Path::new("/work/proj/.lingxi/other.json")), None);
    }

    #[test]
    fn managed_auto_mode_fold_covers_base_dropins_and_clear() {
        let enabled = vec![
            r#"{"disableAutoMode":"disable"}"#.to_string(),
            r#"{"permissions":{"allow":["Read"]}}"#.to_string(),
        ];
        assert!(auto_mode_disabled_from_managed_tiers(&enabled));
        assert!(!auto_mode_disabled_from_managed_tiers(&[
            r#"{"disableAutoMode":"enable"}"#.to_string(),
            r#"{"permissions":{}}"#.to_string(),
        ]));
    }

    #[tokio::test]
    async fn handle_event_fires_with_correct_source_per_path() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let p = paths();
        let firer = RecordingFirer::default();

        handle_event(&ev("/home/u/.lingxi/settings.json"), &p, &firer).await;
        handle_event(&ev("/work/proj/.lingxi/settings.json"), &p, &firer).await;
        handle_event(&ev("/work/proj/.lingxi/settings.local.json"), &p, &firer).await;
        let policy = managed_settings_dir().join("managed-settings.json");
        handle_event(&ev(&policy.to_string_lossy()), &p, &firer).await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 4);
        assert_eq!(recorded[0].0, ConfigChangeSource::UserSettings);
        assert_eq!(
            recorded[0].1,
            Some(PathBuf::from("/home/u/.lingxi/settings.json"))
        );
        assert_eq!(recorded[1].0, ConfigChangeSource::ProjectSettings);
        assert_eq!(recorded[2].0, ConfigChangeSource::LocalSettings);
        assert_eq!(recorded[3].0, ConfigChangeSource::PolicySettings);
        assert_eq!(recorded[3].1, Some(policy));
    }

    #[tokio::test]
    async fn handle_event_non_settings_is_noop() {
        let p = paths();
        let firer = RecordingFirer::default();
        handle_event(&ev("/work/proj/src/main.rs"), &p, &firer).await;
        handle_event(&ev("/home/u/.lingxi/LINGXI.md"), &p, &firer).await;
        assert!(firer.fired.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn handle_event_suppresses_one_recent_internal_write() {
        // Own project root — see `paths_rooted`: sharing `/work/proj` with
        // `handle_event_fires_with_correct_source_per_path` made the two race
        // through the global internal-write map (each failed the other's
        // assertion, intermittently, under a full-workspace run).
        let p = paths_rooted("/work/suppress-one");
        let firer = RecordingFirer::default();
        let path = PathBuf::from("/work/suppress-one/.lingxi/settings.json");
        permission::mark_internal_write(&path);

        handle_event(&ev(&path.to_string_lossy()), &p, &firer).await;
        assert!(firer.fired.lock().unwrap().is_empty());

        handle_event(&ev(&path.to_string_lossy()), &p, &firer).await;
        assert_eq!(firer.fired.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn run_watch_loop_drives_synthetic_stream() {
        // Inject a synthetic change stream (no real FSEvents) and assert the
        // loop classifies + fires each event, then exits cleanly when the
        // stream ends — deterministic, no FS timing dependency.
        let p = paths();
        let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
        let events = vec![
            ev("/home/u/.lingxi/settings.json"),
            ev("/work/proj/src/ignored.rs"),
            ev("/work/proj/.lingxi/settings.local.json"),
        ];
        let stream = tokio_stream::iter(events);
        run_watch_loop(Box::pin(stream), p, firer.clone()).await;

        let recorded = firer.fired.lock().unwrap();
        assert_eq!(recorded.len(), 2, "only the two settings paths fire");
        assert_eq!(recorded[0].0, ConfigChangeSource::UserSettings);
        assert_eq!(recorded[1].0, ConfigChangeSource::LocalSettings);
    }

    #[test]
    fn watch_dirs_dedup_and_cover_layers() {
        // Reads the managed dir (via `paths()`/`SettingsPaths::resolve` AND a
        // second direct call), while ~10 tests in `lib.rs` flip
        // `LINGXI_MANAGED_DIR` around their own bodies under
        // `crate::desktop::tests::MANAGED_ENV_LOCK`. Without taking the SAME lock the two
        // reads here can straddle a flip and the policy path stops classifying.
        let _managed_env = crate::desktop::tests::MANAGED_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let p = paths();
        let dirs = p.watch_dirs();
        // user dir, project .claude dir (covers both project + local), policy
        // managed dir, drop-in dir. project + local share `.lingxi/` so dedup
        // collapses them.
        assert!(dirs.contains(&PathBuf::from("/home/u/.lingxi")));
        assert!(dirs.contains(&PathBuf::from("/work/proj/.lingxi")));
        assert!(dirs.contains(&managed_settings_dir()));
        assert!(dirs.contains(&managed_settings_dir().join("managed-settings.d")));
        // `.claude` appears exactly once despite two files inside it.
        let claude_count = dirs
            .iter()
            .filter(|d| *d == &PathBuf::from("/work/proj/.lingxi"))
            .count();
        assert_eq!(claude_count, 1);
    }
}
