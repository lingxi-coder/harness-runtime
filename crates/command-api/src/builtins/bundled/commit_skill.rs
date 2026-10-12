//! The `/commit` bundled prompt from Claude Code 2.1.295's `fo()` factory.
//!
//! The source factory builds its prompt from raw guidance and the session's
//! commit attribution, escapes user/config text so it cannot introduce a shell
//! substitution, then lets the dispatcher expand only the source-authored
//! `!`command`` blocks under the prompt's allow/deny lists.

use crate::model::{
    append_source_units, append_text_units, source_from_units, trim_source_text, BundledPromptFn,
    CommandFrontmatter,
};
use lingxi_core::types::utf16_json::Utf16JsonProjection;

const DEFAULT_COMMIT_ATTRIBUTION: &str = "Co-Authored-By: Claude <noreply@anthropic.com>";

pub(crate) const COMMIT_DESCRIPTION: &str = "Create a git commit. Use whenever you are about to create a commit, whether the user asked for one or it is a step in your current task — it gathers git context and applies the required commit workflow (message style, staging rules, attribution).";

const ALLOWED_TOOLS: &[&str] = &[
    "Bash(certified: git add)",
    "Bash(certified: git status)",
    "Bash(certified: git commit)",
    "PowerShell(git add *)",
    "PowerShell(git status *)",
    "PowerShell(git commit -m *)",
];

const DISALLOWED_GIT_FLAGS: &[&str] = &[
    "git add --f*",
    "git add * --f*",
    "git add -f*",
    "git add * -f*",
    "git add --c*",
    "git add * --c*",
    "git commit *--fil*",
    "git commit * -F*",
    "git commit *--te*",
    "git commit * -t*",
    "git commit *--pathspec-fr*",
    "git commit *--no-veri*",
    "git commit *--no-g*",
    "git commit *--am*",
    "git commit *--allow-empty*",
    "git commit *--reu*",
    "git commit *--ree*",
];

/// `mo` from the source `fo()` factory, including its platform-specific
/// PowerShell variants for the three allowed command forms.
pub(crate) fn commit_allowed_tools() -> Vec<String> {
    ALLOWED_TOOLS
        .iter()
        .map(|tool| (*tool).to_owned())
        .collect()
}

/// `Qi` from the source `fo()` factory. Native `vUe()` emits each deny pattern
/// for both shell families; its extra `tt()` contribution is an empty list.
pub(crate) fn commit_disallowed_tools() -> Vec<String> {
    let mut tools = Vec::with_capacity(DISALLOWED_GIT_FLAGS.len() * 2);
    for pattern in DISALLOWED_GIT_FLAGS {
        tools.push(format!("Bash({pattern})"));
        tools.push(format!("PowerShell({pattern})"));
    }
    tools
}

/// Prompt factory for the reachable Native `commit` skill.
pub struct CommitPromptFn;

impl BundledPromptFn for CommitPromptFn {
    fn build(&self, args: &Utf16JsonProjection) -> Utf16JsonProjection {
        let guidance = trim_source_text(args);
        let guidance = fx(&guidance);
        let attribution = fx(&commit_attribution());
        let has_guidance = guidance
            .string_units("")
            .is_some_and(|units| !units.is_empty());
        let has_attribution = attribution
            .string_units("")
            .is_some_and(|units| !units.is_empty());

        let mut output = Vec::new();
        append_text_units(
            &mut output,
            "## Context\n\n- Current git status: !`git status`\n- Current git diff (staged and unstaged changes): !`git diff HEAD`\n- Current branch: !`git branch --show-current`\n- Recent commits: !`git log --oneline -10`\n",
        );
        if has_guidance {
            append_text_units(&mut output, "\nUser guidance for this commit: ");
            append_source_units(&mut output, &guidance);
            append_text_units(&mut output, "\n");
        }

        append_text_units(
            &mut output,
            r#"
## Git Safety Protocol

- NEVER update the git config
- NEVER run destructive git commands (push --force, reset --hard, checkout ., restore ., clean -f, branch -D) unless the user explicitly requests these actions
- NEVER skip hooks (--no-verify, --no-gpg-sign, etc) unless the user explicitly requests it
- NEVER force push to main/master; warn the user if they request it
- CRITICAL: Always create NEW commits rather than amending, unless the user explicitly requests a git amend. When a pre-commit hook fails, the commit did NOT happen — so --amend would modify the PREVIOUS commit, which may result in destroying work or losing previous changes. Instead, after hook failure, fix the issue, re-stage, and create a NEW commit
- When staging files, prefer adding specific files by name rather than using `git add -A` or `git add .`, which can accidentally include sensitive files (.env, credentials) or large binaries
- Do not commit files that likely contain secrets (.env, credentials.json, etc). Warn the user if they specifically request to commit those files
- If there are no changes to commit (i.e. no untracked files and no modifications), do not create an empty commit
- Never use git commands with the -i flag (like git rebase -i or git add -i) since they require interactive input which is not supported

## Your task

Based on the above changes, create a single git commit:

1. Analyze the changes and draft a commit message:
   - Look at the recent commits above to follow this repository's commit message style
   - Summarize the nature of the changes (new feature, enhancement, bug fix, refactoring, test, docs, etc.)
   - Ensure the message accurately reflects the changes and their purpose (i.e. "add" means a wholly new feature, "update" means an enhancement to an existing feature, "fix" means a bug fix, etc.)
   - Draft a concise (1-2 sentences) commit message that focuses on the "why" rather than the "what""#,
        );
        if has_attribution {
            append_text_units(&mut output, "\n\n");
            append_source_units(&mut output, &attribution);
        }
        append_text_units(&mut output, "\n\n2. Stage the relevant files and create the commit. To ensure good formatting, ALWAYS pass the commit message inline via a ");

        #[cfg(not(target_os = "windows"))]
        append_text_units(&mut output, "`HEREDOC`, never from a file or template (`-F`, `--file` and `-t` are refused while this skill runs):\n```\ngit commit -m \"$(cat <<'EOF'\nCommit message here.");
        #[cfg(target_os = "windows")]
        append_text_units(&mut output, "`here-string`, never from a file or template (`-F`, `--file` and `-t` are refused while this skill runs):\n```\ngit commit -m @'\nCommit message here.");

        if has_attribution {
            append_text_units(&mut output, "\n\n");
            append_source_units(&mut output, &attribution);
        }

        #[cfg(not(target_os = "windows"))]
        append_text_units(&mut output, "\nEOF\n\"\n)\n```\n");
        #[cfg(target_os = "windows")]
        append_text_units(
            &mut output,
            "\n'@\n```\nThe closing `'@` MUST be at column 0 with no leading whitespace.\n",
        );

        append_text_units(
            &mut output,
            "\n3. Run git status after the commit completes to verify it succeeded.\n\n4. If the commit fails due to a pre-commit hook: fix the issue, re-stage, and create a NEW commit. Never use --amend or --no-verify to get past a failing hook.\n\nYou have the capability to call multiple tools in a single response. Stage and create the commit using a single message. Do not run additional commands to read or explore code beyond the git context above, and do not use any non-git tools for this task.",
        );

        source_from_units(output)
    }
}

