//! Byte-exact context fork (a.k.a. fork agent).
//!
//! The fork path is implemented (codex #5): the model spawns a child that
//! inherits the parent's FULL conversation context + system prompt for a
//! byte-identical prompt-cache prefix. The pure, dependency-free helpers — the
//! feature gate, recursion guard, cache-prefix message builders, and the
//! boilerplate consts — live in the leaf [`lingxi_core::host::fork_subagent`] module so
//! BOTH `tool-agent` (where `AgentTool` builds the forked messages) and this
//! `agent` crate (the spawner / runner) can reach them without a dependency
//! cycle. The synthetic `FORK_AGENT` [`crate::definition::AgentDefinition`]
//! lives in [`crate::builtins::fork_agent_definition`] (it needs the
//! `AgentDefinition` types) and is resolved on the fork path by
//! [`crate::handle::PoolSubagentSpawner::lookup_definition`].
//!
//! This module re-exports the canonical consts from `traits` so any downstream
//! that referenced the `agent::fork::` path stays stable.

pub use lingxi_core::host::fork_subagent::{
    FORK_BOILERPLATE_TAG, FORK_DIRECTIVE_PREFIX, FORK_SUBAGENT_TYPE,
};
