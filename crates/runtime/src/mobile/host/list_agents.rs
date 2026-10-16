//! Mobile producer for the current-session `/list-agents` roster.
//!
//! Mobile owns the live task and agent-name stores but has no coordinator or
//! current team identity. Reuse the same validated local-agent projection as
//! Desktop, leave teammate rows empty, and preserve Native's existing warning
//! because cross-session peer discovery is not wired here.

use crate::list_agents_support::{local_subagent_rows, reverse_agent_names};
use async_trait::async_trait;
use command_api::builtins::list_agents::{
    ListAgentsCommandAvailability, ListAgentsProvider, LocalAgentRoster,
};
use lingxi_core::host::agent_name_registry::AgentNameRegistry;
use lingxi_core::host::task_registry::{TaskListFilter, TaskRegistryHandle};
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) struct MobileListAgentsProvider {
    tasks: Arc<dyn TaskRegistryHandle>,
    agent_names: Arc<dyn AgentNameRegistry>,
    availability: ListAgentsCommandAvailability,
}

impl MobileListAgentsProvider {
    pub(crate) fn new(
        tasks: Arc<dyn TaskRegistryHandle>,
        agent_names: Arc<dyn AgentNameRegistry>,
        availability: ListAgentsCommandAvailability,
    ) -> Self {
        Self {
            tasks,
            agent_names,
            availability,
        }
    }
}

#[async_trait]
impl ListAgentsProvider for MobileListAgentsProvider {
    fn command_availability(&self) -> ListAgentsCommandAvailability {
        self.availability
    }

    async fn snapshot_local_roster(&self) -> Result<LocalAgentRoster, Utf16JsonProjection> {
        let tasks = self
            .tasks
            .list(TaskListFilter::default())
            .await
            .map_err(|error| {
                Utf16JsonProjection::from(format!("task registry read failed: {error}"))
            })?;
        let names_by_agent_id = reverse_agent_names(&self.agent_names.list().await);
        let subagents = local_subagent_rows(&tasks, &names_by_agent_id, &HashSet::new())?;

        Ok(LocalAgentRoster {
            subagents,
            // Mobile does not construct or infer a coordinator team roster.
            teammates: Vec::new(),
            // Native's existing warning explains that the omitted cross-session
            // portion may be missing; no additional mobile-only warning is added.
            local_list_failed: true,
        })
    }
}
