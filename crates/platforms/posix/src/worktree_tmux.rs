//! Builds + runs the `tmux new-session` invocation used to give a freshly
//! created worktree its own detached tmux session.
//!
//! Mirrors claude-code 2.1.206's worktree-tmux creation call: `Ur("tmux",
//! ["new-session","-d","-s",name,"-c",path])`, which returns `{created:
//! false, error}` on a non-zero exit and success otherwise (binary
//! @216347493). This module is intentionally ISOLATED from
//! `swarm/tmux.rs` (which is pane-oriented, for the multi-agent swarm
//! feature) — worktree tmux sessions are a separate, session-oriented
//! concern. It is not yet wired into boot or `ExitWorktree`; see
//! `docs/superpowers/plans/2026-07-14-worktree-tmux-launch.md` Tasks 3-5.

use std::collections::HashMap;
use std::path::Path;

use lingxi_core::host::{ProcessCommand, ProcessRunner, Sandbox};

/// Audit reason stamped on the `tmux new-session` command handed to the
/// [`ProcessRunner`]. This is an internal infra invocation (not a
/// user/agent-issued Bash command), so it goes through
/// [`Sandbox::bypass_with_audit`] the same way hook commands do (see
/// `hooks/src/executor.rs`'s `"hook_command"` bypass), which both mints the
/// `SandboxedCommand` and records the audit trail via the real `Sandbox`
/// impl (e.g. `PosixSandbox::bypass_with_audit`'s `tracing::warn!`).
const WORKTREE_TMUX_AUDIT_REASON: &str = "worktree_tmux_new_session";

/// Build the argv (excluding the `tmux` program name itself) for creating a
/// detached tmux session rooted at `worktree_path`:
/// `new-session -d -s <session_name> -c <worktree_path>`.
#[must_use]
pub fn build_worktree_tmux_argv(session_name: &str, worktree_path: &Path) -> Vec<String> {
    vec![
        "new-session".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        session_name.to_string(),
        "-c".to_string(),
        worktree_path.to_string_lossy().into_owned(),
    ]
}

/// Run `tmux new-session -d -s <session_name> -c <worktree_path>` through
/// the given [`ProcessRunner`] seam. Maps a non-zero exit to
/// `Err(stderr)` (mirrors 206's `{created:false,error}`); a zero exit
/// (or any output) maps to `Ok(())`. A runner-level [`mobile_linux_api::ProcessError`]
/// is stringified into the `Err`.
///
/// `sandbox` mints the `SandboxedCommand` via [`Sandbox::bypass_with_audit`]
/// (not the internal `SandboxedCommand::__new_sandboxed` constructor) so the
/// audit hook the real `Sandbox` impl runs on every bypass actually fires
/// for this infra-issued `tmux` invocation.
pub async fn create_worktree_tmux_session(
    runner: &dyn ProcessRunner,
    sandbox: &dyn Sandbox,
    session_name: &str,
    worktree_path: &Path,
) -> Result<(), String> {
    let pcmd = ProcessCommand {
        command: "tmux".to_string(),
        args: build_worktree_tmux_argv(session_name, worktree_path),
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(pcmd, WORKTREE_TMUX_AUDIT_REASON);
    let output = runner.run(&sandboxed).await.map_err(|e| e.to_string())?;
    if output.exit_code != 0 {
        return Err(output.stderr);
    }
    Ok(())
}

/// Audit reason for the `tmux -V` install probe.
const WORKTREE_TMUX_PROBE_AUDIT_REASON: &str = "worktree_tmux_probe";

/// Probe whether `tmux` is installed by running `tmux -V` and checking for a
/// zero exit — byte-faithful to 206's `i4i()` install check (`{code} =
/// Ur("tmux", ["-V"]); return code === 0`, binary @216346996). A runner-level
/// error (spawn failure, tmux not on PATH) counts as "not installed"
/// (`false`), matching the JS `code === 0` semantics where a failed spawn is
/// non-zero.
pub async fn tmux_is_installed(runner: &dyn ProcessRunner, sandbox: &dyn Sandbox) -> bool {
    let pcmd = ProcessCommand {
        command: "tmux".to_string(),
        args: vec!["-V".to_string()],
        cwd: None,
        env: HashMap::new(),
        timeout: None,
        stdin: None,
    };
    let sandboxed = sandbox.bypass_with_audit(pcmd, WORKTREE_TMUX_PROBE_AUDIT_REASON);
    matches!(runner.run(&sandboxed).await, Ok(out) if out.exit_code == 0)
}

/// Platform-specific "how to install tmux" hint — byte-faithful to 206's
/// `s4i()` (binary @216347060). 206 switches on its `Ut()` platform enum
/// (`macos` / `linux` / `wsl` / `windows` / default); this maps
/// [`std::env::consts::OS`] to the same strings, folding `wsl` into the
/// `linux` arm (they share the identical message in 206) since Rust's `OS`
/// const reports WSL as `"linux"`.
#[must_use]
pub fn tmux_install_hint() -> &'static str {
    match std::env::consts::OS {
        "macos" => "Install tmux with: brew install tmux",
        "linux" => {
            "Install tmux with: sudo apt install tmux (Debian/Ubuntu) or sudo dnf install tmux (Fedora/RHEL)"
        }
        "windows" => "tmux is not natively available on Windows. Consider using WSL or Cygwin.",
        _ => "Install tmux using your system package manager.",
    }
}

