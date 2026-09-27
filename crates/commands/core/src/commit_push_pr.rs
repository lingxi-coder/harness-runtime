//! `/commit-push-pr` — port of the claude-code `commit-push-pr` prompt command.
//!
//! Behavioral-parity port of
//! `claude-code/src/commands/commit-push-pr.ts`. The TS command builds its
//! prompt via `getPromptContent(defaultBranch, prAttribution)` and appends any
//! user-supplied args under an "## Additional instructions from user" section,
//! then injects the resulting text as the next user turn (`ts_type=prompt`,
//! kind `inject`).
//!
//! This handler is HANDLE-FREE: it cannot run git or read user settings at
//! build time, so it reproduces the TS default (external, non-undercover,
//! no-custom-settings) path verbatim:
//! - `defaultBranch` = `main` (the same value the TS uses as its canonical
//!   `contentLength` estimate),
//! - `commitAttribution` = `Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>`
//!   (TS external-repo fallback model name),
//! - `prAttribution` = `🤖 Generated with [Claude Code](https://claude.com/claude-code)`,
//! - `SAFEUSER` / `whoami` context lines resolve to empty (TS
//!   `process.env.X || ''` default),
//! - the changelog section, Slack step, and reviewer args take their
//!   non-undercover default values.

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;

/// Per-command allowed-tools for `/commit-push-pr`, verbatim from the TS
/// `commit-push-pr.ts:10-23` `ALLOWED_TOOLS`. Injected into a fresh per-command
/// effective policy before the embedded `!`git …`` / `!`gh …`` bodies are
/// expanded — notably `gh pr view --json number 2>/dev/null || true`
/// (commit_push_pr.rs:75), which is neither read-only nor auto-safe and relies
/// on `Bash(gh pr view:*)` being injected here. Surfaced via
/// [`CommitPushPrHandler::allowed_tools`].
pub const ALLOWED_TOOLS: &[&str] = &[
    "Bash(git checkout --branch:*)",
    "Bash(git checkout -b:*)",
    "Bash(git add:*)",
    "Bash(git status:*)",
    "Bash(git push:*)",
    "Bash(git commit:*)",
    "Bash(gh pr create:*)",
    "Bash(gh pr edit:*)",
    "Bash(gh pr view:*)",
    "Bash(gh pr merge:*)",
    "ToolSearch",
    "mcp__slack__send_message",
    "mcp__claude_ai_Slack__slack_send_message",
];

/// Default branch used by the prompt (matches the TS `contentLength` estimate).
const DEFAULT_BRANCH: &str = "main";

/// TS external-repo default commit attribution line.
const COMMIT_ATTRIBUTION: &str = "Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>";

/// TS default PR attribution line.
const PR_ATTRIBUTION: &str = "🤖 Generated with [LingXi](https://claude.com/claude-code)";

/// `/commit-push-pr` handler — injects the commit/push/PR prompt template.
///
/// No orchestrator dependency: this is a static template injection. The next
/// conversation turn picks up the injected content as if the user had typed it.
#[derive(Debug, Default)]
pub struct CommitPushPrHandler;

