//! §24b — per-subagent inline `mcpServers` tool building + teardown seam.
//!
//! Port of claude `Agr` (2.1.251 @~160977000): given a resolved subagent
//! spawn's [`crate::definition::AgentDefinition`], connect its frontmatter
//! `mcpServers` and build the per-tool wire entries the child's advertised
//! tool pool should carry, then tear down exactly the connections THIS spawn
//! newly created when it exits (never a by-name REUSED connection — see
//! [`crate::mcp_servers::ScopedAgentMcpServer::is_newly_created`]).
//!
//! `agent` cannot build these tools itself: a real per-tool `MCPTool`
//! (`tool_mcp` crate) dispatches through a live `tool_api::BuiltinToolContext`
//! (analytics bus, provider, task registry, …) that only the composition root
//! constructs — the same reason [`crate::handle::PoolSubagentSpawner`]
//! injects its `hook_executor`/`skill_loader` via a filled-after-construction
//! cell instead of owning them outright. [`AgentMcpToolBuilder`] is that same
//! pattern applied to MCP: the composition root fills
//! [`crate::handle::PoolSubagentSpawner::mcp_tool_builder_handle`] with a
//! closure that owns the `Arc<mcp::McpRegistry>` + `BuiltinToolContext` and
//! does the actual connect + `tool_mcp::MCPTool::new_for_tool(..).with_bound_server_key(..)`
//! construction; unfilled (tests / minimal builds) is a strict no-op — the
//! subagent's tool pool is byte-identical to before this feature.

use crate::definition::AgentDefinition;
use protocol::AgentId;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tool_api::tool_trait::Tool;

/// One connection this subagent spawn newly created (claude `Agr`'s
/// `isNewlyCreated:true` client, i.e. an inline RECORD `mcpServers` entry —
/// never a by-name REUSED one). Torn down exactly once when the spawn ends,
/// regardless of how it ends (claude tears down in `runAgent`'s `finally`).
///
/// `server_name` is the PLAIN (unscoped) server name, carried only so a
/// teardown failure can be logged with claude's exact copy: `"[Agent: {type}]
/// Error cleaning up MCP server '{server_name}': {err}"`.
#[derive(Clone)]
pub struct AgentMcpCleanupHandle {
    /// Plain server name, for the teardown-failure log line only.
    pub server_name: String,
    /// Runs the actual teardown (a plain transport close — see
    /// `mcp::registry::McpRegistry::disconnect_agent_scoped`'s doc for why
    /// this must never revoke OAuth tokens). `Err` carries a display-ready
    /// message for the log line above; never fatal to the caller.
    #[allow(clippy::type_complexity)]
    pub run:
        Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<(), String>> + Send>> + Send + Sync>,
}

/// The tools + teardown handles built for one subagent spawn's per-agent MCP
/// servers (claude `Agr`'s return `{tools, cleanup}` — `clients`/`agentClients`
/// have no port-side equivalent since dispatch here is registry-key-based, not
/// a held client reference).
#[derive(Clone, Default)]
pub struct AgentMcpToolSet {
    /// Per-tool wire entries (claude `Fe`) to append to the resolved pool —
    /// [`crate::tool_resolver::AgentToolResolver::resolve`]'s `agent_mcp_tools`
    /// parameter.
    pub tools: Vec<Arc<dyn Tool>>,
    /// Newly-created connections to tear down on spawn exit (claude `U`/the
    /// `cleanup` closure's loop). Empty when `mcp_servers` was empty, every
    /// entry was by-name-reused, or no builder is wired.
    pub cleanups: Vec<AgentMcpCleanupHandle>,
}

/// Identity reservation held until construction and any cancellation cleanup finish.
/// Builders must move this lease into asynchronous cleanup when cancelled before returning.
pub type AgentMcpConstructionLease = Arc<dyn Send + Sync>;

/// Injected per-spawn agent-scoped MCP tool builder (claude `Agr`). Takes the
/// FINAL resolved [`AgentId`] + [`AgentDefinition`] for one spawn (owned, so
/// the returned future is `'static`) and returns the built tool set.
///
/// Filled via [`crate::handle::PoolSubagentSpawner::mcp_tool_builder_handle`]
/// AFTER the composition root's `Arc<mcp::McpRegistry>` + builtin
/// `BuiltinToolContext` exist (the same construction-order cycle-break as
/// `hook_executor`/`skill_loader` — see that field's doc in `handle.rs`).
#[allow(clippy::type_complexity)]
pub type AgentMcpToolBuilder = Arc<
    dyn Fn(
            AgentId,
            AgentDefinition,
            Option<AgentMcpConstructionLease>,
        ) -> Pin<Box<dyn Future<Output = AgentMcpToolSet> + Send>>
        + Send
        + Sync,
>;

/// Run every cleanup in `cleanups`, logging claude's exact
/// `"[Agent: {agent_type}] Error cleaning up MCP server '{name}': {err}"` on a
/// failure (never fatal — a subagent's cleanup failure must not fail the
/// spawn that already completed). Runs sequentially, matching claude's
/// `for(let z of U) ... await z.cleanup()` loop.
pub async fn run_agent_mcp_cleanups(cleanups: Vec<AgentMcpCleanupHandle>, agent_type: &str) {
    for cleanup in cleanups {
        if let Err(err) = (cleanup.run)().await {
            tracing::warn!(
                "[Agent: {}] Error cleaning up MCP server '{}': {}",
                agent_type,
                cleanup.server_name,
                err
            );
        }
    }
}