/// Derives the tmux `-s` session name for a worktree, given the git repo
/// root and the worktree's name (e.g. a branch-derived slug like
/// `"feature/x"` or `"pr-123"`).
///
/// **Byte-faithful to the 2.1.206 binary**: the `--worktree`/`--tmux` CLI
/// site (@225872136) computes the session name as
/// `bWn(repoRoot, Xvt(worktreeName))`, where
/// - `DYc(name) = name.replaceAll("/", "+")` (@216338465)
/// - `Xvt(name) = "worktree-" + DYc(name)` (@216338465)
/// - `bWn(e, t) = (kd.basename(e) + "_" + t).replace(/[/.]/g, "_")`
///   (@216338294)
///
/// i.e. 206's real session name embeds BOTH the git repo root's basename
/// AND the worktree name: `/` in the worktree name is swapped to `+` first
/// (`DYc`/`Xvt`), then any remaining `/` or `.` in the whole
/// `{basename(repoRoot)}_worktree-{name}` string (from either half) is
/// squashed to `_` (`bWn`'s trailing `.replace`).
///
/// This fn implements that composition directly:
/// - `base` = `repo_root`'s file name (the `kd.basename(e)` analog; empty
///   string if `repo_root` has no file-name component).
/// - `branch` = `"worktree-" + worktree_name.replace('/', "+")` (the `Xvt`
///   analog).
/// - the result is `"{base}_{branch}"` with every `/` and `.` in the WHOLE
///   string replaced with `_` (the `bWn` analog) — this final pass also
///   catches a `.` inside the repo-root basename itself (e.g. `my.repo`),
///   matching 206's behavior of running the squash over the full string,
///   not just the worktree-name half.
///
/// We additionally fold `:` and whitespace to `_`. 206 doesn't do this
/// because the worktree names it feeds in (branch-derived slugs) are
/// already colon/whitespace-free by construction; this fn accepts an
/// arbitrary `&str`, so the extra folding is a defensive addition — tmux
/// treats `:` and whitespace specially in `-t`/`-s` target syntax.
#[must_use]
pub fn worktree_tmux_session_name(repo_root: &Path, worktree_name: &str) -> String {
    let base = repo_root.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let branch = format!("worktree-{}", worktree_name.replace('/', "+"));
    let combined = format!("{base}_{branch}");
    combined
        .chars()
        .map(|c| {
            if c == '.' || c == '/' || c == ':' || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::PosixSandbox;
    use async_trait::async_trait;
    use lingxi_core::host::{ProcessHandle, SandboxedCommand};
    use mobile_linux_api::{ProcessError, ProcessOutput};
    use std::path::PathBuf;
    use std::sync::Mutex;

    #[test]
    fn build_worktree_tmux_argv_shape_is_exact() {
        let argv = build_worktree_tmux_argv("wt-feat", Path::new("/repo/.lingxi/worktrees/feat"));
        assert_eq!(
            argv,
            vec![
                "new-session".to_string(),
                "-d".to_string(),
                "-s".to_string(),
                "wt-feat".to_string(),
                "-c".to_string(),
                "/repo/.lingxi/worktrees/feat".to_string(),
            ]
        );
    }

    /// Mock `ProcessRunner` that returns a canned exit code + stderr and
    /// records the `tmux` argv it was handed, mirroring the `MockRunner`
    /// pattern used in `hooks/src/executor_test.rs`.
    struct MockRunner {
        exit_code: i32,
        stderr: String,
        recorded_command: Mutex<Option<String>>,
        recorded_args: Mutex<Option<Vec<String>>>,
    }

    impl MockRunner {
        fn new(exit_code: i32, stderr: &str) -> Self {
            Self {
                exit_code,
                stderr: stderr.to_string(),
                recorded_command: Mutex::new(None),
                recorded_args: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl ProcessRunner for MockRunner {
        async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
            *self.recorded_command.lock().unwrap() = Some(cmd.inner().command.clone());
            *self.recorded_args.lock().unwrap() = Some(cmd.inner().args.clone());
            Ok(ProcessOutput {
                stdout: String::new(),
                stderr: self.stderr.clone(),
                exit_code: self.exit_code,
                timed_out: false,
            })
        }

        async fn spawn_background(
            &self,
            _cmd: &SandboxedCommand,
        ) -> Result<ProcessHandle, ProcessError> {
            Err(ProcessError::Unsupported)
        }

        async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
            Ok(())
        }

        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn zero_exit_maps_to_ok_and_uses_the_argv() {
        let runner = MockRunner::new(0, "");
        let sandbox = PosixSandbox::new();
        let path = PathBuf::from("/tmp/wt-x");
        let result = create_worktree_tmux_session(&runner, &sandbox, "wt-x", &path).await;
        assert!(result.is_ok(), "expected Ok, got {result:?}");
        assert_eq!(
            *runner.recorded_command.lock().unwrap(),
            Some("tmux".to_string())
        );
        assert_eq!(
            *runner.recorded_args.lock().unwrap(),
            Some(build_worktree_tmux_argv("wt-x", &path))
        );
    }

    #[tokio::test]
    async fn nonzero_exit_maps_to_err_with_stderr() {
        let runner = MockRunner::new(1, "boom");
        let sandbox = PosixSandbox::new();
        let path = PathBuf::from("/tmp/wt-y");
        let result = create_worktree_tmux_session(&runner, &sandbox, "wt-y", &path).await;
        assert_eq!(result, Err("boom".to_string()));
    }

    #[tokio::test]
    async fn tmux_is_installed_true_on_zero_exit_and_probes_dash_v() {
        let runner = MockRunner::new(0, "");
        let sandbox = PosixSandbox::new();
        assert!(tmux_is_installed(&runner, &sandbox).await);
        // 206's `i4i()` runs exactly `tmux -V`.
        assert_eq!(
            *runner.recorded_command.lock().unwrap(),
            Some("tmux".to_string())
        );
        assert_eq!(
            *runner.recorded_args.lock().unwrap(),
            Some(vec!["-V".to_string()])
        );
    }

    #[tokio::test]
    async fn tmux_is_installed_false_on_nonzero_exit() {
        let runner = MockRunner::new(127, "tmux: command not found");
        let sandbox = PosixSandbox::new();
        assert!(!tmux_is_installed(&runner, &sandbox).await);
    }

    #[test]
    fn tmux_install_hint_matches_206_for_this_platform() {
        // Byte-lock the hint for whichever platform the test runs on against
        // 206's `s4i()`.
        let expected = match std::env::consts::OS {
            "macos" => "Install tmux with: brew install tmux",
            "linux" => "Install tmux with: sudo apt install tmux (Debian/Ubuntu) or sudo dnf install tmux (Fedora/RHEL)",
            "windows" => "tmux is not natively available on Windows. Consider using WSL or Cygwin.",
            _ => "Install tmux using your system package manager.",
        };
        assert_eq!(tmux_install_hint(), expected);
        assert!(!tmux_install_hint().is_empty());
    }

    #[test]
    fn session_name_is_deterministic() {
        let repo = Path::new("/Users/x/myrepo");
        assert_eq!(
            worktree_tmux_session_name(repo, "feature/x"),
            worktree_tmux_session_name(repo, "feature/x")
        );
    }

    #[test]
    fn session_name_is_tmux_legal() {
        let repo = Path::new("/Users/x/myrepo");
        for input in ["feature/x", "release/1.2.3", "pr 123", "weird:name", ""] {
            let name = worktree_tmux_session_name(repo, input);
            assert!(
                !name.contains('.')
                    && !name.contains(':')
                    && !name.chars().any(char::is_whitespace),
                "session name {name:?} (from {input:?}) is not tmux -s legal"
            );
        }
    }

    #[test]
    fn session_name_matches_documented_scheme() {
        // bWn(repoRoot, Xvt("feature/x")) = basename("/Users/x/myrepo") +
        // "_" + "worktree-feature+x" = "myrepo_worktree-feature+x" (no dots
        // to squash).
        assert_eq!(
            worktree_tmux_session_name(Path::new("/Users/x/myrepo"), "feature/x"),
            "myrepo_worktree-feature+x"
        );
        // A worktree name with a dot exercises 206's `/[/.]/g -> "_"` squash.
        assert_eq!(
            worktree_tmux_session_name(Path::new("/Users/x/myrepo"), "release/1.2.3"),
            "myrepo_worktree-release+1_2_3"
        );
    }

    #[test]
    fn session_name_squashes_dot_in_repo_root_basename() {
        // The `bWn` squash (`.replace(/[/.]/g, "_")`) runs over the WHOLE
        // `{basename}_{tag}` string, so a `.` in the repo-root basename
        // itself must also be folded to `_`, not just dots in the
        // worktree-name half.
        assert_eq!(
            worktree_tmux_session_name(Path::new("/x/my.repo"), "feature/x"),
            "my_repo_worktree-feature+x"
        );
    }

    #[test]
    fn session_name_is_non_empty_for_edge_input() {
        assert!(!worktree_tmux_session_name(Path::new("/x/myrepo"), "").is_empty());
    }
}
