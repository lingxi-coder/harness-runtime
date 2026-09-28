//! `/memory` — spawns `$EDITOR` on `<config>/claude/LINGXI.md`.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 7.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/memory` handler — opens the user's memory file in `$EDITOR`.
///
/// Calls
/// [`OrchestratorHandle::open_memory_editor`](platform_api::OrchestratorHandle::open_memory_editor)
/// and renders `"Edited {path} (exit {code})."` on success or the locked
/// `"Could not edit memory: {error}"` prefix on failure.
#[derive(Clone)]
pub struct MemoryHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl MemoryHandler {
    /// Construct a `MemoryHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for MemoryHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::MEMORY_STARTED);
        match self.handle.open_memory_editor().await {
            Ok(outcome) => {
                let details = format!(
                    "{{\"edited_path\":\"{}\",\"exit_code\":{}}}",
                    outcome.edited_path.display(),
                    outcome.exit_code
                );
                telemetry::emit_command_completed(cmd_evt::MEMORY_COMPLETED, &details);
                CommandResult::Done {
                    display: Some(format!(
                        "Edited {} (exit {}).",
                        outcome.edited_path.display(),
                        outcome.exit_code
                    )),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::MEMORY_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not edit memory: {msg}")),
                }
            }
        }
    }

    fn name(&self) -> &str {
        "memory"
    }

    fn description(&self) -> &str {
        core_description("memory")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;
    use std::path::PathBuf;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "memory".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_renders_template_with_path_and_exit_code() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_memory_path(PathBuf::from("/home/u/.config/claude/LINGXI.md"));
        mock.set_editor_exit_code(0);
        let h = MemoryHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Edited /home/u/.config/claude/LINGXI.md (exit 0).");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn nonzero_exit_still_reports_as_edited() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_memory_path(PathBuf::from("/tmp/LINGXI.md"));
        mock.set_editor_exit_code(2);
        let h = MemoryHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "Edited /tmp/LINGXI.md (exit 2).");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn failure_prefixes_handle_error() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_memory_editor_error("EDITOR not found".to_string());
        let h = MemoryHandler::new(mock);
        match h.handle(&args()).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Could not edit memory: handle action failed: EDITOR not found"
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = MemoryHandler::new(mock);
        assert_eq!(h.name(), "memory");
        assert_eq!(h.description(), "Edit LINGXI.md files and memory settings");
    }
}
