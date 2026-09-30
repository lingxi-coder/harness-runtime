//! BASH.4 — persistent shell working directory across `BashTool::call`s.
//!
//! Mirrors claude-code `STATE.cwd` semantics (Shell.ts / bashProvider.ts): a
//! foreground `cd` is observed via a `pwd -P` readback written to an internal
//! tracking temp file, so the *next* Bash call spawns under the new directory.
//! Background tasks and subagents must NOT mutate the shared cwd.
//!
//! The tests drive a recording `ProcessRunner` that (i) captures each spawn's
//! cwd + command string and (ii) simulates `pwd -P` by parsing the
//! ` && pwd -P >| '<file>'` readback redirect out of the spawned command and
//! writing a chosen path into that file — exactly what a real shell would do.

use async_trait::async_trait;
use hooks::{CwdChangedFire, CwdChangedFirer};
use lingxi_core::host::process::{ProcessHandle, ProcessRunner};
use lingxi_core::host::sandbox::SandboxedCommand;
use mobile_linux_api::{ProcessError, ProcessOutput};
use serde_json::json;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tool_api::test_support::{fresh_ctx, fresh_tx, shell_test_ctx};
use tool_api::Tool;
use tool_shell::BashTool;

/// Serializes EVERY test whose correctness depends on
/// `LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR` (TFo) having a fixed value. The
/// `maintain_*` test SETS that process-global env var; every other cwd test
/// reads it via `tfo_maintain_cwd` and assumes it is UNSET (so an in-workspace
/// `cd` persists rather than resets). Because `cargo test` runs the file's tests
/// in parallel threads, all of them must take this lock to avoid the env-set of
/// one test perturbing another. `std::env` is process-global; this is the
/// canonical "serialize env-mutating tests" guard.
static MAINTAIN_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Acquire [`MAINTAIN_ENV_LOCK`] tolerant of poisoning: the `()` payload carries
/// no state, so a prior test panicking while holding the lock must not cascade
/// into spurious `PoisonError` unwraps that MASK the original failure.
fn maintain_lock() -> std::sync::MutexGuard<'static, ()> {
    MAINTAIN_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// One captured spawn: the working directory passed to the runner and the
/// assembled command string (the LAST arg — `bash -c -l <cmd>` after BASH.4's
/// login-shell flag, so the command is no longer at a fixed index).
#[derive(Clone)]
struct Spawn {
    cwd: Option<PathBuf>,
    command: String,
}

/// Recording `ProcessRunner`. Each foreground `run` (and background
/// `spawn_background`) is captured. Before returning, `run` pops the next
/// `sim_pwd` entry and, if `Some(path)`, writes that path into the readback
/// tracking file — simulating the shell's `pwd -P` after a `cd`.
struct RecordingRunner {
    fg: Mutex<Vec<Spawn>>,
    bg: Mutex<Vec<Spawn>>,
    sim_pwd: Mutex<VecDeque<Option<PathBuf>>>,
    output: ProcessOutput,
    bg_ok: bool,
}

impl RecordingRunner {
    fn new(sim_pwd: Vec<Option<PathBuf>>, bg_ok: bool) -> Arc<Self> {
        Arc::new(Self {
            fg: Mutex::new(Vec::new()),
            bg: Mutex::new(Vec::new()),
            sim_pwd: Mutex::new(sim_pwd.into_iter().collect()),
            output: ProcessOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                timed_out: false,
            },
            bg_ok,
        })
    }
}

/// Parse the cwd tracking file out of a ` && pwd -P >| '<path>'` readback.
fn extract_cwd_file(command: &str) -> Option<PathBuf> {
    let marker = "pwd -P >| ";
    let idx = command.rfind(marker)?;
    let rest = command[idx + marker.len()..].trim();
    let inner = rest.strip_prefix('\'')?.strip_suffix('\'')?;
    // Undo the shell single-quote escaping `'\''` → `'`.
    Some(PathBuf::from(inner.replace("'\\''", "'")))
}

