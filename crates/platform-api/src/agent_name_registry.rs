//! `AgentNameRegistry` — name → [`protocol::AgentId`] map for `SendMessage`
//! routing of spawned (async) subagents.
//!
//! Port of claude-code's `AppState.agentNameRegistry` (`Map<string, AgentId>`,
//! `AgentTool.tsx:704-711`): when an ASYNC agent is spawned with a `name`, the
//! tool registers `name → agentId` so a later `SendMessage({ to: name })` can
//! resolve the running agent. Sync agents are NOT registered (claude comment
//! `AgentTool.tsx:700-702`: the coordinator is blocked for sync agents, so
//! `SendMessage` routing does not apply).
//!
//! This is a SEPARATE namespace from the coordinator's teammate roster
//! ([`crate::team_registry`] / coordinator `TeamRegistry::find_by_name`), which
//! already resolves teammate names. claude likewise keeps a separate
//! `agentNameRegistry` Map distinct from the team roster; a `SendMessage`
//! resolver that wants full parity checks the teammate roster first, then this
//! registry (documented lookup order — see the step report).
//!
//! Object-safe so a `Arc<dyn AgentNameRegistry>` can ride on
//! `tool_api::BuiltinToolContext`.

use async_trait::async_trait;
use protocol::AgentId;
use std::collections::HashMap;
use std::sync::RwLock;

/// Name → agent-id registry for spawned async subagents (claude
/// `AppState.agentNameRegistry`).
#[async_trait]
pub trait AgentNameRegistry: Send + Sync {
    /// Register `name → agent_id` (claude `next.set(name, asAgentId(agentId))`,
    /// `AgentTool.tsx:706`). A later registration of the same name overwrites
    /// (claude `Map.set` semantics).
    async fn register(&self, name: &str, agent_id: AgentId);

    /// Resolve a previously-registered name to its agent id, or `None`.
    async fn resolve(&self, name: &str) -> Option<AgentId>;

    /// Remove a name's mapping (used on agent teardown so the name can be
    /// reused; claude rebuilds the Map without the entry).
    async fn unregister(&self, name: &str);

    /// Enumerate the current `name -> agent_id` mappings. Default empty so
    /// older hosts remain source-compatible until they opt into listing.
    async fn list(&self) -> Vec<(String, AgentId)> {
        Vec::new()
    }
}

/// In-memory [`AgentNameRegistry`] backed by an `RwLock<HashMap>`. The faithful
/// analog of claude's `AppState.agentNameRegistry` Map; the live wiring (who
/// holds the `Arc`, who reads it for `SendMessage`) is threaded by the host.
#[derive(Default)]
pub struct InMemoryAgentNameRegistry {
    map: RwLock<HashMap<String, AgentId>>,
}

impl InMemoryAgentNameRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl AgentNameRegistry for InMemoryAgentNameRegistry {
    async fn register(&self, name: &str, agent_id: AgentId) {
        self.map
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_string(), agent_id);
    }

    async fn resolve(&self, name: &str) -> Option<AgentId> {
        self.map
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .copied()
    }

    async fn unregister(&self, name: &str) {
        self.map
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }

    async fn list(&self) -> Vec<(String, AgentId)> {
        self.map
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, id)| (name.clone(), *id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn trait_is_object_safe() {
        let _: Option<Arc<dyn AgentNameRegistry>> = None;
    }

    #[tokio::test]
    async fn register_resolve_unregister_round_trip() {
        let reg = InMemoryAgentNameRegistry::new();
        let id = AgentId::new();
        assert_eq!(reg.resolve("worker-a").await, None);

        reg.register("worker-a", id).await;
        assert_eq!(reg.resolve("worker-a").await, Some(id));

        // Re-register overwrites (claude Map.set).
        let id2 = AgentId::new();
        reg.register("worker-a", id2).await;
        assert_eq!(reg.resolve("worker-a").await, Some(id2));

        reg.unregister("worker-a").await;
        assert_eq!(reg.resolve("worker-a").await, None);
    }

    #[tokio::test]
    async fn list_returns_registered_names() {
        let reg = InMemoryAgentNameRegistry::new();
        let a = AgentId::new();
        let b = AgentId::new();
        reg.register("worker-a", a).await;
        reg.register("worker-b", b).await;
        let mut entries = reg.list().await;
        entries.sort_by(|l, r| l.0.cmp(&r.0));
        assert_eq!(
            entries,
            vec![("worker-a".to_string(), a), ("worker-b".to_string(), b)]
        );
    }
}
