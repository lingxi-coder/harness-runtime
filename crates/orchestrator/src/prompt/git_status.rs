//! Git status probe — produces `Option<GitStatus>` for the env-block git
//! bool, plus [`status_block`] which builds the v2.1.183 `gitStatus` system-
//! prompt attachment (claude-code `m8r`, binary offset ~197199600).
//!
//! Shells out to the `git` CLI for read-only repo discovery. This is
//! intentionally simpler than a `gix` integration: the workspace pins Rust
//! 1.82 (no edition 2024), and recent `gix` transitives require edition 2024.
//!
//! Errors at any step degrade gracefully to `None` so the assembler falls back
//! to the non-git env block / omits the gitStatus attachment without panicking.
#![forbid(unsafe_code)]

use crate::prompt::GitStatus;
use std::path::Path;
use std::process::Command;

/// Probe the cwd for a git repo. Returns `None` when cwd is not inside a git
/// repo, the `git` CLI is missing, the repo is unreadable, or HEAD cannot be
/// resolved.
///
/// Only the *presence* of `Some(_)` is consumed by the env block (the
/// `Is a git repository: {true|false}` line); the fields are retained for
/// backward compatibility / other callers.
#[must_use]
pub fn probe(cwd: &Path) -> Option<GitStatus> {
    // Discovery: `git rev-parse --is-inside-work-tree` returns "true" + 0
    // when cwd is in a work tree. Anything else → not a repo.
    let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }

    // Branch name. `--abbrev-ref HEAD` prints the short branch (e.g. "main")
    // or "HEAD" when detached.
    let branch_raw = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch_trim = branch_raw.trim();
    let branch = if branch_trim == "HEAD" {
        "HEAD detached".to_string()
    } else {
        branch_trim.to_string()
    };

    // Dirty-tree detection. `git status --porcelain` is empty when clean,
    // non-empty when there are unstaged / staged / untracked changes.
    let status = run_git(cwd, &["status", "--porcelain"])?;
    let working_dir_clean = status.is_empty();

    Some(GitStatus {
        branch,
        working_dir_clean,
        file_changes_summary: String::new(),
    })
}

/// Number of recent commits captured by the `gitStatus` attachment.
/// claude-code `m8r` runs `git log --oneline -n 5` (binary offset ~197199900).
const RECENT_COMMITS_N: usize = 5;

/// `d8r` — the byte cap above which the `git status --short` block is
/// truncated. claude-code uses 2 KB (the truncation marker reads "exceeds 2k
/// characters", binary offset ~197200698).
const STATUS_TRUNCATE_BYTES: usize = 2000;

