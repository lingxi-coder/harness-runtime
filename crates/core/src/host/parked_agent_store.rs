//! [`ParkedAgentStore`] — the seam that makes a backgrounded agent restorable
//! after the process that ran it exits.
//!
//! `TaskRegistry` is in-memory, so a parked background agent's LAUNCH
//! configuration — model, cwd, isolation, depth, agent type — died with the
//! process even though its conversation and (for a forked skill) its permission
//! scoping were already on disk. A restarted process could read what an agent
//! had said and had no idea how to start it again.
//!
//! A seam rather than a direct call because the store needs the session
//! directory, which lives at the composition root, while the park/unpark
//! moments are down in the task handler.
//!
//! ## The contract is absence-means-terminal
//!
//! [`park`](ParkedAgentStore::park) is called each time an agent COMES TO REST
//! — the only point a resume can target, because the agent is idle between
//! turn-sets with a complete transcript — and
//! [`unpark`](ParkedAgentStore::unpark) when it reaches a terminal state.
//! A restore therefore relies on the ABSENCE of a record rather than on a
//! status field, so a reader cannot forget to check one and revive a finished
//! agent.

use async_trait::async_trait;

use crate::host::subagent_spawn::SubagentSpawnRequest;

/// Records which background agents are parked and restorable.
#[async_trait]
pub trait ParkedAgentStore: Send + Sync {
    /// Record `agent_id` as parked, with everything a rebuild needs.
    ///
    /// Called on every rest, not only the first: the launch configuration does
    /// not change, but re-writing keeps the record's presence tied to the
    /// agent's liveness rather than to a single moment early in its life.
    async fn park(
        &self,
        task_id: &str,
        agent_id: crate::types::AgentId,
        description: &str,
        request: &SubagentSpawnRequest,
    );

    /// Forget `agent_id` — it reached a terminal state and must never be
    /// restored.
    async fn unpark(&self, agent_id: crate::types::AgentId);
}
