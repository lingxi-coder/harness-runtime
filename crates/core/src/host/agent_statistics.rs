//! Ephemeral, session-owned statistics for launches admitted by the Agent tool.
//! A token follows the existing execution owner; other agent entrypoints never
//! mint one. Tokens retain their originating authority across session switches.

use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentRequestedStatistics {
    pub background: u64,
    pub foreground: u64,
    pub unset: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentKilledStatistics {
    pub parent: u64,
    pub user: u64,
    pub system: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentRefusedStatistics {
    pub depth_limit: u64,
    pub concurrency_limit: u64,
    pub budget: u64,
}

/// Snapshot field order and agent-type insertion order match the local Agent
/// result contract. This is not persisted or restored from a transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AgentSessionStatisticsSnapshot {
    pub spawned: u64,
    pub requested: AgentRequestedStatistics,
    pub started_in_background: u64,
    pub max_depth: u32,
    pub spawned_by_subagents: u64,
    pub completed: i64,
    pub failed: u64,
    pub killed: AgentKilledStatistics,
    pub refused: AgentRefusedStatistics,
    #[serde(serialize_with = "serialize_by_type")]
    pub by_type: Vec<(String, u64)>,
}

fn serialize_by_type<S: serde::Serializer>(
    entries: &[(String, u64)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = serializer.serialize_map(Some(entries.len()))?;
    for (name, count) in entries {
        map.serialize_entry(name, count)?;
    }
    map.end()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentSpawnRefusal {
    DepthLimit,
    ConcurrencyLimit,
    Budget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentKillReason {
    Parent,
    User,
    System,
}

impl AgentKillReason {
    pub fn from_task_reason(reason: &str) -> Self {
        match reason {
            "parent" => Self::Parent,
            "system" | "budget" | "budget_exceeded" | "memory_pressure" => Self::System,
            _ => Self::User,
        }
    }
}

#[derive(Debug, Default)]
pub struct AgentSessionStatistics(Mutex<AgentSessionStatisticsSnapshot>);

impl AgentSessionStatistics {
    pub fn snapshot(&self) -> AgentSessionStatisticsSnapshot {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// `/clear` resets the same authority. A child still running may subsequently
    /// finish, so terminal counts may exceed the post-clear spawn count.
    pub fn reset(&self) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Default::default();
    }

    pub fn record_refused(&self, reason: AgentSpawnRefusal) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reason {
            AgentSpawnRefusal::DepthLimit => state.refused.depth_limit += 1,
            AgentSpawnRefusal::ConcurrencyLimit => state.refused.concurrency_limit += 1,
            AgentSpawnRefusal::Budget => state.refused.budget += 1,
        }
    }

    /// Admission is not a spawn: only the execution owner's `started` receipt
    /// increments counters. Failed preparation therefore cannot claim a launch.
    pub fn prepare_spawn(
        self: &Arc<Self>,
        agent_type: String,
        requested_background: Option<bool>,
        started_in_background: bool,
        depth: u32,
    ) -> AgentSpawnToken {
        AgentSpawnToken(Arc::new(SpawnState {
            statistics: self.clone(),
            agent_type,
            requested_background,
            started_in_background,
            depth,
            lifecycle: Mutex::new(SpawnLifecycle::Pending),
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpawnLifecycle {
    Pending,
    Started,
    Completed,
    Failed,
    Killed,
    Refused,
}

#[derive(Debug)]
struct SpawnState {
    statistics: Arc<AgentSessionStatistics>,
    agent_type: String,
    requested_background: Option<bool>,
    started_in_background: bool,
    depth: u32,
    lifecycle: Mutex<SpawnLifecycle>,
}

/// Clones share one start/terminal receipt. This host-only capability is never
/// serialized into tool input or durable task state.
#[derive(Debug, Clone)]
pub struct AgentSpawnToken(Arc<SpawnState>);

impl PartialEq for AgentSpawnToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for AgentSpawnToken {}

impl AgentSpawnToken {
    pub fn statistics(&self) -> Arc<AgentSessionStatistics> {
        self.0.statistics.clone()
    }

    pub fn refused(&self, reason: AgentSpawnRefusal) {
        let mut lifecycle = self
            .0
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle != SpawnLifecycle::Pending {
            return;
        }
        *lifecycle = SpawnLifecycle::Refused;
        self.0.statistics.record_refused(reason);
    }

    pub fn started(&self) {
        self.release_start(|| true);
    }

    /// Linearize the accepted startup gate and its receipt against an immediate
    /// runner terminal on another thread. A rejected gate never counts a spawn.
    pub fn release_start(&self, release: impl FnOnce() -> bool) -> bool {
        let mut lifecycle = self
            .0
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let released = release();
        if !released || *lifecycle != SpawnLifecycle::Pending {
            return released;
        }
        let mut state = self
            .0
            .statistics
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *lifecycle = SpawnLifecycle::Started;
        state.spawned += 1;
        match self.0.requested_background {
            Some(true) => state.requested.background += 1,
            Some(false) => state.requested.foreground += 1,
            None => state.requested.unset += 1,
        }
        state.started_in_background += u64::from(self.0.started_in_background);
        state.max_depth = state.max_depth.max(self.0.depth);
        state.spawned_by_subagents += u64::from(self.0.depth > 1);
        if let Some((_, count)) = state
            .by_type
            .iter_mut()
            .find(|(name, _)| name == &self.0.agent_type)
        {
            *count += 1;
        } else {
            state.by_type.push((self.0.agent_type.clone(), 1));
        }
        true
    }

    pub fn completed(&self) {
        self.finish(SpawnLifecycle::Completed, None, false);
    }
    pub fn failed(&self) {
        self.finish(SpawnLifecycle::Failed, None, false);
    }
    pub fn killed(&self, reason: AgentKillReason) {
        self.finish(SpawnLifecycle::Killed, Some(reason), false);
    }
    pub fn cancelled_after_completion(&self) {
        self.finish(SpawnLifecycle::Killed, Some(AgentKillReason::User), true);
    }

    fn finish(
        &self,
        next: SpawnLifecycle,
        killed: Option<AgentKillReason>,
        retract_completion: bool,
    ) {
        let mut lifecycle = self
            .0
            .lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle != SpawnLifecycle::Started
            && !(retract_completion && *lifecycle == SpawnLifecycle::Completed)
        {
            return;
        }
        let mut state = self
            .0
            .statistics
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *lifecycle == SpawnLifecycle::Completed {
            state.completed -= 1;
        }
        *lifecycle = next;
        match next {
            SpawnLifecycle::Completed => state.completed += 1,
            SpawnLifecycle::Failed => state.failed += 1,
            SpawnLifecycle::Killed => match killed.unwrap_or(AgentKillReason::User) {
                AgentKillReason::Parent => state.killed.parent += 1,
                AgentKillReason::User => state.killed.user += 1,
                AgentKillReason::System => state.killed.system += 1,
            },
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_and_terminal_receipts_are_once_only_across_owner_clones() {
        let stats = Arc::new(AgentSessionStatistics::default());
        let rejected = stats.prepare_spawn("missing".into(), None, false, 1);
        rejected.failed();
        assert_eq!(stats.snapshot().spawned, 0);
        stats.record_refused(AgentSpawnRefusal::ConcurrencyLimit);
        let foreground = stats.prepare_spawn("Explore".into(), Some(false), false, 1);
        let background = stats.prepare_spawn("worker".into(), None, true, 2);
        foreground.started();
        background.started();
        foreground.clone().started();
        foreground.completed();
        foreground.failed();
        background.killed(AgentKillReason::Parent);
        background.completed();
        foreground.cancelled_after_completion();
        foreground.cancelled_after_completion();
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.spawned, 2);
        assert_eq!(snapshot.completed, 0);
        assert_eq!(snapshot.failed, 0);
        assert_eq!(
            snapshot.killed,
            AgentKilledStatistics {
                parent: 1,
                user: 1,
                system: 0
            }
        );
        assert_eq!(
            snapshot.requested,
            AgentRequestedStatistics {
                background: 0,
                foreground: 1,
                unset: 1
            }
        );
        assert_eq!(snapshot.started_in_background, 1);
        assert_eq!(snapshot.spawned_by_subagents, 1);
        assert_eq!(snapshot.max_depth, 2);
        assert_eq!(
            snapshot.by_type,
            vec![("Explore".into(), 1), ("worker".into(), 1)]
        );
        assert_eq!(snapshot.refused.concurrency_limit, 1);
    }

    #[test]
    fn clear_and_new_session_keep_live_tokens_with_original_authority() {
        let old = Arc::new(AgentSessionStatistics::default());
        let token = old.prepare_spawn("worker".into(), Some(true), true, 1);
        token.started();
        old.reset();
        let resumed = Arc::new(AgentSessionStatistics::default());
        token.completed();
        assert_eq!(old.snapshot().spawned, 0);
        assert_eq!(old.snapshot().completed, 1);
        assert_eq!(
            resumed.snapshot(),
            AgentSessionStatisticsSnapshot::default()
        );
        assert!(serde_json::to_string(&old.snapshot())
            .unwrap()
            .ends_with("\"by_type\":{}}"));
    }
}
