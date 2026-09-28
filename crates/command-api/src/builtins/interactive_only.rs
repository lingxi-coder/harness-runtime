//! Reusable handler for commands whose Claude implementation is interactive-only.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;

/// Headless fallback for a command whose real surface is interactive TUI-only.
#[derive(Debug, Clone)]
pub struct InteractiveOnlyHandler {
    name: String,
    description: String,
}

impl InteractiveOnlyHandler {
    /// Build a fallback handler for `name` with its registry description.
    #[must_use]
    pub fn new(name: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
        }
    }
}

#[async_trait]
impl BuiltinCommandHandler for InteractiveOnlyHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::Done {
            display: Some(format!(
                "/{} is available in interactive TUI mode only.",
                self.name
            )),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }
}
