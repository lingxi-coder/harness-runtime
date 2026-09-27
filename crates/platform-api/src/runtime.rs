//! Background task spawning abstraction. Engine code MUST NOT call
//! `tokio::spawn` directly — see D17.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use thiserror::Error;

/// Opaque handle to a background task previously submitted via
/// [`RuntimeSpawner::spawn`].
///
/// The handle is `Serialize`/`Deserialize` so it can flow through telemetry
/// and snapshots without coupling to a runtime-specific join handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackgroundTaskHandle {
    /// Human-readable name supplied at spawn time (used for logs and metrics).
    pub task_name: String,
    /// Spawner-assigned monotonically-increasing ID.
    pub task_id: u64,
}

/// Spawner for background async tasks.
///
/// Engine code receives an `Arc<dyn RuntimeSpawner>` and never depends on a
/// concrete async runtime.
#[async_trait]
pub trait RuntimeSpawner: Send + Sync {
    /// Spawn `task` in the background. The returned handle is used by
    /// [`Self::cancel`] to request cancellation.
    async fn spawn(
        &self,
        name: &str,
        task: Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    ) -> Result<BackgroundTaskHandle, RuntimeError>;

    /// Asynchronously sleep for `duration`.
    async fn sleep(&self, duration: Duration);

    /// Request cancellation of a previously-spawned task. Cooperative —
    /// implementations should drop the future at the next await point rather
    /// than hard-killing the OS thread.
    async fn cancel(&self, handle: &BackgroundTaskHandle) -> Result<(), RuntimeError>;
}

/// Failure modes for [`RuntimeSpawner`] calls.
#[derive(Debug, Clone, Error)]
pub enum RuntimeError {
    /// Spawner is shutting down and is no longer accepting new tasks.
    #[error("runtime is shutting down")]
    ShuttingDown,
    /// No task is registered under the supplied handle.
    #[error("background task {0} not found")]
    NotFound(String),
    /// Catch-all for spawner-internal errors.
    #[error("spawner internal error: {0}")]
    Internal(String),
}