impl CommitPushPrHandler {
    /// Construct a new `CommitPushPrHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

/// Build the prompt content, porting the TS `getPromptContent` template
/// verbatim for the default (external, non-undercover) path.
fn build_prompt() -> String {
    // TS `reviewerArg` / `addReviewerArg` default (non-undercover) values.
    let reviewer_arg = " and `--reviewer anthropics/claude-code`";
    let add_reviewer_arg = " (and add `--add-reviewer anthropics/claude-code`)";
    // TS `changelogSection` default value.
    let changelog_section = "\n\n## Changelog\n<!-- CHANGELOG:START -->\n[If this PR contains user-facing changes, add a changelog entry here. Otherwise, remove this section.]\n<!-- CHANGELOG:END -->";
    // TS `slackStep` default value.
    let slack_step = "\n\n5. After creating/updating the PR, check if the user's LINGXI.md mentions posting to Slack channels. If it does, use ToolSearch to search for \"slack send message\" tools. If ToolSearch finds a Slack tool, ask the user if they'd like you to post the PR URL to the relevant Slack channel. Only post if the user confirms. If ToolSearch returns no results or errors, skip this step silently—do not mention the failure, do not attempt workarounds, and do not try alternative approaches.";

    // `commitAttribution` is non-empty on the default path, so the conditional
    // `, ending with the attribution text shown in the example below` is
    // emitted, and the heredoc body includes `\n\n${commitAttribution}`.
    format!(
        "## Context

- `SAFEUSER`: {safe_user}
- `whoami`: {username}
- `git status`: !`git status`
- `git diff HEAD`: !`git diff HEAD`
- `git branch --show-current`: !`git branch --show-current`
- `git diff {default_branch}...HEAD`: !`git diff {default_branch}...HEAD`
- `gh pr view --json number 2>/dev/null || true`: !`gh pr view --json number 2>/dev/null || true`

## Git Safety Protocol

- NEVER update the git config
- NEVER run destructive/irreversible git commands (like push --force, hard reset, etc) unless the user explicitly requests them
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it
- NEVER run force push to main/master, warn the user if they request it
- Do not commit files that likely contain secrets (.env, credentials.json, etc)
- Never use git commands with the -i flag (like git rebase -i or git add -i) since they require interactive input which is not supported

## Your task

Analyze all changes that will be included in the pull request, making sure to look at all relevant commits (NOT just the latest commit, but ALL commits that will be included in the pull request from the git diff {default_branch}...HEAD output above).

Based on the above changes:
1. Create a new branch if on {default_branch} (use SAFEUSER from context above for the branch name prefix, falling back to whoami if SAFEUSER is empty, e.g., `username/feature-name`)
2. Create a single commit with an appropriate message using heredoc syntax, ending with the attribution text shown in the example below:
```
git commit -m \"$(cat <<'EOF'
Commit message here.\n\n{commit_attribution}
EOF
)\"
```
3. Push the branch to origin
4. If a PR already exists for this branch (check the gh pr view output above), update the PR title and body using `gh pr edit` to reflect the current diff{add_reviewer_arg}. Otherwise, create a pull request using `gh pr create` with heredoc syntax for the body{reviewer_arg}.
   - IMPORTANT: Keep PR titles short (under 70 characters). Use the body for details.
```
gh pr create --title \"Short, descriptive title\" --body \"$(cat <<'EOF'
## Summary
<1-3 bullet points>

## Test plan
[Bulleted markdown checklist of TODOs for testing the pull request...]{changelog_section}\n\n{pr_attribution}
EOF
)\"
```

You have the capability to call multiple tools in a single response. You MUST do all of the above in a single message.{slack_step}

Return the PR URL when you're done, so the user can see it.",
        safe_user = "",
        username = "",
        default_branch = DEFAULT_BRANCH,
        commit_attribution = COMMIT_ATTRIBUTION,
        add_reviewer_arg = add_reviewer_arg,
        reviewer_arg = reviewer_arg,
        changelog_section = changelog_section,
        pr_attribution = PR_ATTRIBUTION,
        slack_step = slack_step,
    )
}

#[async_trait]
impl BuiltinCommandHandler for CommitPushPrHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        let mut content = build_prompt();

        // TS: append user instructions if args provided (trimmed, non-empty).
        let trimmed = args.raw_args.trim();
        if !trimmed.is_empty() {
            content.push_str("\n\n## Additional instructions from user\n\n");
            content.push_str(trimmed);
        }

        CommandResult::InjectMessage { content }
    }

    fn name(&self) -> &str {
        "commit-push-pr"
    }

    fn description(&self) -> &str {
        core_description("commit-push-pr")
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        ALLOWED_TOOLS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "commit-push-pr".to_string(),
            raw_args: raw.to_string(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_inject_message_with_ported_template() {
        let h = CommitPushPrHandler::new();
        match h.handle(&args("")).await {
            CommandResult::InjectMessage { content } => {
                // Stable substrings from the TS getPromptContent template.
                assert!(content.contains("## Git Safety Protocol"));
                assert!(
                    content.contains("Return the PR URL when you're done, so the user can see it.")
                );
                assert!(content.contains("- NEVER update the git config"));
                assert!(content.contains("gh pr create --title"));
                // No trailing user-instructions section when no args.
                assert!(!content.contains("## Additional instructions from user"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn appends_user_instructions_when_args_present() {
        let h = CommitPushPrHandler::new();
        match h.handle(&args("  use a draft PR  ")).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.contains("## Additional instructions from user\n\nuse a draft PR"));
                // Args are trimmed before interpolation.
                assert!(!content.contains("  use a draft PR  "));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[test]
    fn name_and_description() {
        let h = CommitPushPrHandler::new();
        assert_eq!(h.name(), "commit-push-pr");
        assert_eq!(h.description(), core_description("commit-push-pr"));
    }

    #[test]
    fn allowed_tools_match_ts() {
        // Verbatim from commit-push-pr.ts:10-23 (git checkout/add/status/push/
        // commit + gh pr create/edit/view/merge + ToolSearch + slack mcp tools).
        let h = CommitPushPrHandler::new();
        let tools = h.allowed_tools();
        assert_eq!(tools.len(), 13);
        assert_eq!(tools[0], "Bash(git checkout --branch:*)");
        assert!(tools.contains(&"Bash(gh pr view:*)"));
        assert!(tools.contains(&"Bash(git push:*)"));
        assert!(tools.contains(&"ToolSearch"));
    }
}
