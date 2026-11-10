// Shared test-only spool filesystem for scheduler and registry tests.
use async_trait::async_trait;
use lingxi_core::host::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use std::collections::HashMap;
use std::pin::Pin;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct InMemoryFs {
    files: tokio::sync::Mutex<HashMap<String, String>>,
}

impl InMemoryFs {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl FileSystem for InMemoryFs {
    async fn read_file(
        &self,
        path: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let content = self
            .files
            .lock()
            .await
            .get(path)
            .cloned()
            .unwrap_or_default();
        Ok(lingxi_core::host::filesystem::apply_line_window(
            content, offset, limit,
        ))
    }
    async fn write_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.files.lock().await.insert(path.into(), content.into());
        Ok(())
    }
    fn is_within_workspace(&self, _path: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _path: &str,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        Err(FsError::Io(
            "watch is unused by auto-dream spool tests".into(),
        ))
    }
    async fn append_file(&self, path: &str, content: &str) -> Result<(), FsError> {
        self.files
            .lock()
            .await
            .entry(path.into())
            .or_default()
            .push_str(content);
        Ok(())
    }
    async fn truncate(&self, path: &str, length: u64) -> Result<(), FsError> {
        if let Some(content) = self.files.lock().await.get_mut(path) {
            let length = usize::try_from(length)
                .unwrap_or(usize::MAX)
                .min(content.len());
            content.truncate(length);
        }
        Ok(())
    }
    async fn file_mtime(&self, _path: &str) -> Result<SystemTime, FsError> {
        Ok(UNIX_EPOCH)
    }
    async fn file_size(&self, path: &str) -> Result<u64, FsError> {
        Ok(self
            .files
            .lock()
            .await
            .get(path)
            .map_or(0, |content| content.len() as u64))
    }
    async fn delete_file(&self, path: &str) -> Result<(), FsError> {
        self.files.lock().await.remove(path);
        Ok(())
    }
    async fn symlink(&self, _target: &str, _link: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn flock_exclusive(&self, _path: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io(
            "locking is unused by auto-dream spool tests".into(),
        ))
    }
    async fn fsync(&self, _path: &str) -> Result<(), FsError> {
        Ok(())
    }
}
