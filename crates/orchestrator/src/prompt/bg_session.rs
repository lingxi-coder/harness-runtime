//! `# Background Session` system-prompt section (M8 cc2.1.198).
//!
//! Ports the real 2.1.198 binary's `_ff()` (@219583413 region) — the section
//! that makes a BACKGROUND worktree job auto-commit / push / open a draft PR
//! when its code work is done. The 2.1.198 "Background agents auto commit,
//! push, and open a draft PR" changelog item is PROMPT-DRIVEN in the binary:
//! there is no programmatic completion-time git flow; the shipping paragraph
//! below instructs the model to run `gh pr create --draft` itself, and the
//! no-remote degradation is likewise instruction text ("Skip the PR only if …
//! there's no remote to push to (then commit and say where the work is)").
//!
//! Gates (binary `_ff`):
//! * `CLAUDE_CODE_SESSION_KIND !== "bg"` → no section (lingxi:
//!   `LINGXI_SESSION_KIND`).
//! * no `CLAUDE_JOB_DIR` → no section (lingxi: `LINGXI_JOB_DIR`).
//! * isolation `"none"` (binary `HAo()`) → the work-in-place message and NO
//!   shipping paragraph.
//! * `CLAUDE_BG_ISOLATION === "worktree"` → the `isolation: worktree`
//!   first-action message.
//! * otherwise → the enforced isolate-before-edit message.
//!
//! Depth note: `HAo()` falls back from the env var to the bg-session config's
//! `bgIsolation` and then the `worktree.bgIsolation` setting; lingxi reads
//! only the `LINGXI_BG_ISOLATION` env var today (no bg-session config /
//! worktree-settings seam exists yet — that fallback lands with the `--bg`
//! dispatcher).
//!
//! Placement: after the output-style section, before `# Context management`
//! (binary `cx()` order: env_info_simple, language, output_style,
//! **bg-session**, scratchpad, context_management).

/// Compute the section from explicit inputs (pure; the env-reading wrapper is
/// [`from_env`]). `session_kind`/`job_dir`/`isolation` mirror
/// `CLAUDE_CODE_SESSION_KIND` / `CLAUDE_JOB_DIR` / the resolved isolation
/// mode.
#[must_use]
pub fn section(
    session_kind: Option<&str>,
    job_dir: Option<&str>,
    isolation: Option<&str>,
) -> Option<String> {
    if session_kind != Some("bg") {
        return None;
    }
    let job_dir = job_dir.filter(|d| !d.is_empty())?;
    let in_place = isolation == Some("none");
    let isolation_msg = if in_place {
        "Edit files directly in your working directory \u{2014} this session is configured to \
         work in place rather than isolating into a worktree. Skip EnterWorktree unless the \
         user explicitly asks to work in a worktree."
            .to_string()
    } else if isolation == Some("worktree") {
        format!(
            "This agent is configured with `isolation: worktree`. Call the EnterWorktree tool \
             as your first action \u{2014} before reading files or running commands \u{2014} \
             unless your cwd is already under `{dot}/worktrees/`. If EnterWorktree fails, \
             continue in place.",
            dot = branding::DOT_DIR
        )
    } else {
        format!(
            "Before making any code changes, use the EnterWorktree tool to isolate your work \
             from other parallel jobs and the user's working copy \u{2014} unless your cwd is \
             already under `{dot}/worktrees/`, in which case you're already isolated. This is \
             enforced: file edits in the shared checkout are rejected until you isolate, so \
             call EnterWorktree before your first edit rather than after a rejected attempt. \
             If you're only reading, searching, or answering questions, skip this and work in \
             place. If EnterWorktree fails, continue in place.",
            dot = branding::DOT_DIR
        )
    };
    let shipping = if in_place {
        String::new()
    } else {
        "\n\nOnce your work is isolated in a worktree, shipping is part of the task: when \
         you've made code changes, commit them, push the branch, and open a draft PR (`gh pr \
         create --draft`) without stopping to ask \u{2014} don't end the job with uncommitted \
         work or \"say the word and I'll open the PR\". Never push to main/master, force-push, \
         or merge. If you're working in the user's own checkout instead \u{2014} you never \
         isolated, EnterWorktree failed, or your cwd was already a worktree when the job \
         started (you didn't enter it yourself, so it may be one the user is actively using) \
         \u{2014} ask before committing or switching branches. Skip the PR only if the user \
         said not to open one or there's no remote to push to (then commit and say where the \
         work is)."
            .to_string()
    };
    let tmp_path = std::path::Path::new(job_dir).join("tmp");
    Some(format!(
        "# Background Session\n\nThis session runs as a background job. The user may be \
         chatting with you live or may have stepped away to check results later \u{2014} \
         respond naturally either way, and don't refer to yourself as \"a background \
         agent.\"\n\nUse `$LINGXI_JOB_DIR/tmp` (`{tmp}`) for any temporary files (scripts, \
         query files, intermediate outputs) instead of `/tmp` \u{2014} parallel bg jobs share \
         `/tmp` and clobber each other's files. This directory already exists and is cleaned \
         up when the job is deleted.\n\n{isolation_msg}{shipping}",
        tmp = tmp_path.display()
    ))
}

