//! `/status` — render the 11-line status panel.
//!
//! See plan M5-11 T0 step 3 for the locked layout (header + 10 data rows;
//! column-1 width = 13 chars; row prefix = `"  "`).
//! Failure prefix: `"Could not gather status: "` (currently unreachable —
//! `get_status_snapshot` is infallible).

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::{OrchestratorHandle, StatusSnapshot};
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/status` handler — renders the locked 11-line panel.
#[derive(Clone)]
pub struct StatusHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl StatusHandler {
    /// Construct a `StatusHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for StatusHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::STATUS_STARTED);
        let snap = self.handle.get_status_snapshot().await;
        telemetry::emit_command_completed(cmd_evt::STATUS_COMPLETED, "");
        CommandResult::Done {
            display: Some(render_status(&snap)),
        }
    }
    fn name(&self) -> &str {
        "status"
    }
    fn description(&self) -> &str {
        core_description("status")
    }
}

/// Render the locked 11-line status panel.
#[must_use]
pub fn render_status(s: &StatusSnapshot) -> String {
    let mut out = String::with_capacity(512);
    out.push_str("Status:\n");
    push_row(&mut out, "Session:", &s.session_id);
    push_row(&mut out, "Model:", &s.model);
    push_row(&mut out, "Messages:", &s.n_messages.to_string());
    push_row(&mut out, "Cost:", &format!("${:.4}", s.total_cost_usd));
    push_row(
        &mut out,
        "Tokens:",
        &format!("{}+{}", s.input_tokens, s.output_tokens),
    );
    push_row(
        &mut out,
        "MCP:",
        &format!("{}/{} connected", s.n_mcp_connected, s.n_mcp_total),
    );
    push_row(&mut out, "Hooks:", &format!("{} registered", s.n_hooks));
    push_row(&mut out, "Agents:", &format!("{} available", s.n_agents));
    push_row(&mut out, "Started:", &s.started_at);
    push_row(&mut out, "Working dir:", &s.cwd.display().to_string());
    out
}

fn push_row(out: &mut String, label: &str, value: &str) {
    out.push_str("  ");
    out.push_str(label);
    // Pad label to column-1 width = 13 chars (longest label = "Working dir:" = 12 chars + 1 space).
    let pad = 13_usize.saturating_sub(label.len());
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(value);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "status".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn renders_11_lines_with_locked_layout() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let snap = StatusSnapshot {
            session_id: "abc-123".into(),
            model: "claude-opus-4-7".into(),
            model_profile: None,
            n_messages: 17,
            total_cost_usd: 0.0421,
            input_tokens: 4_500,
            output_tokens: 1_200,
            n_mcp_connected: 1,
            n_mcp_total: 2,
            n_hooks: 3,
            n_agents: 5,
            started_at: "2026-05-26T10:00:00Z".into(),
            cwd: std::path::PathBuf::from("/repo"),
            active_workers: 0,
            setting_sources: Vec::new(),
        };
        mock.set_status_snapshot(snap);
        let h = StatusHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            let expected = "\
Status:
  Session:     abc-123
  Model:       claude-opus-4-7
  Messages:    17
  Cost:        $0.0421
  Tokens:      4500+1200
  MCP:         1/2 connected
  Hooks:       3 registered
  Agents:      5 available
  Started:     2026-05-26T10:00:00Z
  Working dir: /repo
";
            assert_eq!(s, expected);
            assert_eq!(s.matches('\n').count(), 11);
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = StatusHandler::new(mock);
        assert_eq!(h.name(), "status");
        assert_eq!(h.description(), "Show LingXi status including version, model, account, API connectivity, and tool statuses");
    }
}
