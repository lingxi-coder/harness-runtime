//! Tokio-backed [`RuntimeSpawner`] for desktop hosts.
//!
//! Wraps `tokio::spawn` so engine code can request background tasks without
//! depending on tokio directly. The spawner keeps a registry of handles so
//! [`RuntimeSpawner::cancel`] can abort a running task by id.

use async_trait::async_trait;
use lingxi_core::host::{BackgroundTaskHandle, RuntimeError, RuntimeSpawner};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::task::JoinHandle;

/// Tokio-backed spawner. Holds the live `JoinHandle`s keyed by task id so
/// they can be aborted on demand.
pub struct PosixRuntime {
    next: AtomicU64,
    handles: Mutex<HashMap<u64, JoinHandle<()>>>,
}

impl PosixRuntime {
    /// Build a fresh spawner.
    #[must_use]
    pub fn new() -> Self {
        Self {
            next: AtomicU64::new(1),
            handles: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for PosixRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
#[allow(clippy::unwrap_used)] // std Mutex poison is treated as a fatal bug.
impl RuntimeSpawner for PosixRuntime {
    async fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().unwrap().insert(id, tokio::spawn(task));
        Ok(BackgroundTaskHandle {
            task_name: name.to_string(),
            task_id: id,
        })
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError> {
        if let Some(h) = self.handles.lock().unwrap().remove(&handle.task_id) {
            h.abort();
        }
        Ok(())
    }
}
