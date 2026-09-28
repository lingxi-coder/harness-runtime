//! `/mcp` — list registered MCP servers + their status.
//!
//! Locked display template (`LingXi` UX, M5-11 T0 step 2 L6):
//!   `"MCP servers ({count}):\n  {name}  {status}  {transport}\n…"`
//! Failure prefix: `"Could not list MCP servers: "` (currently unreachable —
//! `list_mcp_servers` is infallible).

use crate::builtin_support::list_render::render_list;
use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::{McpServerInfo, McpStatus, OrchestratorHandle};
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/mcp` handler — list mode.
#[derive(Clone)]
pub struct McpHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl McpHandler {
    /// Construct a `McpHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for McpHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::MCP_STARTED);
        let servers = self.handle.list_mcp_servers().await;
        let rows: Vec<String> = servers.iter().map(format_row).collect();
        let s = render_list(
            "MCP servers",
            rows,
            "No MCP servers configured. Use `claude mcp add` to add a server.",
        );
        telemetry::emit_command_completed(cmd_evt::MCP_COMPLETED, "");
        CommandResult::Done { display: Some(s) }
    }
    fn name(&self) -> &str {
        "mcp"
    }
    fn description(&self) -> &str {
        core_description("mcp")
    }
}

fn format_row(s: &McpServerInfo) -> String {
    let status = match &s.status {
        McpStatus::Connected => "connected".to_string(),
        McpStatus::Disconnected => "disconnected".to_string(),
        McpStatus::Error(e) => format!("error: {e}"),
    };
    format!("{}  {}  {}", s.name, status, s.transport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "mcp".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn empty_list() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = McpHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "No MCP servers configured. Use `claude mcp add` to add a server.\n"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn two_servers_with_mixed_status() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_mcp_servers(vec![
            McpServerInfo {
                name: "memory".into(),
                status: McpStatus::Connected,
                transport: "stdio".into(),
            },
            McpServerInfo {
                name: "filesystem".into(),
                status: McpStatus::Error("connection refused".into()),
                transport: "stdio".into(),
            },
        ]);
        let h = McpHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "MCP servers (2):\n  memory  connected  stdio\n  filesystem  error: connection refused  stdio\n"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = McpHandler::new(mock);
        assert_eq!(h.name(), "mcp");
        assert_eq!(h.description(), "Manage MCP servers");
    }
}
