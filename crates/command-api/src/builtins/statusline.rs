//! `/statusline` — set up LingXi's status line UI.
//!
//! 1:1 behavioral port of the TS `statusline` prompt command
//! (`src/commands/statusline.tsx`). The TS `getPromptForCommand` trims the
//! user args and falls back to a default sentence when empty, then injects:
//!
//! ```text
//! Create an Agent with subagent_type "statusline-setup" and the prompt "<prompt>"
//! ```
//!
//! where `Agent` is the TS `AGENT_TOOL_NAME` constant (`'Agent'`).
//!
//! No orchestrator dependency: this is a static template injection. The next
//! conversation turn picks up the injected content as if the user had typed it.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;

/// The TS `AGENT_TOOL_NAME` constant (`src/tools/AgentTool/constants.ts`).
const AGENT_TOOL_NAME: &str = "Agent";

/// Default prompt used when the user supplies no args (matches the TS fallback).
const DEFAULT_PROMPT: &str = "Configure my statusLine from my shell PS1 configuration";

/// `/statusline` handler — returns the ported setup prompt as `InjectMessage`.
///
/// Handle-free: the behavior is a pure function of the user args, so the
/// struct carries no orchestrator handle and constructs via `new()`/`default()`.
#[derive(Debug, Default)]
pub struct StatuslineHandler;

impl StatuslineHandler {
    /// Construct a new `StatuslineHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for StatuslineHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        // TS: `const prompt = args.trim() || '<default>'`
        let trimmed = args.raw_args.trim();
        let prompt = if trimmed.is_empty() {
            DEFAULT_PROMPT
        } else {
            trimmed
        };
        // TS: `Create an ${AGENT_TOOL_NAME} with subagent_type "statusline-setup" and the prompt "${prompt}"`
        let content = format!(
            "Create an {AGENT_TOOL_NAME} with subagent_type \"statusline-setup\" and the prompt \"{prompt}\""
        );
        CommandResult::InjectMessage { content }
    }

    fn name(&self) -> &str {
        "statusline"
    }

    fn description(&self) -> &str {
        core_description("statusline")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "statusline".to_string(),
            raw_args: raw.to_string(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_inject_message_with_default_prompt_when_args_empty() {
        let h = StatuslineHandler::new();
        match h.handle(&args("")).await {
            CommandResult::InjectMessage { content } => {
                assert_eq!(
                    content,
                    "Create an Agent with subagent_type \"statusline-setup\" \
                     and the prompt \"Configure my statusLine from my shell PS1 configuration\""
                );
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn returns_inject_message_with_trimmed_user_args() {
        let h = StatuslineHandler::new();
        match h.handle(&args("  use a minimal layout  ")).await {
            CommandResult::InjectMessage { content } => {
                assert_eq!(
                    content,
                    "Create an Agent with subagent_type \"statusline-setup\" \
                     and the prompt \"use a minimal layout\""
                );
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn inject_content_contains_stable_substring() {
        let h = StatuslineHandler::new();
        if let CommandResult::InjectMessage { content } = h.handle(&args("")).await {
            assert!(content.contains("subagent_type \"statusline-setup\""));
        } else {
            panic!("expected InjectMessage");
        }
    }

    #[test]
    fn name_and_description() {
        let h = StatuslineHandler::new();
        assert_eq!(h.name(), "statusline");
        assert_eq!(h.description(), core_description("statusline"));
    }
}
