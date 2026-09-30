//! Desktop unit-test watcher backend. Real file operations are delegated;
//! only native event acquisition is replaced. No native worker is started or
//! detached, and production watcher composition is unchanged.

use async_trait::async_trait;
use futures_core::Stream;
use lingxi_core::host::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use platform_posix::PosixFileSystem;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::task::{Context, Poll};
use std::time::SystemTime;

pub(super) struct WatchFs {
    inner: PosixFileSystem,
    live: Arc<AtomicUsize>,
}

impl WatchFs {
    pub(super) fn new(root: PathBuf) -> Self {
        Self {
            inner: PosixFileSystem::new(root),
            live: Arc::new(AtomicUsize::new(0)),
        }
    }
}

struct PendingWatch(Arc<AtomicUsize>);

impl Stream for PendingWatch {
    type Item = FileEvent;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<FileEvent>> {
        Poll::Pending
    }
}

impl Drop for PendingWatch {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl FileSystem for WatchFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        self.inner.read_file(path, offset, limit).await
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.inner.write_file(path, content).await
    }
    fn is_within_workspace(&self, path: &str) -> bool {
        self.inner.is_within_workspace(path)
    }
    async fn watch(
        &self,
        dir: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = FileEvent> + Send>>, FsError> {
        if !std::path::Path::new(dir).is_dir() {
            return Err(FsError::Io(format!(
                "watch target is not a directory: {dir}"
            )));
        }
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(Box::pin(PendingWatch(self.live.clone())))
    }
    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.inner.append_file(path, content).await
    }
    async fn truncate(&self, path: &str, len: u64) -> Result<(), FsError> {
        self.inner.truncate(path, len).await
    }
    async fn file_mtime(&self, path: &str) -> Result<SystemTime, FsError> {
        self.inner.file_mtime(path).await
    }
    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        self.inner.file_size(path).await
    }
    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.inner.delete_file(path).await
    }
    async fn symlink(&self, target: &str, link: &str) -> Result<(), FsError> {
        self.inner.symlink(target, link).await
    }
    async fn flock_exclusive(&self, path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        self.inner.flock_exclusive(path).await
    }
    async fn fsync(&self, path: &str) -> Result<(), FsError> {
        self.inner.fsync(path).await
    }
}

struct Firer;

#[async_trait]
impl crate::desktop::settings_watch::ConfigChangeFirer for Firer {
    async fn fire_config_change(&self, _: hooks::events::ConfigChangeSource, _: Option<PathBuf>) {}
}

#[async_trait]
impl hooks::file_changed_firer::FileChangedFirer for Firer {
    async fn fire(&self, _: hooks::file_changed_firer::FileChangedFire) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[tokio::test]
async fn desktop_test_watch_stream_drop_releases_owner() {
    let root = tempfile::tempdir().unwrap();
    let fs = WatchFs::new(root.path().to_path_buf());
    let stream = fs.watch(root.path().to_str().unwrap()).await.unwrap();
    assert_eq!(fs.live.load(Ordering::SeqCst), 1);
    drop(stream);
    assert_eq!(fs.live.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn desktop_test_watchers_start_and_drain_real_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let fs = Arc::new(WatchFs::new(root.path().to_path_buf()));
    let settings = crate::desktop::settings_watch::SettingsWatcher::new(
        root.path(),
        root.path(),
        Arc::new(Firer),
    )
    .spawn(fs.clone())
    .await;
    let settings_count = fs.live.load(Ordering::SeqCst);
    assert!(settings_count > 0);
    let changed = crate::desktop::file_changed_watch::FileChangedWatcher::new(
        &["watched.txt"],
        root.path(),
        Arc::new(Firer),
    )
    .spawn(fs.clone())
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while fs.live.load(Ordering::SeqCst) <= settings_count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("file-changed supervisor must arm its stream");
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        settings.shutdown_and_drain().await;
        changed.shutdown_and_drain().await;
    })
    .await
    .expect("real watcher owners must finish draining");
    assert_eq!(fs.live.load(Ordering::SeqCst), 0);
}
