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
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(2);

/// Probe the cwd for a git repo. Returns `None` when cwd is not inside a git
/// repo, the `git` CLI is missing, the repo is unreadable, or HEAD cannot be
/// resolved.
///
/// Only the *presence* of `Some(_)` is consumed by the env block (the
/// `Is a git repository: {true|false}` line); the fields are retained for
/// backward compatibility / other callers.
#[must_use]
pub fn probe(cwd: &Path) -> Option<GitStatus> {
    probe_with_deadline(cwd, Instant::now() + SNAPSHOT_TIMEOUT)
}

fn probe_with_deadline(cwd: &Path, deadline: Instant) -> Option<GitStatus> {
    // Discovery: `git rev-parse --is-inside-work-tree` returns "true" + 0
    // when cwd is in a work tree. Anything else → not a repo.
    let inside = run_git(cwd, deadline, &["rev-parse", "--is-inside-work-tree"])?;
    if inside.trim() != "true" {
        return None;
    }

    // Branch name. `--abbrev-ref HEAD` prints the short branch (e.g. "main")
    // or "HEAD" when detached.
    let branch_raw = run_git(cwd, deadline, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let branch_trim = branch_raw.trim();
    let branch = if branch_trim == "HEAD" {
        "HEAD detached".to_string()
    } else {
        branch_trim.to_string()
    };

    // Dirty-tree detection. `git status --porcelain` is empty when clean,
    // non-empty when there are unstaged / staged / untracked changes.
    let status = run_git(cwd, deadline, &["--no-optional-locks", "status", "--short"])?;
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
    snapshot(cwd)
        .1
        .map(|block| block.trim_start_matches("gitStatus: ").to_owned())
}

/// One bounded probe supplies both env facts and the rendered attachment.
pub(crate) fn snapshot(cwd: &Path) -> (Option<GitStatus>, Option<String>) {
    snapshot_with_deadline(cwd, Instant::now() + SNAPSHOT_TIMEOUT)
}

fn snapshot_with_deadline(cwd: &Path, deadline: Instant) -> (Option<GitStatus>, Option<String>) {
    let Some(inside) = run_git(cwd, deadline, &["rev-parse", "--is-inside-work-tree"]) else {
        return (None, None);
    };
    if inside.trim() != "true" {
        return (None, None);
    }
    // HEAD may be unborn. The attachment still exists in that case (Native
    // uses HEAD), while the legacy env probe requires a resolved branch.
    let branch = run_git(cwd, deadline, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let status = run_git(cwd, deadline, &["--no-optional-locks", "status", "--short"]);
    let probe = branch
        .as_ref()
        .zip(status.as_ref())
        .map(|(branch, status)| GitStatus {
            branch: if branch.trim() == "HEAD" {
                "HEAD detached".into()
            } else {
                branch.trim().into()
            },
            working_dir_clean: status.is_empty(),
            file_changes_summary: String::new(),
        });
    let branch = branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
        .unwrap_or("HEAD");
    let block =
        status_value_from_probe(cwd, deadline, branch, status.as_deref().unwrap_or_default());
    (probe, block.map(|value| format!("gitStatus: {value}")))
}

fn status_value_from_probe(
    cwd: &Path,
    deadline: Instant,
    branch: &str,
    status: &str,
) -> Option<String> {
    // `wO()` — main branch: resolve `origin/HEAD`, strip `origin/`, then pick
    // the first of [detected, "main", "master"] that exists as
    // `refs/remotes/origin/<name>`; else "main".
    let main_branch = resolve_main_branch(cwd, deadline);

    // `git --no-optional-locks status --short` (trimmed) → the Status: body.
    let status_short = status.trim().to_owned();
    // Truncate as `m8r` does (`d8r` cap) with the byte-exact marker.
    let status_body = if status_short.len() > STATUS_TRUNCATE_BYTES {
        let mut boundary = STATUS_TRUNCATE_BYTES;
        while !status_short.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let head = &status_short[..boundary];
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
    let recent_commits = run_git(
        cwd,
        deadline,
        &["--no-optional-locks", "log", "--oneline", "-n", &n],
    )
    .map(|s| s.trim().to_string())
    .unwrap_or_default();

    // `git config user.name` (trimmed) — only included when non-empty.
    let git_user = run_git(cwd, deadline, &["config", "user.name"])
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
fn resolve_main_branch(cwd: &Path, deadline: Instant) -> String {
    let detected = run_git(
        cwd,
        deadline,
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
        if git_ok(
            cwd,
            deadline,
            &["show-ref", "--verify", "--quiet", &reference],
        ) {
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
fn run_git(cwd: &Path, deadline: Instant, args: &[&str]) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    let mut command = Command::new("git");
    command.args(args).current_dir(cwd);
    run_command_with_deadline(&mut command, deadline)
}

/// Own the direct child and its process group until all probe output is done.
/// Drop cleanup also covers errors after spawn and successful Git processes
/// that leave a credential/helper subprocess running in the background.
struct ProbeChild(std::process::Child);

impl Drop for ProbeChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Ok(pid) = i32::try_from(self.0.id()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_command_with_deadline(command: &mut Command, deadline: Instant) -> Option<String> {
    if Instant::now() >= deadline {
        return None;
    }
    // A regular anonymous file avoids a blocking pipe-drainer thread. A
    // descendant inheriting stdout cannot keep a reader alive after its parent
    // exits, and the file is deleted when its final handle closes.
    let mut output = tempfile::tempfile().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = ProbeChild(
        command
            .stdin(Stdio::null())
            .stdout(Stdio::from(output.try_clone().ok()?))
            .stderr(Stdio::null())
            .spawn()
            .ok()?,
    );
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            _ => return None,
        }
    };
    // Kill the owned group before reading, even when Git exited successfully.
    // This retires descendants that inherited output handles; no detached
    // output task or thread survives this function.
    drop(child);
    if !status.success() || Instant::now() >= deadline {
        return None;
    }
    output.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    output.read_to_end(&mut bytes).ok()?;
    if Instant::now() >= deadline {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn git_ok(cwd: &Path, deadline: Instant, args: &[&str]) -> bool {
    run_git(cwd, deadline, args).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn deadline_kills_and_reaps_the_actual_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("echo $$ > \"$1\"; exec sleep 30")
            .arg("git-deadline-test")
            .arg(&pid_file);
        let started = Instant::now();
        assert!(
            run_command_with_deadline(&mut command, started + Duration::from_millis(150)).is_none()
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        assert!(!Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    }

    #[cfg(unix)]
    #[test]
    fn successful_parent_cleans_up_descendant_inheriting_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("descendant-pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30 & echo $! > \"$1\"; printf done")
            .arg("git-descendant-test")
            .arg(&pid_file);
        let started = Instant::now();
        assert_eq!(
            run_command_with_deadline(&mut command, started + Duration::from_secs(2)).as_deref(),
            Some("done")
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        // The shell has exited and cannot reap its helper. Allow the platform's
        // orphan reaper to collect it after our group SIGKILL.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let alive = Command::new("kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success();
            if !alive {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "descendant survived probe cleanup"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn unborn_head_still_has_status_attachment() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        let (probe, block) = snapshot(dir.path());
        assert!(probe.is_none());
        assert!(block.unwrap().contains("Current branch: HEAD"));
    }

    #[test]
    fn combined_snapshot_has_consistent_probe_and_status() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
                "--allow-empty"
            ])
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success());
        std::fs::write(dir.path().join("changed"), "test").unwrap();
        let (probe, block) = snapshot(dir.path());
        assert!(!probe.unwrap().working_dir_clean);
        assert!(block.unwrap().contains("?? changed"));
    }
}
