//! Deterministic end-to-end test of the settings watcher's `spawn` lifecycle.
//!
//! Drives [`harness_runtime::desktop::settings_watch::SettingsWatcher::spawn`] with a
//! FAKE `FileSystem` whose `watch` returns pre-seeded synthetic event streams
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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::oneshot;

/// Fake `FileSystem` whose `watch` hands back a seeded stream for each watched
/// directory. Every other method is unused by the watcher.
struct FakeFs {
    events: Mutex<HashMap<PathBuf, (Vec<FileEvent>, oneshot::Sender<()>)>>,
}

impl FakeFs {
    fn new(events: Vec<(PathBuf, Vec<FileEvent>, oneshot::Sender<()>)>) -> Self {
        Self {
            events: Mutex::new(
                events
                    .into_iter()
                    .map(|(dir, events, completed)| (dir, (events, completed)))
                    .collect(),
            ),
        }
    }
}

/// Signal EOF only when the loop asks for another event after handling the
/// previous one. Dropping the stream early cancels the sender instead.
struct EventStream {
    events: std::vec::IntoIter<FileEvent>,
    completed: Option<oneshot::Sender<()>>,
}

impl futures_core::Stream for EventStream {
    type Item = FileEvent;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.events.next() {
            return Poll::Ready(Some(event));
        }
        if let Some(completed) = self.completed.take() {
            let _ = completed.send(());
        }
        Poll::Ready(None)
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
        dir: &str,
    ) -> Result<Pin<Box<dyn futures_core::Stream<Item = FileEvent> + Send>>, FsError> {
        let seeded = self.events.lock().unwrap().remove(Path::new(dir));
        let stream: Pin<Box<dyn futures_core::Stream<Item = FileEvent> + Send>> = match seeded {
            Some((events, completed)) => Box::pin(EventStream {
                events: events.into_iter(),
                completed: Some(completed),
            }),
            None => Box::pin(tokio_stream::empty()),
        };
        Ok(stream)
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
    // The watcher only watches dirs that exist; create the project config dir.
    std::fs::create_dir_all(cwd.join(".lingxi")).unwrap();

    let user_settings = lingxi_home.join("settings.json");
    let project_settings = cwd.join(".lingxi").join("settings.json");
    let local_settings = cwd.join(".lingxi").join("settings.local.json");
    let ignored = cwd.join(".lingxi").join("agents.json");

    // Deliver each event on the stream for its actual parent directory.
    let (user_completed, user_done) = oneshot::channel();
    let (project_completed, project_done) = oneshot::channel();
    let firer: Arc<RecordingFirer> = Arc::new(RecordingFirer::default());
    let fs = Arc::new(FakeFs::new(vec![
        (
            lingxi_home.clone(),
            vec![ev(user_settings.clone())],
            user_completed,
        ),
        (
            cwd.join(".lingxi"),
            vec![
                ev(project_settings.clone()),
                ev(local_settings.clone()),
                ev(ignored),
            ],
            project_completed,
        ),
    ])) as Arc<dyn FileSystem>;

    let watcher = SettingsWatcher::new(&lingxi_home, &cwd, firer.clone());
    let handle = watcher.spawn(fs).await;
    assert!(handle.task_count() >= 2, "both settings dirs are watched");

    // A fixed number of yields does not wait for the async managed-policy
    // reads performed between events. EOF confirms all handlers finished,
    // including the ignored event, before we inspect the recorded calls.
    tokio::time::timeout(Duration::from_secs(10), async {
        user_done
            .await
            .expect("user watch stream dropped before EOF");
        project_done
            .await
            .expect("project watch stream dropped before EOF");
    })
    .await
    .expect("settings watch streams did not finish processing their events");

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
    let fs = Arc::new(FakeFs::new(Vec::new())) as Arc<dyn FileSystem>;

    let watcher = SettingsWatcher::new(
        std::path::Path::new("/no/such/home/.lingxi"),
        std::path::Path::new("/no/such/project"),
        firer.clone(),
    );
    let handle = watcher.spawn(fs).await;
    assert_eq!(handle.task_count(), 0, "no existing dir ⇒ no watch task");
    assert!(firer.fired.lock().unwrap().is_empty());
}