/// Build the v2.1.183 `gitStatus` system-prompt attachment value, EXCLUDING
/// the `gitStatus: ` key prefix (the caller — [`render_git_status_block`] —
/// adds it, mirroring claude-code `WZa`'s `${key}: ${value}`).
///
/// 1:1 with claude-code `m8r`: returns `None` when cwd is not a git repo or any
/// probe fails (`m8r` returns `null`, and `hE` then omits the `gitStatus` key).
/// On success the value is an array joined by a BLANK LINE (`\n\n`):
/// ```text
/// This is the git status at the start of the conversation. Note that this status is a snapshot in time, and will not update during the conversation.
///
/// Current branch: {branch}
///
/// Main branch (you will usually use this for PRs): {main}
///
/// Git user: {user}            (omitted when `git config user.name` is empty)
///
/// Status:
/// {git status --short, or "(clean)"}
///
/// Recent commits:
/// {git log --oneline -n 5}
/// ```
#[must_use]
pub fn status_value(cwd: &Path) -> Option<String> {
    // `vy()` — is this a git repo at all? Reuse the porcelain probe.
    let inside = run_git(cwd, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }

    // `wy()` — current branch: `git rev-parse --abbrev-ref HEAD`, or "HEAD".
    let branch = run_git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
        .map(|s| {
            let t = s.trim();
            if t.is_empty() {
                "HEAD".to_string()
            } else {
                t.to_string()
            }
        })
        .unwrap_or_else(|| "HEAD".to_string());

    // `wO()` — main branch: resolve `origin/HEAD`, strip `origin/`, then pick
    // the first of [detected, "main", "master"] that exists as
    // `refs/remotes/origin/<name>`; else "main".
    let main_branch = resolve_main_branch(cwd);

    // `git --no-optional-locks status --short` (trimmed) → the Status: body.
    let status_short = run_git(cwd, &["--no-optional-locks", "status", "--short"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    // Truncate as `m8r` does (`d8r` cap) with the byte-exact marker.
    let status_body = if status_short.len() > STATUS_TRUNCATE_BYTES {
        let head = &status_short[..STATUS_TRUNCATE_BYTES];
        format!(
            "{head}\n... (truncated because it exceeds 2k characters. \
If you need more information, run \"git status\" using {})",
            shell_name()
        )
    } else {
        status_short
    };
    // `${u||"(clean)"}` — empty status renders "(clean)".
    let status_body = if status_body.is_empty() {
        "(clean)".to_string()
    } else {
        status_body
    };

    // `git --no-optional-locks log --oneline -n 5` (trimmed) → Recent commits.
    let n = RECENT_COMMITS_N.to_string();
    let recent_commits = run_git(cwd, &["--no-optional-locks", "log", "--oneline", "-n", &n])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // `git config user.name` (trimmed) — only included when non-empty.
    let git_user = run_git(cwd, &["config", "user.name"])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // Assemble the array and join with a blank line (`.join("\n\n")`).
    let mut parts: Vec<String> = Vec::with_capacity(6);
    parts.push(
        "This is the git status at the start of the conversation. Note that this status is a \
snapshot in time, and will not update during the conversation."
            .to_string(),
    );
    parts.push(format!("Current branch: {branch}"));
    parts.push(format!(
        "Main branch (you will usually use this for PRs): {main_branch}"
    ));
    if let Some(user) = git_user {
        parts.push(format!("Git user: {user}"));
    }
    parts.push(format!("Status:\n{status_body}"));
    parts.push(format!("Recent commits:\n{recent_commits}"));

    Some(parts.join("\n\n"))
}

/// Resolve the main branch (`wO`): `git symbolic-ref --short
/// refs/remotes/origin/HEAD`, strip a leading `origin/`, then return the first
/// of `[detected, "main", "master"]` that exists as
/// `refs/remotes/origin/<name>`; falling back to `"main"`.
fn resolve_main_branch(cwd: &Path) -> String {
    let detected = run_git(
        cwd,
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )
    .map(|s| s.trim().trim_start_matches("origin/").to_string())
    .filter(|s| !s.is_empty());

    let candidates: Vec<String> = match &detected {
        Some(d) => vec![d.clone(), "main".to_string(), "master".to_string()],
        None => vec!["main".to_string(), "master".to_string()],
    };
    for cand in candidates {
        let reference = format!("refs/remotes/origin/{cand}");
        if git_ok(cwd, &["show-ref", "--verify", "--quiet", &reference]) {
            return cand;
        }
    }
    "main".to_string()
}

/// Render the full system-prompt `gitStatus` block — `gitStatus: ` followed by
/// [`status_value`] — matching claude-code `WZa`'s `${key}: ${value}` for the
/// `systemContext.gitStatus` entry. `None` when cwd is not a git repo.
///
/// This is appended by the assembler to the SYSTEM PROMPT as a trailing dynamic
/// cache block (claude-code threads it through `WZa(systemPromptArray,
/// systemContext)` → `getSystemPrompt`'s output), NOT as a user message.
#[must_use]
pub fn render_git_status_block(cwd: &Path) -> Option<String> {
    status_value(cwd).map(|v| format!("gitStatus: {v}"))
}

/// The shell name used in the truncation marker (`Su()?ns:Js` — bash on POSIX).
fn shell_name() -> &'static str {
    if cfg!(windows) {
        "PowerShell"
    } else {
        "bash"
    }
}

/// Run `git <args>` in `cwd` and return stdout when the command succeeded.
/// Returns `None` on any failure (binary missing, non-zero exit, unreadable
/// repo, …).
fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Like [`run_git`] but returns only success/failure (for `show-ref --verify`).
fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
