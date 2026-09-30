//! Snapshots of the in-memory state per agent type.

use lingxi_core::types::SnapshotId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::SystemTime;

/// Snapshot of the memory state visible to one agent type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMemorySnapshot {
    /// Unique identifier for this snapshot.
    pub snapshot_id: SnapshotId,
    /// Agent type label (e.g. `main`, `subagent`).
    pub agent_type: String,
    /// Paths included in this snapshot.
    pub included_paths: Vec<PathBuf>,
    /// When the snapshot was captured.
    pub created_at: SystemTime,
}
