//! `/usage` — render the current session's flat usage/cost snapshot.
//!
//! The interactive TUI Usage tab reads the same `OrchestratorHandle::snapshot_cost`
//! seam. This handler covers headless slash-dispatch paths without opening the
//! settings screen.

use async_trait::async_trait;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;
use platform_api::{CostSnapshot, OrchestratorHandle};
use std::sync::Arc;

const DESCRIPTION: &str = "Show current session usage";

/// `/usage` handler backed by the orchestrator's cumulative cost snapshot.
#[derive(Clone)]
pub struct UsageHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl UsageHandler {
    /// Construct a `UsageHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for UsageHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let cost = self.handle.snapshot_cost().await;
        CommandResult::Done {
            display: Some(render_usage_snapshot(&cost)),
        }
    }

    fn name(&self) -> &str {
        "usage"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }
}

fn render_usage_snapshot(cost: &CostSnapshot) -> String {
    cost::render::cost_summary_from_snapshot(cost)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;
    use std::time::Duration;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "usage".to_string(),
            raw_args: String::new(),
            positional_args: Vec::new(),
        }
    }

    #[tokio::test]
    async fn renders_cost_summary_block() {
        use platform_api::orchestrator::ModelUsageRow;
        let rows = vec![ModelUsageRow {
            model: "claude-opus-4-8".into(),
            provider: None,
            total_nano_usd: 123_400_000,
            input_tokens: 5_000,
            output_tokens: 2_000,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }];
        let snap = CostSnapshot {
            total_usd: 0.1234,
            unknown_models: true,
            api_duration: Duration::from_millis(5_000),
            session_duration: Duration::from_secs(125), // wall = 125_000 ms
            code_lines_added: 10,
            code_lines_removed: 1,
            by_model: rows.clone(),
            ..CostSnapshot::default()
        };
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_cost_snapshot(snap.clone());
        let h = UsageHandler::new(mock);

        let expected = cost::render::cost_summary(&cost::render::CostSummaryInput {
            total_usd: 0.1234,
            unknown_models: true,
            api_duration_ms: 5_000,
            wall_duration_ms: 125_000,
            code_lines_added: 10,
            code_lines_removed: 1,
            by_model: &rows,
        });
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => assert_eq!(s, expected),
            other => panic!("expected display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let h = UsageHandler::new(Arc::new(MockOrchestratorHandle::new()));
        assert_eq!(h.name(), "usage");
        assert_eq!(h.description(), DESCRIPTION);
    }
}
