//! `/permissions` — opens `$EDITOR` on `<config-dir>/claude/permissions.json`.
//!
//! Locked display template (`LingXi` UX, M5-11 T0 step 2 L5):
//!   `"Edited {path} (exit {code})."`
//! Failure prefix: `"Could not edit permissions: "`.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use platform_api::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/permissions` handler — opens `$EDITOR` on the permissions file.
#[derive(Clone)]
pub struct PermissionsHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl PermissionsHandler {
    /// Construct a `PermissionsHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for PermissionsHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::PERMISSIONS_STARTED);
        match self.handle.edit_permissions_file().await {
            Ok(o) => {
                telemetry::emit_command_completed(cmd_evt::PERMISSIONS_COMPLETED, "");
                CommandResult::Done {
                    display: Some(format!(
                        "Edited {} (exit {}).",
                        o.edited_path.display(),
                        o.exit_code
                    )),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::PERMISSIONS_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not edit permissions: {msg}")),
                }
            }
        }
    }
    fn name(&self) -> &str {
        "permissions"
    }
    fn description(&self) -> &str {
        core_description("permissions")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "permissions".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_renders_edited_template() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = PermissionsHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Edited /tmp/mock/permissions.json (exit 0).");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn failure_prefixes_error() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_permissions_editor_error("permission denied".into());
        let h = PermissionsHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "Could not edit permissions: handle action failed: permission denied"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = PermissionsHandler::new(mock);
        assert_eq!(h.name(), "permissions");
        assert_eq!(
            h.description(),
            "Manage allow and deny tool permission rules"
        );
    }
}
