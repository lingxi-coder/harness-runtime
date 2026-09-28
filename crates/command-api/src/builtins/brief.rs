//! `/brief` — toggle the live Brief-only session mode.
//!
//! Claude Code exposes this as an interactive command that updates the live
//! `isBriefOnly` app state. LingXi keeps the same state transition in the
//! shared platform session flags so the `SendUserMessage` tool observes a
//! runtime toggle immediately, without consulting a process-environment
//! snapshot.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;

/// Handle-free `/brief` command. The live state is shared by the CLI, command
/// dispatcher, and tool registry through `platform_api::session_flags`.
#[derive(Debug, Default)]
pub struct BriefHandler;

impl BriefHandler {
    /// Construct a `/brief` handler.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for BriefHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        let enabled = platform_api::session_flags::toggle_brief_mode_enabled();
        let display = if enabled {
            "Brief-only mode enabled"
        } else {
            "Brief-only mode disabled"
        };
        CommandResult::Done {
            display: Some(display.to_string()),
        }
    }

    fn name(&self) -> &str {
        "brief"
    }

    fn description(&self) -> &str {
        core_description("brief")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "brief".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn toggles_live_mode_and_reports_state() {
        let prior = platform_api::session_flags::brief_mode_enabled();
        platform_api::session_flags::set_brief_mode_enabled(false);
        let handler = BriefHandler::new();

        assert!(matches!(
            handler.handle(&args()).await,
            CommandResult::Done {
                display: Some(ref display)
            } if display == "Brief-only mode enabled"
        ));
        assert!(platform_api::session_flags::brief_mode_enabled());
        assert!(matches!(
            handler.handle(&args()).await,
            CommandResult::Done {
                display: Some(ref display)
            } if display == "Brief-only mode disabled"
        ));
        assert!(!platform_api::session_flags::brief_mode_enabled());

        platform_api::session_flags::set_brief_mode_enabled(prior);
    }

    #[test]
    fn metadata_matches_oracle_command() {
        let handler = BriefHandler::new();
        assert_eq!(handler.name(), "brief");
        assert_eq!(handler.description(), "Toggle brief-only mode");
    }
}
