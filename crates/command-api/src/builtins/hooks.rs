//! `/hooks` — list registered hooks.
//!
//! Locked display template (`LingXi` UX, M5-11 T0 step 2 L7):
//!   `"Hooks ({count}):\n  {name}  {event}  {timeout_ms}ms\n…"`
//! Failure prefix: `"Could not list hooks: "` (currently unreachable —
//! `list_hooks` is infallible).

use crate::builtin_support::list_render::render_list;
use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::{HookInfo, OrchestratorHandle};
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/hooks` handler — list mode.
#[derive(Clone)]
pub struct HooksHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl HooksHandler {
    /// Construct a `HooksHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for HooksHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::HOOKS_STARTED);
        let hooks = self.handle.list_hooks().await;
        let rows: Vec<String> = hooks.iter().map(format_row).collect();
        let s = render_list("Hooks", rows, "No hooks configured");
        telemetry::emit_command_completed(cmd_evt::HOOKS_COMPLETED, "");
        CommandResult::Done { display: Some(s) }
    }
    fn name(&self) -> &str {
        "hooks"
    }
    fn description(&self) -> &str {
        core_description("hooks")
    }
}

fn format_row(h: &HookInfo) -> String {
    format!("{}  {}  {}ms", h.name, h.event, h.timeout_ms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "hooks".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn empty_list() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = HooksHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "No hooks configured\n");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn two_hooks() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_hooks(vec![
            HookInfo {
                name: "fmt".into(),
                event: "PostToolUse".into(),
                matcher: Some("Write|Edit".into()),
                timeout_ms: 60_000,
                ..HookInfo::default()
            },
            HookInfo {
                name: "lint".into(),
                event: "Stop".into(),
                matcher: None,
                timeout_ms: 30_000,
                ..HookInfo::default()
            },
        ]);
        let h = HooksHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "Hooks (2):\n  fmt  PostToolUse  60000ms\n  lint  Stop  30000ms\n"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = HooksHandler::new(mock);
        assert_eq!(h.name(), "hooks");
        assert_eq!(h.description(), "View hook configurations for tool events");
    }
}