/// Read the gates from the live environment (`LINGXI_SESSION_KIND` /
/// `LINGXI_JOB_DIR` / `LINGXI_BG_ISOLATION`) and compute the section. `None`
/// for every non-bg session — the assembler stays byte-identical for the
/// interactive path.
#[must_use]
pub fn from_env() -> Option<String> {
    let kind = std::env::var("LINGXI_SESSION_KIND").ok();
    let job_dir = std::env::var("LINGXI_JOB_DIR").ok();
    // Binary `HAo()`: only "worktree"/"none" pass through from the env; other
    // values fall to the (not-yet-ported) config fallbacks → treated as unset.
    let isolation = std::env::var("LINGXI_BG_ISOLATION")
        .ok()
        .filter(|v| v == "worktree" || v == "none");
    section(kind.as_deref(), job_dir.as_deref(), isolation.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_sessions_get_no_section() {
        assert_eq!(section(None, Some("/jobs/ab12"), None), None);
        assert_eq!(section(Some("interactive"), Some("/jobs/ab12"), None), None);
        // bg without a job dir is also gated off (binary: `if(!e)return null`).
        assert_eq!(section(Some("bg"), None, None), None);
        assert_eq!(section(Some("bg"), Some(""), None), None);
    }

    #[test]
    fn default_isolation_carries_enforced_isolate_and_draft_pr_shipping() {
        let s = section(Some("bg"), Some("/home/u/.lingxi/jobs/ab12"), None).unwrap();
        // Byte anchors from the binary `_ff()` (@219583413).
        assert!(s.starts_with("# Background Session\n\n"));
        assert!(s.contains(
            "This session runs as a background job. The user may be chatting with you live \
             or may have stepped away to check results later \u{2014} respond naturally \
             either way, and don't refer to yourself as \"a background agent.\""
        ));
        assert!(s.contains("Use `$LINGXI_JOB_DIR/tmp` (`/home/u/.lingxi/jobs/ab12/tmp`)"));
        assert!(s.contains(
            "This is enforced: file edits in the shared checkout are rejected until you \
             isolate, so call EnterWorktree before your first edit rather than after a \
             rejected attempt."
        ));
        // The 2.1.198 auto-commit/push/draft-PR directive, verbatim.
        assert!(s.contains(
            "Once your work is isolated in a worktree, shipping is part of the task: when \
             you've made code changes, commit them, push the branch, and open a draft PR \
             (`gh pr create --draft`) without stopping to ask \u{2014} don't end the job \
             with uncommitted work or \"say the word and I'll open the PR\"."
        ));
        assert!(s.contains("Never push to main/master, force-push, or merge."));
        // The no-remote degradation path (THIS repo has no remote): commit
        // locally and report, never a hard failure.
        assert!(s.ends_with(
            "Skip the PR only if the user said not to open one or there's no remote to push \
             to (then commit and say where the work is)."
        ));
        // Rebrand: the managed-worktree marker uses the lingxi dot dir.
        assert!(s.contains(&format!("`{}/worktrees/`", branding::DOT_DIR)));
    }

    #[test]
    fn worktree_isolation_gets_first_action_message_and_shipping() {
        let s = section(Some("bg"), Some("/j/x"), Some("worktree")).unwrap();
        assert!(s.contains(
            "This agent is configured with `isolation: worktree`. Call the EnterWorktree \
             tool as your first action"
        ));
        assert!(s.contains("open a draft PR (`gh pr create --draft`)"));
    }

    #[test]
    fn isolation_none_works_in_place_and_never_ships() {
        let s = section(Some("bg"), Some("/j/x"), Some("none")).unwrap();
        assert!(s.contains(
            "Edit files directly in your working directory \u{2014} this session is \
             configured to work in place rather than isolating into a worktree. Skip \
             EnterWorktree unless the user explicitly asks to work in a worktree."
        ));
        // `r = t ? "" : …` — no shipping paragraph in-place.
        assert!(!s.contains("gh pr create"));
        assert!(!s.contains("shipping is part of the task"));
        assert!(s.ends_with("asks to work in a worktree."));
    }
}
