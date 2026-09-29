//! `MockRuntimeSpawner` — uses tokio's runtime under the hood, but is the only
//! place in the workspace allowed to import tokio outside of dev-deps.

#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Tokio-backed [`RuntimeSpawner`] used by engine tests. The spawner owns each
/// task's [`JoinHandle`] so cancellation can abort it.
pub struct MockRuntimeSpawner {
    next_id: AtomicU64,
    handles: Mutex<HashMap<u64, JoinHandle<()>>>,
}

impl Default for MockRuntimeSpawner {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl RuntimeSpawner for MockRuntimeSpawner {
    async fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let h = tokio::spawn(task);
        self.handles.lock().unwrap().insert(id, h);
        Ok(BackgroundTaskHandle {
            task_name: name.into(),
            task_id: id,
        })
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        let h = self.handles.lock().unwrap().remove(&handle.task_id);
        if let Some(h) = h {
            h.abort();
            Ok(())
        } else {
            Err(RuntimeError::NotFound(handle.task_name.clone()))
        }
    }
}
