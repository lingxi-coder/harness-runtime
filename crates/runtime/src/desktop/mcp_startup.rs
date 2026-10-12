//! Folding background startup MCP connects into the live session.
//!
//! With [`super::DesktopConfig::connect_mcp_in_background`] the registry dials
//! every startup server on a detached task (see
//! [`mcp::McpRegistry::begin_background_connects`]). The watcher here re-reads
//! the registry each time a dial settles and replaces the live MCP tool and
//! prompt catalogs, so a server's tools reach the next model request as soon
//! as it connects, including servers served from the discovery cache, which
//! publish no catalog-change notification.

use std::sync::Arc;

use command_api::CommandRegistry;
use tokio::sync::{watch, RwLock};
use tool_api::{BuiltinToolContext, ToolRegistry};

/// Startup MCP progress, published after the live catalogs absorbed the
/// latest settled dial.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct McpStartupProgress {
    /// Startup servers still dialing. Zero once every startup server has
    /// connected, failed, or needs authentication.
    pub pending: usize,
}

/// Report the completed startup discovery round (oracle `Pd`) once, from the
/// catalogs the session actually holds.
pub(super) async fn report_mcp_tools_commands_loaded(
    registry: &mcp::McpRegistry,
    tool_context: BuiltinToolContext,
    bus: &telemetry::AnalyticsBus,
) {
    let tools = tool_mcp::build_registered_mcp_tools(registry, tool_context).await;
    let prompt_commands =
        command_api::mcp_prompts::mcp_prompt_commands(&registry.connected_prompts().await);
    super::emit_mcp_tools_commands_loaded(
        bus,
        &super::mcp_tools_commands_loaded_payload(
            super::registered_mcp_tool_count(&tools),
            &prompt_commands,
        ),
    )
    .await;
}

/// Watch the registry's background dials until every startup server settles.
///
/// The registry is held weakly so the watcher never keeps a dropped runtime's
/// MCP graph alive.
pub(super) fn spawn_startup_watcher(
    registry: &Arc<mcp::McpRegistry>,
    tools: Arc<ToolRegistry>,
    tool_context: BuiltinToolContext,
    commands: Arc<RwLock<CommandRegistry>>,
    bus: Arc<telemetry::AnalyticsBus>,
    progress: watch::Sender<McpStartupProgress>,
) -> tokio::task::JoinHandle<()> {
    let mut inflight = registry.watch_background_connects();
    let registry = Arc::downgrade(registry);
    tokio::spawn(async move {
        loop {
            let pending = *inflight.borrow_and_update();
            let Some(live) = registry.upgrade() else {
                break;
            };
            let registered =
                tool_mcp::build_registered_mcp_tools(live.as_ref(), tool_context.clone()).await;
            tools.replace_mcp_tools(registered);
            super::assembly::reconcile_mcp_prompt_catalog(live.as_ref(), &commands).await;
            if pending == 0 {
                report_mcp_tools_commands_loaded(live.as_ref(), tool_context.clone(), &bus).await;
            }
            drop(live);
            progress.send_replace(McpStartupProgress { pending });
            if pending == 0 || inflight.changed().await.is_err() {
                break;
            }
        }
    })
}

/// Aborts the background startup dials if assembly fails before the watcher
/// takes them over, so a runtime that never assembled leaves no dial running.
pub(super) struct StartupDialsGuard(Option<Arc<mcp::McpRegistry>>);

impl StartupDialsGuard {
    pub(super) fn new(registry: Arc<mcp::McpRegistry>) -> Self {
        Self(Some(registry))
    }

    /// The runtime assembled: its lifecycle owns the dials from here on.
    pub(super) fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StartupDialsGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.0.take() {
            registry.abort_background_connects();
        }
    }
}
