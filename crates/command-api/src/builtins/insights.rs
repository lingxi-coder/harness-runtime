//! `/insights` — returns the locked insights prompt as an injected user
//! message so the next turn reports on the user's LingXi usage.
//!
//! 1:1 behavioral port of the claude-code `insights` slash command
//! (`src/commands/insights.ts`, a `type: 'prompt'` command whose
//! `getPromptForCommand(args)` runs the usage-report pipeline and then
//! returns a single `text` content block). The block frames the generated
//! report for Claude and ends with the exact `<message>` the assistant must
//! output verbatim. The TS consults `args` only via
//! `args?.includes('--homespaces')` (the ant-only remote-collection flag);
//! user-supplied args are surfaced here from `args.raw_args`.

use crate::builtin_support::names::core_description;
use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;

/// `/insights` handler — returns the insights prompt as `InjectMessage`.
///
/// Handle-free: `/insights` injects the framing prompt (with the optional
/// trailing user input). The next conversation turn picks up the injected
/// content as if the user had typed it.
#[derive(Debug, Default)]
pub struct InsightsHandler;

impl InsightsHandler {
    /// Construct a new `InsightsHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for InsightsHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::InjectMessage {
            content: build_prompt(&args.raw_args),
        }
    }

    fn name(&self) -> &str {
        "insights"
    }

    fn description(&self) -> &str {
        core_description("insights")
    }
}

/// Build the insights framing prompt, appending an `Additional user input:`
/// line only when `args` is non-empty (the TS reads `args` solely to detect
/// the `--homespaces` collection flag; here any raw args are surfaced so the
/// next turn can honor them).
fn build_prompt(args: &str) -> String {
    let additional = if args.is_empty() {
        String::new()
    } else {
        format!("Additional user input: {args}\n")
    };
    // claude-code 2.1.205 framing — single-newline sections, the
    // for-your-context-only summary label, and the verbatim-output
    // instruction (the older "Here is what the user sees:" / "Now output the
    // following message exactly:" wording is gone from the binary).
    format!(
        r#"The user just ran /insights to generate a usage report analyzing their LingXi sessions.
Here is the full insights data:
{{insights_json}}
Report URL: {{report_url}}
HTML file: {{html_path}}
Facets directory: {{facets_dir}}
At-a-glance summary (for your context only — the user has not seen any output yet):
{{user_summary}}{{upload_hint}}
Output the text between <message> tags verbatim as your entire response. Do not omit any line:
<message>
Your shareable insights report is ready:
{{report_url}}
Want to dig into any section or try one of the suggestions?
</message>
{additional}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "insights".to_string(),
            raw_args: raw.to_string(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_inject_message_with_locked_template() {
        let h = InsightsHandler::new();
        match h.handle(&args("")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.contains(
                    "The user just ran /insights to generate a usage report analyzing their LingXi sessions."
                ));
                assert!(content.contains(
                    "Output the text between <message> tags verbatim as your entire response. Do not omit any line:"
                ));
                assert!(content.contains(
                    "At-a-glance summary (for your context only — the user has not seen any output yet):"
                ));
                assert!(content.contains("Your shareable insights report is ready:"));
                assert!(
                    content.contains("Want to dig into any section or try one of the suggestions?")
                );
                // No args => no "Additional user input:" line.
                assert!(!content.contains("Additional user input:"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn surfaces_additional_user_input_when_args_present() {
        let h = InsightsHandler::new();
        match h.handle(&args("--homespaces")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.contains("Additional user input: --homespaces"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let h = InsightsHandler::new();
        assert_eq!(h.name(), "insights");
        assert_eq!(h.description(), core_description("insights"));
    }
}
