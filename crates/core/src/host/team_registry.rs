//! `TeamRegistryHandle` — narrow trait giving the bridge / TUI PULL path a way
//! to read coordinator workers WITHOUT depending on the concrete
//! `coordinator` crate.
//!
//! Mirrors the [`crate::host::task_registry::TaskRegistryHandle`] decoupling pattern:
//! the abstract read seam lives here in `traits`; the concrete impl lives in
//! `coordinator` (lowering `WorkerAgent` → [`WorkerInfo`]). Tests inject an
//! in-memory mock.
//!
//! [`WorkerInfo`] is a plain-old-data projection of the coordinator's
//! `WorkerAgent`, field-shaped to lower 1:1 onto the TUI `WorkerRow` and the
//! `client-protocol` roster DTO (added in T18). `status` is a simplified
//! wire/label string (the same convention as [`crate::host::task_registry::TaskRecord`]
//! `status`) so this crate need not import the concrete `WorkerStatus` enum.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

/// Host-owned team-file activity writer. Query handlers report actual query
/// entry and stop/failure boundaries; enqueueing a mailbox message alone does
/// not imply a teammate is active.
#[async_trait]
pub trait TeamMemberActivityHandle: Send + Sync {
    async fn set_active(
        &self,
        team_name: &str,
        member_name: &str,
        active: bool,
    ) -> Result<(), String>;
}

/// Session-owned leader names for task-list resolution. The upstream store is
/// host-scoped (`ds().taskList`); desktop hosts must not share one mutable name.
static LEADER_TEAM_NAMES: RwLock<std::collections::BTreeMap<String, String>> =
    RwLock::new(std::collections::BTreeMap::new());

/// Register the implicit team's task-list identity, or remove it on teardown.
pub fn set_leader_team_name_for_session(session_id: &str, team_name: Option<&str>) {
    let mut names = LEADER_TEAM_NAMES
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(name) = team_name {
        names.insert(session_id.to_owned(), name.to_owned());
    } else {
        names.remove(session_id);
    }
}

/// Read only the calling session's implicit team identity.
#[must_use]
pub fn leader_team_name_for_session(session_id: &str) -> Option<String> {
    LEADER_TEAM_NAMES
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(session_id)
        .cloned()
}

/// One coordinator worker as surfaced to the bridge / TUI read path.
///
/// Field-shaped to lower 1:1 onto the TUI `WorkerRow`
/// (`agent_id` / `name` / `agent_type` / `status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerInfo {
    /// A submitted plan has not received the leader's decision yet.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_plan_approval: bool,
    /// Worker agent id, stringified (the `AgentId` display / wire form).
    pub agent_id: String,
    /// Agent-type string (e.g. `"explorer"`, `"writer"`).
    pub agent_type: String,
    /// Display name.
    pub name: String,
    /// Simplified status label (e.g. `"idle"`, `"working"`, `"failed"`).
    pub status: String,
}

/// Read-only roster surface used by the bridge poll + TUI live feed.
///
/// Object-safe so consumers can hold an `Arc<dyn TeamRegistryHandle>` and
/// tests can inject a mock returning a fixed roster.
#[async_trait]
pub trait TeamRegistryHandle: Send + Sync {
    /// List all currently registered workers, lowered to [`WorkerInfo`].
    async fn list_workers(&self) -> Vec<WorkerInfo>;

    /// The name of the single team this coordinator is running, if set.
    async fn team_name(&self) -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn TeamRegistryHandle>> = None;
    }

    #[test]
    fn leader_team_names_isolate_sessions_and_teardown() {
        set_leader_team_name_for_session("registry-session-a", Some("alpha"));
        set_leader_team_name_for_session("registry-session-b", Some("beta"));
        assert_eq!(
            leader_team_name_for_session("registry-session-a").as_deref(),
            Some("alpha")
        );
        assert_eq!(
            leader_team_name_for_session("registry-session-b").as_deref(),
            Some("beta")
        );
        assert_eq!(
            leader_team_name_for_session("registry-session-unknown"),
            None
        );
        set_leader_team_name_for_session("registry-session-a", None);
        assert_eq!(leader_team_name_for_session("registry-session-a"), None);
        assert_eq!(
            leader_team_name_for_session("registry-session-b").as_deref(),
            Some("beta")
        );
        set_leader_team_name_for_session("registry-session-b", None);
    }

    #[test]
    fn worker_info_roundtrip_json() {
        let info = WorkerInfo {
            awaiting_plan_approval: false,
            agent_id: "agent:00000000-0000-0000-0000-000000000001".into(),
            agent_type: "explorer".into(),
            name: "alpha".into(),
            status: "working".into(),
        };
        let s = serde_json::to_string(&info).unwrap();
        let back: WorkerInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(info, back);
    }
}
