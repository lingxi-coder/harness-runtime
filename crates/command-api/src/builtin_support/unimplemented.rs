//! Shared stub handler used for every slash command that lingxi-core has not
//! yet implemented in v0.6.0 (M5).
//!
//! The current registry advertises 86 builtin names. The 19 core commands
//! point at per-name placeholder structs (see `builtin/core_placeholders.rs`)
//! so M5-10 / M5-11 can swap each one's body independently. Both kinds return
//! the same locked literal until the real bodies land.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-09-commands-surface.md`
//! Task 0 step 4 for the byte-locked literal.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;

/// Stub handler that returns the locked
/// `"{name}: not implemented in v0.6.0 (M5)"` literal.
///
/// Used for every M5-09-registered command that does not have a real body yet.
#[derive(Debug, Clone)]
pub struct UnimplementedCommandHandler {
    name: String,
    description: String,
}

impl UnimplementedCommandHandler {
    /// Construct a handler keyed by `name` with the given `description` for
    /// `/help` rendering.
    #[must_use]
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }

    /// Produce the locked stub literal for a given command name (without `/`).
    ///
    /// Public so the dispatcher can use the same formatting for "Unknown
    /// command" responses without going through a handler instance.
    #[must_use]
    pub fn stub_literal(name: &str) -> String {
        format!("{name}: not implemented in v0.6.0 (M5)")
    }
}

#[async_trait]
impl BuiltinCommandHandler for UnimplementedCommandHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::Done {
            display: Some(Self::stub_literal(&self.name)),
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CommandResult;
    use crate::parser::ParsedSlashCommand;

    #[tokio::test]
    async fn returns_locked_literal_with_name_substituted() {
        let h = UnimplementedCommandHandler::new("teleport", "(unimplemented in v0.6.0)");
        let args = ParsedSlashCommand {
            name: "teleport".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, "teleport: not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Done{{display: Some(...)}}, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn name_and_description_accessors_return_constructor_values() {
        let h = UnimplementedCommandHandler::new("teleport", "Configure remote sessions");
        assert_eq!(h.name(), "teleport");
        assert_eq!(h.description(), "Configure remote sessions");
    }

    #[tokio::test]
    async fn empty_name_still_produces_locked_format() {
        let h = UnimplementedCommandHandler::new("", "");
        let args = ParsedSlashCommand {
            name: String::new(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        match h.handle(&args).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(s, ": not implemented in v0.6.0 (M5)");
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
