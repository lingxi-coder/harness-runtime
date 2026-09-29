//! `/config` — opens `$EDITOR` on `<config-dir>/claude/config.json`.
//!
//! Locked display template (`LingXi` UX, M5-11 T0 step 2 L2):
//!   `"Edited {path} (exit {code})."`
//! Failure prefix: `"Could not edit config: "`.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

/// `/config` handler — opens `$EDITOR` on the config file.
#[derive(Clone)]
pub struct ConfigHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ConfigHandler {
    /// Construct a `ConfigHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ConfigHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::CONFIG_STARTED);
        match self.handle.edit_config_file().await {
            Ok(o) => {
                telemetry::emit_command_completed(cmd_evt::CONFIG_COMPLETED, "");
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
                telemetry::emit_command_failed(cmd_evt::CONFIG_FAILED, &msg);
                CommandResult::Done {
                    display: Some(format!("Could not edit config: {msg}")),
                }
            }
        }
    }
    fn name(&self) -> &str {
        "config"
    }
    fn description(&self) -> &str {
        core_description("config")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "config".into(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn success_renders_edited_template() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ConfigHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(s, "Edited /tmp/mock/config.json (exit 0).");
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn failure_prefixes_error() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_config_editor_error("permission denied".into());
        let h = ConfigHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args()).await {
            assert_eq!(
                s,
                "Could not edit config: handle action failed: permission denied"
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ConfigHandler::new(mock);
        assert_eq!(h.name(), "config");
        assert_eq!(h.description(), "Open settings");
    }
}
