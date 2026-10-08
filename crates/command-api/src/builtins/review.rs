//! `/review` — returns the locked code-review prompt as an injected user
//! message so the next turn fetches a GitHub PR and produces a code review.
//!
//! 1:1 byte port of claude-code 2.1.205's `review` command (`type: "prompt"`,
//! `argumentHint: "[pr number]"`, `progressMessage: "reviewing pull
//! request"`). Its `getPromptForCommand(e)` splits the args on whitespace,
//! cleans the first token of backticks and a leading `#`, and returns:
//! - no PR token → the `gh pr list` guidance line (`hs_`);
//! - a PR token → the review-target template (`gs_(n, rest)`), whose
//!   `Additional instructions from the user: ${t}` line is UNCONDITIONAL
//!   (renders with an empty tail when no extra instructions were given).
//!
//! Note: `/review` is a builtin but non-core command, so
//! `core_description("review")` resolves to the shared fallback string rather
//! than a dedicated description.
//!
//! **UNWIRED since the cc2.1.238 pass.** claude-code 2.1.238 deleted the
//! command outright (`name:"review"`: 2.1.220 = 1 hit, 2.1.238 = 0; likewise
//! `Review a GitHub pull request` 2 → 0) — the PR-review surface moved into
//! the bundled `code-review` skill. `review` was therefore dropped from
//! `BUILTIN_COMMAND_NAMES`, `core_description`, `register_core_batch_3` and
//! the TUI palette. This module is retained (byte-exact prompt template and
//! its tests) as the source for a future skill port; nothing registers it.

use crate::builtin_support::names::core_description;
use crate::model::{
    source_concat, source_from_units, BuiltinCommandHandler, CommandResult, PromptPart,
};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

/// `/review` handler — returns the code-review prompt as `InjectMessage`.
///
/// Handle-free: `/review` is a static template injection (with the user's args
/// interpolated as the PR number). The next conversation turn picks up the
/// injected content as if the user had typed it.
#[derive(Debug, Default)]
pub struct ReviewHandler;

impl ReviewHandler {
    /// Construct a new `ReviewHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for ReviewHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::InjectMessage {
            content: build_prompt(&args.raw_args),
        }
    }

    fn name(&self) -> &str {
        "review"
    }

    fn description(&self) -> &str {
        core_description("review")
    }
}

/// The no-PR-number branch (claude-code 2.1.205 `hs_`).
const LIST_PROMPT: &str = "Run `gh pr list` to show the open pull requests, then ask the user which one to review (`/review <number>`).";

/// Build the injected prompt (claude-code 2.1.205 `getPromptForCommand`):
/// first whitespace token (stripped of backticks and a leading `#`) is the PR
/// number; the rest join as extra instructions.
fn build_prompt(args: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = args.string_units("").unwrap_or_default();
    let mut tokens = Vec::<Vec<u16>>::new();
    let mut current = Vec::new();
    for unit in &units {
        let whitespace = char::from_u32(u32::from(*unit))
            .is_some_and(lingxi_core::host::effort::javascript_whitespace);
        if whitespace {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        } else {
            current.push(*unit);
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    let mut number = tokens.first().cloned().unwrap_or_default();
    number.retain(|unit| *unit != b'`' as u16);
    if number.first().copied() == Some(b'#' as u16) {
        number.remove(0);
    }
    if number.is_empty() {
        return LIST_PROMPT.into();
    }
    let mut instructions = Vec::new();
    for token in tokens.iter().skip(1) {
        if !instructions.is_empty() {
            instructions.push(b' ' as u16);
        }
        instructions.extend_from_slice(token);
    }
    let number = source_from_units(number);
    let instructions = source_from_units(instructions);
    source_concat([
        PromptPart::Text("Review target: GitHub pull request `"),
        PromptPart::Source(&number),
        PromptPart::Text(
            "`.\nGather this target's diff with (instead of any local `git diff`):\n1. `gh pr view ",
        ),
        PromptPart::Source(&number),
        PromptPart::Text(
            " --json title,body,author,baseRefName,headRefName,state,additions,deletions,changedFiles,labels` for context\n2. `gh pr diff ",
        ),
        PromptPart::Source(&number),
        PromptPart::Text(
            "` for the unified diff\nThe PR's diff is the only review scope — local working-tree changes are out of scope. When you need surrounding code, Read the files in this checkout if it matches the PR's branch, otherwise fetch file contents via `gh`.\nAdditional instructions from the user: ",
        ),
        PromptPart::Source(&instructions),
        PromptPart::Text(
            "\nAnalyze the changes and provide a thorough code review that includes:\n- An overview of what the PR does\n- Analysis of code quality and style\n- Specific suggestions for improvements\n- Any potential issues or risks\nKeep your review concise but thorough. Focus on:\n- Code correctness\n- Following project conventions\n- Performance implications\n- Test coverage\n- Security considerations\nFormat your review with clear sections and bullet points.",
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "review".into(),
            raw_args: raw.into(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn no_pr_number_returns_the_list_guidance() {
        let h = ReviewHandler::new();
        match h.handle(&args("")).await {
            CommandResult::InjectMessage { content } => {
                assert_eq!(
                    content.string_at("").unwrap(),
                    "Run `gh pr list` to show the open pull requests, then ask the user \
                     which one to review (`/review <number>`)."
                );
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_number_builds_the_review_target_template() {
        let h = ReviewHandler::new();
        match h.handle(&args("123")).await {
            CommandResult::InjectMessage { content } => {
                let content = content.string_at("").unwrap();
                assert!(content.starts_with("Review target: GitHub pull request `123`.\n"));
                assert!(content.contains("1. `gh pr view 123 --json title,body,author,baseRefName,headRefName,state,additions,deletions,changedFiles,labels` for context\n"));
                assert!(content.contains("2. `gh pr diff 123` for the unified diff\n"));
                // The instructions line is unconditional — empty tail here.
                assert!(content.contains("Additional instructions from the user: \n"));
                assert!(
                    content.ends_with("Format your review with clear sections and bullet points.")
                );
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pr_token_is_cleaned_and_instructions_join() {
        let h = ReviewHandler::new();
        match h.handle(&args("`#456`  focus on   tests")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.starts_with("Review target: GitHub pull request `456`.\n"));
                assert!(content.contains("Additional instructions from the user: focus on tests\n"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let h = ReviewHandler::new();
        assert_eq!(h.name(), "review");
        assert_eq!(h.description(), core_description("review"));
    }
}
