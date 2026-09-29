//! Parallel-spawn coordinator built on top of [`crate::pool::StateMachinePool`].
//!
//! [`MultiAgentDispatcher`] takes a batch of spawn specs and submits them
//! one-by-one to the underlying pool, returning the receiver streams in the
//! original order so the caller can fan out without rolling its own
//! bookkeeping. See spec §10.6.

use crate::context::SubagentContext;
use crate::definition::AgentDefinition;
use crate::pool::{PoolError, StateMachinePool};
use crate::runner::SubagentEvent;
use lingxi_core::types::AgentId;
use std::sync::Arc;
use tokio::sync::mpsc;

/// Description of one agent the dispatcher should spawn.
#[derive(Debug, Clone)]
pub struct MultiAgentSpawnSpec {
    /// Agent type to spawn (must match a known [`AgentDefinition::agent_type`]).
    pub agent_type: String,
    /// Display name for this particular spawn.
    pub name: String,
    /// Initial prompt to seed the agent with.
    pub initial_prompt: String,
    /// Optional ad-hoc definition override, used by ephemeral specs that
    /// don't have a corresponding entry in the registry.
    pub agent_def_override: Option<AgentDefinition>,
}

/// Coordinator that fans out parallel agent spawns over a shared pool.
pub struct MultiAgentDispatcher {
    pool: Arc<StateMachinePool>,
}

impl MultiAgentDispatcher {
    /// Construct a dispatcher backed by `pool`.
    #[must_use]
    pub fn new(pool: Arc<StateMachinePool>) -> Self {
        Self { pool }
    }

    /// Spawn each agent in `specs` (paired one-to-one with `contexts`) and
    /// return a vector of `(agent_id, event_rx)` pairs in the same order.
    ///
    /// The `_coordinator` parameter is reserved for the coordinator-mode
    /// permission filter shipping in Plan 07.
    pub async fn spawn_multi(
        &self,
        specs: Vec<MultiAgentSpawnSpec>,
        _coordinator: AgentId,
        contexts: Vec<SubagentContext>,
    ) -> Result<Vec<(AgentId, mpsc::Receiver<SubagentEvent>)>, PoolError> {
        let mut out = Vec::with_capacity(specs.len());
        for ctx in contexts {
            out.push(self.pool.allocate(ctx).await?);
        }
        Ok(out)
    }
}
