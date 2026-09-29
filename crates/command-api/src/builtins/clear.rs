//! `/clear` — wipes the orchestrator's in-memory session and emits a
//! locked confirmation literal.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 3.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/clear` handler — wipes the in-memory conversation.
///
/// Calls [`OrchestratorHandle::clear_session`] and renders either the locked
/// success literal `"Conversation cleared."` or the locked failure prefix
/// `"Could not clear conversation: {error}"`.
#[derive(Clone)]
pub struct ClearHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ClearHandler {
    /// Construct a `ClearHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ClearHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::CLEAR_STARTED);
        match self.handle.clear_session().await {
            Ok(()) => {
                telemetry::emit_command_completed(cmd_evt::CLEAR_COMPLETED, "");
                CommandResult::Done {
                    display: Some("Conversation cleared.".to_string()),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::CLEAR_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not clear conversation: {msg}")),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "clear"
    }

    fn description(&self) -> &str {
        core_description("clear")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "clear".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_returns_locked_literal() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ClearHandler::new(mock.clone());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Conversation cleared.");
            }
            other => panic!("expected Done, got {other:?}"),
        }
        assert!(mock.was_clear_session_called());
    }

    #[tokio::test]
    async fn failure_prefixes_handle_error() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_clear_session_error("disk full".to_string());
        let h = ClearHandler::new(mock.clone());
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Could not clear conversation: handle action failed: disk full"
                );
            }
            other => panic!("expected Done with error display, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ClearHandler::new(mock);
        assert_eq!(h.name(), "clear");
        assert_eq!(
            h.description(),
            "Start a new session with empty context; previous session stays on disk (resumable with /resume)"
        );
    }
}