#[async_trait]
impl ProcessRunner for RecordingRunner {
    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let inner = cmd.inner();
        // The command string is the LAST arg (`-c -l <cmd>` after BASH.4).
        let command = inner.args.last().cloned().unwrap_or_default();
        // Simulate `pwd -P` writing the post-`cd` physical cwd to the tracking
        // file the tool appended to the command.
        // Pop the next simulated readback, but KEEP the last one in place: a
        // real `pwd -P` always reports the shell's cwd, it never runs out. As a
        // one-shot queue this double was order-dependent — any additional
        // `run()` consumed the only entry, the tracking file was never written,
        // and the tool concluded the cwd had not changed. That surfaced under
        // CPU saturation as "0 CwdChanged fires" and "a foreground cd must
        // advance the shared live-cwd cell", both reproducible ~1-in-8 under
        // load and never in isolation.
        let sim = {
            let mut q = self.sim_pwd.lock().unwrap();
            if q.len() > 1 {
                q.pop_front().flatten()
            } else {
                q.front().cloned().flatten()
            }
        };
        if let Some(target) = sim {
            if let Some(file) = extract_cwd_file(&command) {
                std::fs::write(&file, format!("{}\n", target.display())).unwrap();
            }
        }
        self.fg.lock().unwrap().push(Spawn {
            cwd: inner.cwd.clone(),
            command,
        });
        Ok(self.output.clone())
    }

    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        let inner = cmd.inner();
        self.bg.lock().unwrap().push(Spawn {
            cwd: inner.cwd.clone(),
            command: inner.args.last().cloned().unwrap_or_default(),
        });
        if self.bg_ok {
            Ok(ProcessHandle {
                task_id: "task-bg-1".into(),
                pid: 1234,
            })
        } else {
            Err(ProcessError::Unsupported)
        }
    }

    async fn kill(&self, _handle: &ProcessHandle) -> Result<(), ProcessError> {
        Ok(())
    }

    fn is_available(&self) -> bool {
        true
    }
}

/// Build a `BashTool` whose workspace is `workspace` and whose process runner
/// is the supplied recorder. `workspace` seeds `shell_cwd` via `BashTool::new`.
fn tool_with(workspace: &std::path::Path, runner: Arc<RecordingRunner>) -> BashTool {
    let mut ctx = shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.session_cwd
        .swap(workspace.to_path_buf(), ctx.trusted_dirs());
    ctx.process = runner;
    BashTool::new(ctx)
}

