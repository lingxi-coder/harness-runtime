//! `/exit` — sets the orchestrator's `should_exit` flag and emits the
//! locked confirmation literal.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 6.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// The locked confirmation literal `/exit` and `/quit` render.
///
/// Exported because the CLI REPL renders it from a second place: `/exit` is
/// recognised and short-circuited BEFORE dispatch so the host can offer
/// stay/stop/handoff while the one-way `should_exit` flag is still unset, so
/// on that path this handler never runs. One `const` rather than two string
/// literals keeps the two renderers from drifting apart.
pub const EXIT_DISPLAY: &str = "Exiting.";

/// `/exit` handler — calls
/// [`OrchestratorHandle::request_exit`](lingxi_core::host::OrchestratorHandle::request_exit)
/// and renders `"Exiting."`.
///
/// `request_exit` is infallible — the `_failed` telemetry slot is reserved
/// (registered in [`cmd_evt::EXIT_FAILED`]) but currently unreachable.
#[derive(Clone)]
pub struct ExitHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ExitHandler {
    /// Construct an `ExitHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ExitHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::EXIT_STARTED);
        self.handle.request_exit().await;
        telemetry::emit_command_completed(cmd_evt::EXIT_COMPLETED, "");
        CommandResult::Done {
            display: Some(EXIT_DISPLAY.to_string()),
        }
    }

    fn name(&self) -> &str {
        "exit"
    }

    /// `/exit` description.
    ///
    /// Byte-faithful to claude-code v2.1.183's dynamic getter
    /// `wkl(){return _i()?"Detach from this background session (it keeps
    /// running)":"Exit the CLI"}`. The Rust core has no background-session
    /// predicate at this call site, so we return the non-background (default)
    /// branch `"Exit the CLI"`. The background branch
    /// `"Detach from this background session (it keeps running)"` is deferred
    /// until a background-session predicate is threaded here.
    fn description(&self) -> &str {
        "Exit the CLI"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "exit".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_sets_flag_and_returns_locked_literal() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ExitHandler::new(mock.clone());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Exiting.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(mock.was_exit_requested());
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ExitHandler::new(mock);
        assert_eq!(h.name(), "exit");
        // Byte-faithful to claude-code v2.1.183 wkl() default (non-background)
        // branch: "Exit the CLI" (NOT "Exit the REPL").
        assert_eq!(h.description(), "Exit the CLI");
    }
}
