//! M5-01 Task 7 integration test: end-to-end `TaskRegistryHandle::output`
//! round-trip — create -> seed spool -> output via trait surface.
//!
//! Lives here (not in `lingxi-tools/tests/`) because `lingxi-tasks` is
//! upstream of `lingxi-tools` in the dependency graph; the reverse direction
//! would create a cycle.

#![allow(
    clippy::unwrap_used,
    clippy::cast_possible_truncation,
    clippy::map_unwrap_or
)]

use platform_api::filesystem::{FileContent, FileEvent, FileSystem, FlockGuard, FsError};
use platform_api::task_registry::{TaskCreateInput, TaskRegistryError, TaskRegistryHandle};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tasks::output_manager::TaskOutputManager;
use tasks::registry::TaskRegistry;
use tempfile::tempdir;
use tokio::sync::Mutex as TokioMutex;

struct MapFs {
    files: TokioMutex<HashMap<String, String>>,
}

#[async_trait::async_trait]
impl FileSystem for MapFs {
    async fn read_file(
        &self,
        p: &str,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<FileContent, FsError> {
        let map = self.files.lock().await;
        let content = map.get(p).cloned().unwrap_or_default();
        let off = offset.unwrap_or(0) as usize;
        let body: String = content.chars().skip(off).collect();
        let trimmed = if let Some(l) = limit {
            body.chars().take(l as usize).collect()
        } else {
            body.clone()
        };
        Ok(FileContent {
            truncated: limit.map(|l| body.len() as u64 > l).unwrap_or(false),
            total_lines: content.lines().count() as u64,
            content: trimmed,
        })
    }
    async fn write_file(&self, p: &str, b: &str) -> Result<(), FsError> {
        self.files.lock().await.insert(p.into(), b.into());
        Ok(())
    }
    fn is_within_workspace(&self, _: &str) -> bool {
        true
    }
    async fn watch(
        &self,
        _: &str,
    ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = FileEvent> + Send>>, FsError> {
        Err(FsError::Io("nope".into()))
    }
    async fn append_file(&self, p: &str, b: &str) -> Result<(), FsError> {
        let mut m = self.files.lock().await;
        m.entry(p.into()).or_default().push_str(b);
        Ok(())
    }
    async fn truncate(&self, _: &str, _: u64) -> Result<(), FsError> {
        Ok(())
    }
    async fn file_mtime(&self, _: &str) -> Result<std::time::SystemTime, FsError> {
        Ok(std::time::SystemTime::UNIX_EPOCH)
    }
    async fn file_size(&self, p: &str) -> Result<u64, FsError> {
        Ok(self
            .files
            .lock()
            .await
            .get(p)
            .map(|s| s.len() as u64)
            .unwrap_or(0))
    }
    async fn delete_file(&self, _: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn symlink(&self, _: &str, _: &str) -> Result<(), FsError> {
        Ok(())
    }
    async fn flock_exclusive(&self, _: &str) -> Result<Box<dyn FlockGuard>, FsError> {
        Err(FsError::Io("nope".into()))
    }
    async fn fsync(&self, _: &str) -> Result<(), FsError> {
        Ok(())
    }
}

#[tokio::test]
async fn task_registry_handle_output_round_trip_via_real_output_manager() {
    let dir = tempdir().unwrap();
    let fs: Arc<dyn FileSystem> = Arc::new(MapFs {
        files: TokioMutex::new(HashMap::new()),
    });
    let runtime = Arc::new(test_harness::mocks::MockRuntimeSpawner::default());
    let out_mgr = Arc::new(TaskOutputManager::new(
        PathBuf::from(dir.path()),
        fs.clone(),
    ));
    let registry = Arc::new(TaskRegistry::new(runtime, fs.clone(), out_mgr.clone()));
    let h: &dyn TaskRegistryHandle = registry.as_ref();

    // 1) create
    let rec = h
        .create(TaskCreateInput {
            task_type: "local_bash".into(),
            description: "spool round-trip".into(),
        })
        .await
        .expect("create");

    // 2) seed spool via the manager's filesystem (simulates a handler).
    let path = registry
        .get(&rec.task_id)
        .await
        .unwrap()
        .base()
        .output_file
        .clone();
    let path_str = path.to_str().unwrap().to_string();
    fs.write_file(&path_str, "hello\nworld\n").await.unwrap();

    // 3) output via the handle trait surface
    let chunk = h.output(&rec.task_id, None).await.expect("output");
    assert_eq!(chunk.content, "hello\nworld\n");
    assert_eq!(chunk.total_lines, 2);
    assert!(!chunk.truncated);

    // 4) verify NotFound surface still works for unknown ids
    let missing = h.output("zzzbogus0", None).await;
    assert!(matches!(missing, Err(TaskRegistryError::NotFound(_))));
}
