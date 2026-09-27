//! Cron-style scheduled task registry.
//!
//! Stub registry for cron definitions; the actual cron scheduler (parsing
//! schedules, computing next run, dispatching) lands in Plan 11.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::SystemTime;

/// Definition of a scheduled (cron) task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronTaskDef {
    /// Stable cron task identifier.
    pub id: String,
    /// Cron schedule string (parsing handled in Plan 11).
    pub schedule: String,
    /// Prompt to feed to the spawned agent or task.
    pub prompt: String,
    /// Optional agent type override.
    pub agent_type: Option<String>,
    /// Last successful run timestamp.
    pub last_run: Option<SystemTime>,
    /// Whether the cron job is currently enabled.
    pub enabled: bool,
}

/// In-memory registry of cron definitions.
pub struct CronTaskRegistry {
    tasks: Mutex<HashMap<String, CronTaskDef>>,
}

impl CronTaskRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// Register a cron task definition.
    #[allow(clippy::unwrap_used)]
    pub fn register(&self, def: CronTaskDef) {
        self.tasks.lock().unwrap().insert(def.id.clone(), def);
    }

    /// Remove a cron task definition by ID.
    #[allow(clippy::unwrap_used)]
    pub fn unregister(&self, id: &str) {
        self.tasks.lock().unwrap().remove(id);
    }

    /// Return the cron tasks due to run at `_now`. Stub returns empty.
    #[must_use]
    pub fn find_due(&self, _now: SystemTime) -> Vec<CronTaskDef> {
        // Stub: full schedule parsing in Plan 11.
        Vec::new()
    }
}

impl Default for CronTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}