#[tokio::test]
async fn foreground_cd_persists_to_next_call() {
    // Depends on TFo (the maintain-cwd env) being UNSET — take the shared lock.
    let _g = maintain_lock();
    // A foreground `cd` to a directory WITHIN the workspace persists to the next
    // call (claude-code `J2n` does not fire — `kF(target)` is contained in the
    // allowed set = the workspace). The target is a real `sub/` subdir under the
    // CANONICAL workspace so the containment check (`is_within_allowed` / `R0`)
    // is deterministic under the macOS `/private/var` realpath folding.
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let target_canon = workspace_canon.join("sub");
    std::fs::create_dir(&target_canon).unwrap();

    // First call simulates `cd <target>` (runner writes target into the
    // readback file); second call writes nothing.
    let runner = RecordingRunner::new(vec![Some(target_canon.clone()), None], false);
    let tool = tool_with(&workspace_canon, runner.clone());

    tool.call(
        json!({ "command": format!("cd {}", target_canon.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("first call ok");

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("second call ok");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2, "two foreground spawns expected");
    // First spawn runs under the initial workspace.
    assert_eq!(fg[0].cwd.as_deref(), Some(workspace_canon.as_path()));
    // Foreground command carries the readback redirect.
    assert!(
        fg[0].command.contains(" && pwd -P >| "),
        "foreground cmd should append pwd readback, got: {}",
        fg[0].command
    );
    // Second spawn inherits the canonicalized post-`cd` (in-workspace) cwd —
    // the cwd was NOT reset because the target is inside the workspace.
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(target_canon.as_path()),
        "second call should spawn under the cd'd directory",
    );
}

/// claude-code has ONE shell spawn for both modes (`vV`); `run_in_background`
/// only decides what happens after the shell is already running in the session
/// cwd. So a `cd` persisted by an earlier FOREGROUND call must be where the
/// next backgrounded command starts — it used to start in the workspace, i.e.
/// somewhere else than the command right before it.
#[tokio::test]
async fn background_spawns_in_the_persisted_shell_cwd() {
    let _g = maintain_lock();
    // The target must be INSIDE the workspace or the `cd` is refused and the
    // shell cwd never advances — without this the test would pass vacuously
    // against the old behaviour.
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let target_canon = workspace_canon.join("sub");
    std::fs::create_dir(&target_canon).unwrap();

    let runner = RecordingRunner::new(vec![Some(target_canon.clone())], true);
    let tool = tool_with(&workspace_canon, runner.clone());

    tool.call(
        json!({ "command": format!("cd {}", target_canon.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("foreground cd ok");

    tool.call(
        json!({ "command": "ls", "run_in_background": true }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("background call ok");

    // Anti-vacuity: the shell STARTED at the workspace, so the bg assertion
    // below is about the `cd` having landed and not about the two paths having
    // been the same all along. (That the `cd` landed is what the bg assertion
    // itself proves — there is no earlier observation of `shell_cwd`.)
    {
        let fg = runner.fg.lock().unwrap();
        assert_eq!(fg.len(), 1, "one foreground spawn expected");
        assert_eq!(fg[0].cwd.as_deref(), Some(workspace_canon.as_path()));
    }
    let bg = runner.bg.lock().unwrap();
    assert_eq!(bg.len(), 1, "one background spawn expected");
    assert_eq!(
        bg[0].cwd.as_deref(),
        Some(target_canon.as_path()),
        "the backgrounded command must start where the shell already is",
    );
}

#[tokio::test]
async fn background_does_not_mutate_shell_cwd_and_omits_readback() {
    let workspace = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    // sim_pwd would only fire on a foreground `run`; the bg call never reads
    // the file. Provide one entry for the trailing foreground probe (None).
    let runner = RecordingRunner::new(vec![None], true);
    let tool = tool_with(workspace.path(), runner.clone());

    // Background `cd` — must not change the shared cwd.
    tool.call(
        json!({
            "command": format!("cd {}", target.path().display()),
            "run_in_background": true,
        }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("background call ok");

    // A later foreground call must still spawn under the original workspace.
    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("foreground call ok");

    let bg = runner.bg.lock().unwrap();
    assert_eq!(bg.len(), 1, "one background spawn expected");
    assert_eq!(bg[0].cwd.as_deref(), Some(workspace.path()));
    assert!(
        !bg[0].command.contains("pwd -P >|"),
        "background cmd must NOT append the readback, got: {}",
        bg[0].command
    );

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 1);
    assert_eq!(
        fg[0].cwd.as_deref(),
        Some(workspace.path()),
        "background call must not have mutated the shared cwd",
    );
}

#[tokio::test]
async fn subagent_does_not_mutate_shared_cwd() {
    let workspace = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    // Both calls simulate a successful `cd <target>` readback. The first runs
    // as a subagent (agent_id set) and must NOT update the shared cwd.
    let runner = RecordingRunner::new(
        vec![
            Some(target.path().to_path_buf()),
            Some(target.path().to_path_buf()),
        ],
        false,
    );
    let tool = tool_with(workspace.path(), runner.clone());

    let mut sub_ctx = fresh_ctx();
    sub_ctx.agent_id = Some(lingxi_core::types::AgentId::new());

    tool.call(
        json!({ "command": format!("cd {}", target.path().display()) }),
        sub_ctx,
        fresh_tx(),
    )
    .await
    .expect("subagent call ok");

    // A subsequent main-thread call must still see the original workspace,
    // proving the subagent's `cd` did not leak into the shared cwd.
    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("main call ok");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(workspace.path()),
        "subagent cd must not persist to a later main-thread call",
    );
}

#[tokio::test]
async fn deleted_cwd_recovers_to_workspace() {
    let workspace = TempDir::new().unwrap();
    let gone = TempDir::new().unwrap();
    let gone_path = gone.path().to_path_buf();

    // First call moves the shell into `gone`; then `gone` is deleted before the
    // second call, forcing the deleted-cwd recovery path.
    let runner = RecordingRunner::new(vec![Some(gone_path.clone()), None], false);
    let tool = tool_with(workspace.path(), runner.clone());

    tool.call(
        json!({ "command": format!("cd {}", gone_path.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("first call ok");

    // Delete the directory the shell cwd now points at.
    drop(gone);

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("second call recovers");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(workspace.path()),
        "deleted cwd must recover to the workspace",
    );
}

#[tokio::test]
async fn deleted_cwd_and_workspace_yields_locked_error() {
    // Workspace itself is gone, so recovery has nowhere to fall back to.
    let dir = TempDir::new().unwrap();
    let ghost = dir.path().to_path_buf();
    drop(dir); // both shell_cwd (seeded from workspace) and workspace are gone

    let runner = RecordingRunner::new(vec![], false);
    let tool = tool_with(&ghost, runner.clone());

    let err = tool
        .call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect_err("missing workspace must error");

    let expected = format!(
        "Working directory \"{}\" no longer exists. Please restart Claude from an existing directory.",
        ghost.display()
    );
    assert!(
        err.to_string().contains(&expected),
        "expected locked recovery-failure string, got: {err}",
    );
    // No spawn should have happened.
    assert!(runner.fg.lock().unwrap().is_empty());
}

// ===== BASH.4 CwdChanged hook firer (Shell.ts:409 onCwdChangedForHooks) ======

/// Records every `(old, new)` it is fired with, so a test can assert the
/// `BashTool` fires `CwdChanged` exactly when the cwd moves.
struct RecordingFirer {
    fires: Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl RecordingFirer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            fires: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl CwdChangedFirer for RecordingFirer {
    async fn fire(&self, fire: CwdChangedFire) {
        self.fires.lock().unwrap().push((fire.old, fire.new));
    }
}

/// `tool_with`, but additionally injecting a `CwdChanged` firer via the
/// optional builder (the desktop-composition wiring path).
fn tool_with_firer(
    workspace: &std::path::Path,
    runner: Arc<RecordingRunner>,
    firer: Arc<dyn CwdChangedFirer>,
) -> BashTool {
    let mut ctx = shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.session_cwd
        .swap(workspace.to_path_buf(), ctx.trusted_dirs());
    ctx.process = runner;
    BashTool::new(ctx).with_cwd_changed_firer(firer)
}

#[tokio::test]
async fn cwd_change_fires_cwd_changed_hook_with_old_and_new() {
    // Depends on TFo (the maintain-cwd env) being UNSET — take the shared lock.
    let _g = maintain_lock();
    // ONE tempdir, and the `cd` target is a real subdirectory inside it. Both
    // `old` and `new` derive from the SAME canonical root, so there is no second
    // independent `TempDir` whose creation/canonicalization can drift under the
    // macOS fseventsd/APFS churn the harness is prone to (a known non-code flake).
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let sub_canon = workspace_canon.join("sub");
    std::fs::create_dir(&sub_canon).unwrap();

    // Seed the shell cwd from the CANONICAL workspace so `old` is deterministic
    // (no `/var` vs `/private/var` raw-vs-canonical ambiguity). Feed the runner
    // the exact canonical target as the `pwd -P` readback.
    let runner = RecordingRunner::new(vec![Some(sub_canon.clone())], false);
    let firer = RecordingFirer::new();
    let tool = tool_with_firer(&workspace_canon, runner, firer.clone());

    tool.call(
        json!({ "command": format!("cd {}", sub_canon.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("call ok");

    let recorded = firer.fires.lock().unwrap();
    assert_eq!(recorded.len(), 1, "exactly one CwdChanged fire expected");
    // 1:1 onCwdChangedForHooks(cwd, newCwd): `old` = prior shell cwd (canonical
    // workspace), `new` = canonicalized post-`cd` cwd (the `sub` directory).
    assert_eq!(
        recorded[0],
        (workspace_canon, sub_canon),
        "CwdChanged must carry (old=workspace, new=sub)"
    );
}

#[tokio::test]
async fn no_cwd_change_does_not_fire() {
    let workspace = TempDir::new().unwrap();

    // No `cd`: the readback writes nothing, so the cwd is unchanged and the
    // firer must NOT be invoked (claude-code's `oldCwd !== newCwd` guard).
    let runner = RecordingRunner::new(vec![None], false);
    let firer = RecordingFirer::new();
    let tool = tool_with_firer(workspace.path(), runner, firer.clone());

    tool.call(json!({ "command": "echo hi" }), fresh_ctx(), fresh_tx())
        .await
        .expect("call ok");

    assert!(
        firer.fires.lock().unwrap().is_empty(),
        "no cwd change => no CwdChanged fire",
    );
}

#[tokio::test]
async fn no_firer_registered_is_a_silent_noop() {
    // Depends on TFo (the maintain-cwd env) being UNSET — take the shared lock.
    let _g = maintain_lock();
    // ONE tempdir + a real `sub` subdir (same churn-robust shape as the firing
    // test) so the canonical paths can't drift under the harness FS flake.
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let sub_canon = workspace_canon.join("sub");
    std::fs::create_dir(&sub_canon).unwrap();

    // `tool_with` builds the plain `BashTool::new(ctx)` (no firer) — the mobile /
    // plain-caller path. A `cd` still updates the persistent cwd (proving the
    // fire is purely additive), but no firer is invoked (nothing to assert about
    // it — the point is no panic and identical cwd behavior).
    let runner = RecordingRunner::new(vec![Some(sub_canon.clone()), None], false);
    let tool = tool_with(&workspace_canon, runner.clone());

    tool.call(
        json!({ "command": format!("cd {}", sub_canon.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("first call ok (no firer registered)");

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("second call ok");

    // The cwd still moved despite no firer — the fire is best-effort/no-op only.
    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(sub_canon.as_path()),
        "cwd update is unchanged when no firer is registered",
    );
}

// ===== P2-08 — Bash is the single writer of the shared live-cwd cell ==========
//
// claude-code keeps ONE session-global live cwd (`Pt.cwd`, read via `getCwd()`).
// The desktop wiring adopts a shared `Arc<Mutex<PathBuf>>` as the `BashTool`'s
// persistent shell cwd, so a foreground `cd` writes the SAME cell the
// file/search/LSP tools and the orchestrator read. A subagent (`preventCwdChanges
// = !isMainThread`) must never mutate it.

/// `tool_with`, but adopting a shared live-cwd cell (the P2-08 desktop wiring).
/// The caller keeps its own clone of `cell` to observe the writes — the mock
/// "consumer" that stands in for Read/Glob/Grep/LSP + the orchestrator.
fn tool_with_live_cwd(
    workspace: &std::path::Path,
    runner: Arc<RecordingRunner>,
    cell: Arc<Mutex<PathBuf>>,
) -> BashTool {
    let mut ctx = shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.session_cwd
        .swap(workspace.to_path_buf(), ctx.trusted_dirs());
    ctx.process = runner;
    BashTool::new(ctx).with_live_cwd(cell)
}

#[tokio::test]
async fn foreground_cd_advances_the_shared_live_cwd_cell() {
    // Depends on TFo (the maintain-cwd env) being UNSET — take the shared lock.
    let _g = maintain_lock();
    // A foreground `cd` to an in-workspace subdir writes the post-`cd` directory
    // into the injected shared cell, so a consumer holding a clone of the Arc
    // observes it (single writer, 1:1 with CC's getCwd()/setCwdState).
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let sub_canon = workspace_canon.join("sub");
    std::fs::create_dir(&sub_canon).unwrap();

    let cell = Arc::new(Mutex::new(workspace_canon.clone()));
    let runner = RecordingRunner::new(vec![Some(sub_canon.clone())], false);
    let tool = tool_with_live_cwd(&workspace_canon, runner, cell.clone());

    // Before the cd the consumer sees the workspace.
    assert_eq!(*cell.lock().unwrap(), workspace_canon);

    tool.call(
        json!({ "command": format!("cd {}", sub_canon.display()) }),
        fresh_ctx(),
        fresh_tx(),
    )
    .await
    .expect("cd call ok");

    // The shared cell now holds the canonical post-`cd` directory — the same live
    // cwd the file/search/LSP tools read.
    assert_eq!(
        *cell.lock().unwrap(),
        sub_canon,
        "a foreground cd must advance the shared live-cwd cell",
    );
}

#[tokio::test]
async fn subagent_cd_does_not_advance_the_shared_live_cwd_cell() {
    // A subagent (agent_id set) runs each command per-call and must NEVER move
    // the shared session cwd (CC `preventCwdChanges = !isMainThread`), so the
    // injected shared cell is left untouched.
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let sub_canon = workspace_canon.join("sub");
    std::fs::create_dir(&sub_canon).unwrap();

    let cell = Arc::new(Mutex::new(workspace_canon.clone()));
    let runner = RecordingRunner::new(vec![Some(sub_canon.clone())], false);
    let tool = tool_with_live_cwd(&workspace_canon, runner, cell.clone());

    let mut sub_ctx = fresh_ctx();
    sub_ctx.agent_id = Some(lingxi_core::types::AgentId::new());
    tool.call(
        json!({ "command": format!("cd {}", sub_canon.display()) }),
        sub_ctx,
        fresh_tx(),
    )
    .await
    .expect("subagent call ok");

    assert_eq!(
        *cell.lock().unwrap(),
        workspace_canon,
        "a subagent cd must not mutate the shared live-cwd cell",
    );
}

// ===== Finding #8 — `J2n` cwd-reset when the shell leaves the allowed dirs ====
//
// claude-code `J2n`: after a command, if the shell cwd moved away from the
// original (workspace) AND is NOT contained in an allowed dir — or the env
// `LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR` forces it — chdir back to the
// original, emit `tengu_bash_tool_reset_to_original_dir` (non-env branch only),
// and append `Y2n`'s `\nShell cwd was reset to {original}` to stderr.

/// Read the model-facing `stderr` field out of a Bash `ToolCallResult`.
fn result_stderr(data: &serde_json::Value) -> String {
    data.get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Build a `(workspace, outside)` pair under ONE shared `TempDir` root: `ws/` is
/// the workspace and `outside/` is its SIBLING (genuinely outside the workspace
/// subtree). Both derive from the SAME canonical root, so only one `TempDir` is
/// created/canonicalized per test — the churn-robust shape the firer tests use,
/// which keeps the macOS fseventsd/APFS canonicalize flake from perturbing the
/// reset tests under parallel load. Returns `(root, ws_canon, outside_canon)`;
/// the `TempDir` must be kept alive by the caller.
fn workspace_and_outside() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = TempDir::new().unwrap();
    let root_canon = std::fs::canonicalize(root.path()).unwrap();
    let ws = root_canon.join("ws");
    let outside = root_canon.join("outside");
    std::fs::create_dir(&ws).unwrap();
    std::fs::create_dir(&outside).unwrap();
    (root, ws, outside)
}

#[tokio::test]
async fn cd_outside_workspace_resets_and_warns() {
    let _g = maintain_lock();
    // The `cd` target is a SIBLING of the workspace (outside its subtree), so
    // `J2n` fires: cwd resets to the workspace and stderr gains the `Y2n` warning.
    let (_root, workspace_canon, outside_canon) = workspace_and_outside();

    let runner = RecordingRunner::new(vec![Some(outside_canon.clone()), None], false);
    let tool = tool_with(&workspace_canon, runner.clone());

    let res = tool
        .call(
            json!({ "command": format!("cd {}", outside_canon.display()) }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("first call ok");

    // The reset warning is appended to the (empty) stderr: `"".trim()` == "" so
    // the result is exactly `\nShell cwd was reset to {workspace}`.
    assert_eq!(
        result_stderr(&res.data),
        format!("\nShell cwd was reset to {}", workspace_canon.display()),
        "out-of-workspace cd must append the Y2n reset warning to stderr",
    );

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("second call ok");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    // The shell cwd was reset — the second call spawns under the workspace, NOT
    // the out-of-workspace target.
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(workspace_canon.as_path()),
        "out-of-workspace cd must reset the shell cwd to the workspace",
    );
}

#[tokio::test]
async fn cd_outside_workspace_preserves_existing_stderr() {
    let _g = maintain_lock();
    // Same as above but with non-empty command stderr. NOTE the binary divergence
    // (documented residual): claude-code's BASH tool result `stderr` field carries
    // ONLY the reset warning (`d = Y2n("")`; the command's stderr folds into stdout
    // via the shell, `stderr_length:0` in its telemetry). LingXi surfaces command
    // stderr in the result `stderr` field (a PRE-EXISTING design difference, not
    // introduced by this finding), so the faithful adaptation appends the warning
    // to it: `${stderr.trim()}\nShell cwd was reset to {workspace}`. In claude-code's
    // only real case (empty command stderr) this is byte-identical to `Y2n("")`.
    let (_root, workspace_canon, outside_canon) = workspace_and_outside();

    let runner = Arc::new(StderrRunner {
        stderr: "boom\n".into(),
        inner: RecordingRunner::new(vec![Some(outside_canon.clone())], false),
    });
    let mut ctx = shell_test_ctx(ProcessOutput {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        timed_out: false,
    });
    ctx.session_cwd
        .swap(workspace_canon.clone(), ctx.trusted_dirs());
    ctx.process = runner.clone();
    let tool = BashTool::new(ctx);

    let res = tool
        .call(
            json!({ "command": format!("cd {}", outside_canon.display()) }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("call ok");

    assert_eq!(
        result_stderr(&res.data),
        format!("boom\nShell cwd was reset to {}", workspace_canon.display()),
        "existing stderr is trimmed then the reset warning is appended",
    );
}

/// RAII guard: removes `LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR` on drop so a
/// panicking assert in the `maintain_*` test can NEVER leave the process-global
/// env var set (which would poison every other cwd test in the file).
struct MaintainEnvGuard;
impl Drop for MaintainEnvGuard {
    fn drop(&mut self) {
        std::env::remove_var("LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR");
    }
}

#[tokio::test]
async fn maintain_env_resets_even_in_workspace() {
    let _g = maintain_lock();
    // `LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR=1` (TFo) forces a reset even for
    // an in-workspace `cd` that would otherwise persist.
    let workspace = TempDir::new().unwrap();
    let workspace_canon = std::fs::canonicalize(workspace.path()).unwrap();
    let sub_canon = workspace_canon.join("sub");
    std::fs::create_dir(&sub_canon).unwrap();

    let runner = RecordingRunner::new(vec![Some(sub_canon.clone()), None], false);
    let tool = tool_with(&workspace_canon, runner.clone());

    std::env::set_var("LINGXI_BASH_MAINTAIN_PROJECT_WORKING_DIR", "1");
    // Restored on scope exit even if an assert below panics (keeps the env var
    // from leaking to the other lock-holders).
    let _env_guard = MaintainEnvGuard;
    let res = tool
        .call(
            json!({ "command": format!("cd {}", sub_canon.display()) }),
            fresh_ctx(),
            fresh_tx(),
        )
        .await
        .expect("first call ok");

    // The warning still appends (TFo's `r` branch sets `h=Y2n("")`), but the
    // `tengu_bash_tool_reset_to_original_dir` telemetry does NOT fire (the
    // `if(!r)` guard) — not observable here beyond the inline tracing event.
    assert_eq!(
        result_stderr(&res.data),
        format!("\nShell cwd was reset to {}", workspace_canon.display()),
        "TFo forces a reset warning even for an in-workspace cd",
    );

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("second call ok");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(workspace_canon.as_path()),
        "TFo forces the shell cwd back to the workspace even for an in-workspace cd",
    );
}

#[tokio::test]
async fn subagent_is_unaffected_by_reset() {
    let _g = maintain_lock();
    // A subagent call carries `agent_id` (`prevent_cwd_changes`), so the entire
    // readback block — including the `J2n` reset — is skipped. No reset warning,
    // and the shared cwd is untouched (it was never advanced for a subagent).
    let (_root, workspace_canon, outside_canon) = workspace_and_outside();

    let runner = RecordingRunner::new(vec![Some(outside_canon.clone()), None], false);
    let tool = tool_with(&workspace_canon, runner.clone());

    let mut sub_ctx = fresh_ctx();
    sub_ctx.agent_id = Some(lingxi_core::types::AgentId::new());

    let res = tool
        .call(
            json!({ "command": format!("cd {}", outside_canon.display()) }),
            sub_ctx,
            fresh_tx(),
        )
        .await
        .expect("subagent call ok");

    // No reset warning for a subagent (the readback block is gated out).
    assert_eq!(
        result_stderr(&res.data),
        "",
        "a subagent must not get the reset warning (readback block is skipped)",
    );

    tool.call(json!({ "command": "pwd" }), fresh_ctx(), fresh_tx())
        .await
        .expect("main call ok");

    let fg = runner.fg.lock().unwrap();
    assert_eq!(fg.len(), 2);
    assert_eq!(
        fg[1].cwd.as_deref(),
        Some(workspace_canon.as_path()),
        "subagent cd neither advances nor resets the shared cwd",
    );
}

/// A `ProcessRunner` that returns a fixed stderr on the foreground `run`, while
/// still simulating the `pwd -P` readback (so the reset path can be exercised
/// with non-empty stderr). Delegates capture/readback to an inner
/// `RecordingRunner`.
struct StderrRunner {
    stderr: String,
    inner: Arc<RecordingRunner>,
}

#[async_trait]
impl ProcessRunner for StderrRunner {
    async fn run(&self, cmd: &SandboxedCommand) -> Result<ProcessOutput, ProcessError> {
        let mut out = self.inner.run(cmd).await?;
        out.stderr = self.stderr.clone();
        Ok(out)
    }
    async fn spawn_background(
        &self,
        cmd: &SandboxedCommand,
    ) -> Result<ProcessHandle, ProcessError> {
        self.inner.spawn_background(cmd).await
    }
    async fn kill(&self, handle: &ProcessHandle) -> Result<(), ProcessError> {
        self.inner.kill(handle).await
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
}
