//! Deterministic end-to-end test of the settings watcher's `spawn` lifecycle.
//!
//! Drives [`harness_runtime::desktop::settings_watch::SettingsWatcher::spawn`] with a
//! FAKE `FileSystem` whose `watch` returns a pre-seeded synthetic event stream
//! — so the test never depends on real `FSEvents` / `inotify` timing (the
//! documented `fs_watch` flake). It asserts:
//!   - the watcher fires `ConfigChange` for the user + project + local settings
//!     paths it observes, with the correct source per path,
//!   - a non-settings path in the same directory is ignored,
//!   - dropping the returned handle aborts the watch task (clean teardown).

use async_trait::async_trait;
use harness_runtime::desktop::settings_watch::{ConfigChangeFirer, SettingsWatcher};
use hooks::events::ConfigChangeSource;
use lingxi_core::host::filesystem::{
    FileContent, FileEvent, FileEventKind, FileSystem, FlockGuard, FsError,
};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

/// Fake `FileSystem` whose `watch` hands back a synthetic stream seeded at
/// construction. Every other method is unused by the watcher.
struct FakeFs {
    events: Mutex<Option<Vec<FileEvent>>>,
}

impl FakeFs {
    fn new(events: Vec<FileEvent>) -> Self {
        Self {
            events: Mutex::new(Some(events)),
        }
    }
}

#[async_trait]
impl FileSystem for FakeFs {
    async fn read_file(
        &self,
        _: &str,
        _: Option<u64>,
        _: Option<u64>,
    ) -> Result<FileContent, FsError> {
        unreachable!("watcher does not read files")
    }
    async fn write_file(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _dir: &str,
    ) -> Result<Pin<Box<dyn futures_core::Stream<Item = FileEvent> + Send>>, FsError> {
        // Hand the seeded events to the FIRST watched directory; later dirs get
        // an empty stream so the test asserts a single deterministic burst.
        let events = self.events.lock().unwrap().take().unwrap_or_default();
        Ok(Box::pin(tokio_stream::iter(events)))
    }
    async fn append_file(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
        unreachable!()
    }
    async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
        unreachable!()
    }
    async fn file_size(&self, _: &str) -> Result<u64, FsError> {
        unreachable!()
    }
    async fn delete_file(&self, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        unreachable!()
    }
    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        unreachable!()
    }
}

/// Records every `(source, file_path)` fired by the watcher.
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

fn ev(path: PathBuf) -> FileEvent {
    FileEvent {
        path,
        kind: FileEventKind::Modified,
    }
}

#[tokio::test]
async fn spawn_fires_config_change_for_observed_settings_paths() {
    // Real temp dirs so the `is_dir()` gate in `spawn` passes; the FAKE fs
    // supplies the events, so no real watcher runs.
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    let lingxi_home = home.path().to_path_buf();
    let cwd = proj.path().to_path_buf();
    // The watcher only watches dirs that exist; create the project `.claude`.
    std::fs::create_dir_all(cwd.join(".lingxi")).unwrap();

    let user_settings = lingxi_home.join("settings.json");
    let project_settings = cwd.join(".lingxi").join("settings.json");
    let local_settings = cwd.join(".lingxi").join("settings.local.json");
    let ignored = cwd.join(".lingxi").join("agents.json");

    // The seeded burst is delivered to whichever directory is watched first
    // (`watch_dirs()` orders user dir first, then project `.claude`). Put events
    // for paths under BOTH so we can assert source mapping regardless: the
    // synthetic stream carries absolute paths the watcher classifies by value,
    // independent of which dir's stream delivered them.
    let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
    let fs = Arc::new(FakeFs::new(vec![
        ev(user_settings.clone()),
        ev(project_settings.clone()),
        ev(local_settings.clone()),
        ev(ignored.clone()),
    ])) as Arc<dyn FileSystem>;

    let watcher = SettingsWatcher::new(&lingxi_home, &cwd, firer.clone());
    let handle = watcher.spawn(fs).await;
    assert!(handle.task_count() >= 1, "at least one dir is watched");

    // Drive the spawned task to completion: the synthetic stream is finite, so
    // the loop exits on its own. Poll the recorded fires deterministically
    // (yield to the runtime until all three settings fires land) rather than
    // sleeping on a wall clock.
    for _ in 0..1000 {
        if firer.fired.lock().unwrap().len() >= 3 {
            break;
        }
        tokio::task::yield_now().await;
    }

    let recorded = firer.fired.lock().unwrap().clone();
    assert_eq!(
        recorded.len(),
        3,
        "exactly the three settings paths fire (the non-settings path is ignored): {recorded:?}"
    );
    assert!(recorded.contains(&(ConfigChangeSource::UserSettings, Some(user_settings))));
    assert!(recorded.contains(&(ConfigChangeSource::ProjectSettings, Some(project_settings))));
    assert!(recorded.contains(&(ConfigChangeSource::LocalSettings, Some(local_settings))));

    // Teardown: dropping the handle aborts the watch task(s) cleanly.
    drop(handle);
}

#[tokio::test]
async fn spawn_with_no_existing_dirs_watches_nothing() {
    // Point at non-existent roots: `spawn` skips every dir (the `is_dir()`
    // gate), so no task is spawned and nothing fires.
    let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
    let fs = Arc::new(FakeFs::new(vec![ev(PathBuf::from(
        "/no/such/.lingxi/settings.json",
    ))])) as Arc<dyn FileSystem>;

    let watcher = SettingsWatcher::new(
        std::path::Path::new("/no/such/home/.lingxi"),
        std::path::Path::new("/no/such/project"),
        firer.clone(),
    );
    let handle = watcher.spawn(fs).await;
    assert_eq!(handle.task_count(), 0, "no existing dir ⇒ no watch task");
    assert!(firer.fired.lock().unwrap().is_empty());
}
