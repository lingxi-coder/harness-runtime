//! Queue operation log entries — replayed during crash recovery so the
//! engine can rebuild the queue state after an unclean shutdown.

use crate::queue::{QueuePriority, QueueSource};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

/// One entry in the queue's append-only operation log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum QueueOperation {
    /// Command added to the queue.
    Enqueue {
        /// Command UUID.
        uuid: String,
        /// Priority used at enqueue time.
        priority: QueuePriority,
        /// Originating subsystem.
        source: QueueSource,
    },
    /// Command popped from the queue.
    Dequeue {
        /// UUID of the dequeued command.
        uuid: String,
    },
    /// Command explicitly removed (e.g. user cancellation).
    Remove {
        /// UUID of the removed command.
        uuid: String,
        /// Reason for removal.
        reason: String,
    },
    /// Queue cleared.
    Clear {
        /// Number of items dropped.
        count: usize,
    },
}

/// Sink for [`QueueOperation`]s, wired into
/// [`crate::MessageQueueManager::set_recorder`]. The twin of claude-code's
/// `recordQueueOperation` / `logOperation`: every queue mutation is appended
/// here so the operation log that backs crash-recovery replay is produced.
///
/// The default queue has no recorder (operations log to nowhere); an embedder
/// that wants the recovery log installs one.
#[async_trait]
pub trait QueueOperationRecorder: Send + Sync {
    /// Append one operation to the log.
    async fn record(&self, op: QueueOperation);
}

/// An in-memory [`QueueOperationRecorder`] that accumulates operations in a
/// `Vec`. Used by tests (and as a simple default for embedders that want the
/// log resident in memory).
#[derive(Default)]
pub struct VecRecorder {
    ops: RwLock<Vec<QueueOperation>>,
}

impl VecRecorder {
    /// Construct an empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the recorded operations so far.
    pub async fn ops(&self) -> Vec<QueueOperation> {
        self.ops.read().await.clone()
    }
}

#[async_trait]
impl QueueOperationRecorder for VecRecorder {
    async fn record(&self, op: QueueOperation) {
        self.ops.write().await.push(op);
    }
}
