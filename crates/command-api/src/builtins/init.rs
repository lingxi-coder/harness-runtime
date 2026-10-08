//! `/init` — returns the locked `INIT_PROMPT` template as an injected
//! user message so the next turn analyses the codebase and writes LINGXI.md.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-10-commands-batch-1.md`
//! Task 8.

use crate::builtin_support::names::core_description;
use crate::builtins::templates::INIT_PROMPT;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use telemetry::tengu::command as cmd_evt;

/// `/init` handler — returns the locked init prompt as `InjectMessage`.
///
/// No orchestrator dependency: `/init` is a static template injection. The
/// next conversation turn picks up the injected content as if the user
/// had typed it.
#[derive(Debug, Default)]
pub struct InitHandler;

impl InitHandler {
    /// Construct a new `InitHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for InitHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::INIT_STARTED);
        let details = format!("{{\"template_bytes\":{}}}", INIT_PROMPT.len());
        telemetry::emit_command_completed(cmd_evt::INIT_COMPLETED, &details);
        CommandResult::InjectMessage {
            content: INIT_PROMPT.to_string(),
        }
    }

    fn name(&self) -> &str {
        "init"
    }

    fn description(&self) -> &str {
        core_description("init")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn returns_inject_message_with_locked_template() {
        let h = InitHandler::new();
        let args = ParsedSlashCommand {
            name: "init".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        match h.handle(&args).await {
            CommandResult::InjectMessage { content } => {
                assert_eq!(content, INIT_PROMPT.as_str());
                assert!(!content.contains("claude.ai/code"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inject_message_content_starts_with_locked_first_sentence() {
        let h = InitHandler::new();
        let args = ParsedSlashCommand {
            name: "init".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        };
        let r = h.handle(&args).await;
        if let CommandResult::InjectMessage { content } = r {
            assert!(content.starts_with("Please analyze this codebase"));
        }
    }

    #[test]
    fn name_and_description() {
        let h = InitHandler::new();
        assert_eq!(h.name(), "init");
        assert_eq!(
            h.description(),
            "Initialize a new LINGXI.md file with codebase documentation"
        );
    }
}