/// Resolve the Native commit trailer from the current settings snapshot. The
/// source calls `Fbt()` here and defaults its model identity to "Claude" when
/// the host cannot provide a first-party model identity.
fn commit_attribution() -> Utf16JsonProjection {
    let (commit, pr) = lingxi_core::host::session_flags::attribution();
    if commit.is_some() || pr.is_some() {
        return commit.unwrap_or_else(|| DEFAULT_COMMIT_ATTRIBUTION.into());
    }
    if lingxi_core::host::session_flags::include_co_authored_by() == Some(false) {
        Utf16JsonProjection::from("")
    } else {
        DEFAULT_COMMIT_ATTRIBUTION.into()
    }
}

/// Native `fx()` keeps raw source units but escapes text that could otherwise
/// be parsed as an embedded prompt-shell command.
fn fx(source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let units = source.string_units("").unwrap_or_default();
    let mut separated = Vec::with_capacity(units.len() + 8);
    let mut index = 0;
    while index < units.len() {
        if units[index] == b'`' as u16 && units.get(index + 1).copied() == Some(b'!' as u16) {
            separated.extend([b'`' as u16, b' ' as u16, b'!' as u16]);
            index += 2;
        } else if units[index] == b'!' as u16 && units.get(index + 1).copied() == Some(b'`' as u16)
        {
            separated.extend([b'!' as u16, b' ' as u16, b'`' as u16]);
            index += 2;
        } else {
            separated.push(units[index]);
            index += 1;
        }
    }

    let mut escaped = Vec::with_capacity(separated.len() + 4);
    for (index, unit) in separated.iter().copied().enumerate() {
        let after_whitespace = index == 0
            || separated
                .get(index.wrapping_sub(1))
                .is_some_and(|previous| {
                    char::from_u32(u32::from(*previous))
                        .is_some_and(lingxi_core::host::effort::javascript_whitespace)
                });
        if unit == b'!' as u16 && after_whitespace {
            escaped.push(b'\\' as u16);
        }
        escaped.push(unit);
    }
    source_from_units(escaped)
}

/// Build the command factory's source-fact `allowedTools` metadata.
pub(crate) fn frontmatter() -> CommandFrontmatter {
    CommandFrontmatter {
        allowed_tools: Some(commit_allowed_tools()),
        disallowed_tools: Some(commit_disallowed_tools()),
        ..CommandFrontmatter::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_uses_native_guidance_and_shell_escape_shape() {
        let prompt = CommitPromptFn.build(&"  include the test plan  ".into());
        let text = prompt.string_at("").unwrap();
        assert!(text.contains("User guidance for this commit: include the test plan"));
        assert!(text.contains("NEVER run destructive git commands"));
        assert!(text.contains("create a NEW commit"));
        assert!(text.contains("Co-Authored-By: Claude <noreply@anthropic.com>"));
        assert!(
            text.contains("!`git status`"),
            "only source-authored shell blocks remain active"
        );
        assert!(!text.contains("!`include the test plan"));
    }

    #[test]
    fn fx_escapes_embedded_prompt_shell_prefixes_without_rewriting_units() {
        let raw = Utf16JsonProjection::root_string(
            "guidance !`echo no` and `!echo no`".to_owned(),
            "guidance !`echo no` and `!echo no`"
                .encode_utf16()
                .collect(),
        )
        .unwrap();
        let escaped = fx(&raw);
        assert_eq!(
            escaped.string_at(""),
            Some("guidance \\! `echo no` and ` \\!echo no`")
        );
    }

    #[test]
    fn commit_factory_carries_native_shell_allow_and_deny_lists() {
        let frontmatter = frontmatter();
        assert_eq!(
            frontmatter.allowed_tools.unwrap(),
            [
                "Bash(certified: git add)",
                "Bash(certified: git status)",
                "Bash(certified: git commit)",
                "PowerShell(git add *)",
                "PowerShell(git status *)",
                "PowerShell(git commit -m *)",
            ]
        );
        let denied = frontmatter.disallowed_tools.unwrap();
        assert_eq!(denied.len(), DISALLOWED_GIT_FLAGS.len() * 2);
        assert_eq!(denied.first().unwrap(), "Bash(git add --f*)");
        assert_eq!(denied.last().unwrap(), "PowerShell(git commit *--ree*)");
    }
}
