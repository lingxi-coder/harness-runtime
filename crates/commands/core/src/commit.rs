//! `/commit` — returns the locked commit prompt as an injected user message so
//! the next turn analyses the staged changes and creates a single git commit.
//!
//! 1:1 behavioral port of the claude-code `commit` slash command
//! (`src/commands/commit.ts`, a `prompt`-type command whose
//! `getPromptForCommand(_args, context)` returns `getPromptContent()` run
//! through `executeShellCommandsInPrompt`). The TS `getPromptContent` builds
//! the template below; the embedded ``!`git …` `` patterns are expanded at
//! runtime by `executeShellCommandsInPrompt` (a host concern), so they are
//! preserved verbatim here. `getPromptForCommand` ignores `_args`, so no user
//! args are interpolated into the template.
//!
//! The TS interpolates the default commit attribution
//! (`Co-Authored-By: <model> <noreply@anthropic.com>`) into the HEREDOC commit
//! body; it is ported as the standard `Co-Authored-By:` trailer.

use async_trait::async_trait;
use command_api::builtin_support::names::core_description;
use command_api::model::{BuiltinCommandHandler, CommandResult};
use command_api::parser::ParsedSlashCommand;

/// Per-command allowed-tools for `/commit`, verbatim from the TS
/// `commit.ts:6-10` `ALLOWED_TOOLS`. Injected into a fresh per-command effective
/// policy before the embedded `!`git …`` bodies are expanded, so `git add` /
/// `git status` / `git commit` are permitted alongside the read-only git
/// auto-allow. Surfaced via [`CommitHandler::allowed_tools`].
pub const ALLOWED_TOOLS: &[&str] = &[
    "Bash(git add:*)",
    "Bash(git status:*)",
    "Bash(git commit:*)",
];

/// `/commit` handler — returns the commit prompt as `InjectMessage`.
///
/// Handle-free: `/commit` is a static template injection. The next
/// conversation turn picks up the injected content as if the user had typed it.
#[derive(Debug, Default)]
pub struct CommitHandler;

impl CommitHandler {
    /// Construct a new `CommitHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl BuiltinCommandHandler for CommitHandler {
    async fn handle(&self, _args: &ParsedSlashCommand) -> CommandResult {
        CommandResult::InjectMessage {
            content: build_prompt(),
        }
    }

    fn name(&self) -> &str {
        "commit"
    }

    fn description(&self) -> &str {
        core_description("commit")
    }

    fn allowed_tools(&self) -> &'static [&'static str] {
        ALLOWED_TOOLS
    }
}

/// Build the `/commit` prompt verbatim from the TS `getPromptContent` template.
///
/// The embedded ``!`…` `` shell patterns are expanded by the host's
/// shell-execution pass before the message is delivered; they are intentionally
/// left intact here. The default commit attribution is rendered as the standard
/// `Co-Authored-By:` trailer inside the HEREDOC body.
fn build_prompt() -> String {
    r#"## Context

- Current git status: !`git status`
- Current git diff (staged and unstaged changes): !`git diff HEAD`
- Current branch: !`git branch --show-current`
- Recent commits: !`git log --oneline -10`

## Git Safety Protocol

- NEVER update the git config
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it
- CRITICAL: ALWAYS create NEW commits. NEVER use git commit --amend, unless the user explicitly requests it
- Do not commit files that likely contain secrets (.env, credentials.json, etc). Warn the user if they specifically request to commit those files
- If there are no changes to commit (i.e., no untracked files and no modifications), do not create an empty commit
- Never use git commands with the -i flag (like git rebase -i or git add -i) since they require interactive input which is not supported

## Your task

Based on the above changes, create a single git commit:

1. Analyze all staged changes and draft a commit message:
   - Look at the recent commits above to follow this repository's commit message style
   - Summarize the nature of the changes (new feature, enhancement, bug fix, refactoring, test, docs, etc.)
   - Ensure the message accurately reflects the changes and their purpose (i.e. "add" means a wholly new feature, "update" means an enhancement to an existing feature, "fix" means a bug fix, etc.)
   - Draft a concise (1-2 sentences) commit message that focuses on the "why" rather than the "what"

2. Stage relevant files and create the commit using HEREDOC syntax:
```
git commit -m "$(cat <<'EOF'
Commit message here.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
)"
```

You have the capability to call multiple tools in a single response. Stage and create the commit using a single message. Do not use any other tools or do anything else. Do not send any other text or messages besides these tool calls."#
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "commit".to_string(),
            raw_args: String::new(),
            positional_args: vec![],
        }
    }

    #[tokio::test]
    async fn returns_inject_message_with_locked_template() {
        let h = CommitHandler::new();
        match h.handle(&args()).await {
            CommandResult::InjectMessage { content } => {
                assert!(content.starts_with("## Context"));
                assert!(content.contains("Based on the above changes, create a single git commit:"));
                assert!(content
                    .contains("CRITICAL: ALWAYS create NEW commits. NEVER use git commit --amend"));
                assert!(content.contains("git commit -m \"$(cat <<'EOF'"));
                assert!(content.contains("Co-Authored-By: Claude <noreply@anthropic.com>"));
            }
            other => panic!("expected InjectMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn template_is_independent_of_args() {
        // `getPromptForCommand` ignores `_args`; the prompt must be identical
        // regardless of any user-supplied args.
        let h = CommitHandler::new();
        let with_empty = match h.handle(&args()).await {
            CommandResult::InjectMessage { content } => content,
            other => panic!("expected InjectMessage, got {other:?}"),
        };
        let with_args = match h
            .handle(&ParsedSlashCommand {
                name: "commit".to_string(),
                raw_args: "some user note".to_string(),
                positional_args: vec![],
            })
            .await
        {
            CommandResult::InjectMessage { content } => content,
            other => panic!("expected InjectMessage, got {other:?}"),
        };
        assert_eq!(with_empty, with_args);
    }

    #[test]
    fn name_and_description() {
        let h = CommitHandler::new();
        assert_eq!(h.name(), "commit");
        assert_eq!(h.description(), core_description("commit"));
    }

    #[test]
    fn allowed_tools_match_ts() {
        // Verbatim from commit.ts:6-10.
        let h = CommitHandler::new();
        assert_eq!(
            h.allowed_tools(),
            &[
                "Bash(git add:*)",
                "Bash(git status:*)",
                "Bash(git commit:*)"
            ]
        );
    }
}
