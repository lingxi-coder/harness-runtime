//! Print/SDK-mode registry of detached foreground Bash children.
//!
//! Parity with claude-code 2.1.212 ("Fixed SIGTERM during Bash tool orphaning
//! process trees in print/SDK mode"). claude-code spawns Bash tool commands
//! detached (`detached: true` ⇒ `setsid`, own process group) and registers a
//! `signal-exit` `onExit` handler for `[SIGHUP, SIGINT, SIGTERM]` that
//! `process.kill(-pid, "SIGTERM")`s each live group before the process exits.
//! Without it, an abrupt `SIGTERM` delivered to `claude -p` / an SDK host leaves
//! the Bash subtree orphaned and still running.
//!
//! LingXi's foreground [`super::runner::PosixProcess::run`] normally relies on
//! tokio `kill_on_drop(true)`, which only fires on a graceful future drop — never
//! on an abrupt process-level `SIGTERM`, because tokio runs no destructors when
//! the whole process is signalled. In print/SDK mode we therefore
//!   1. spawn each foreground child in its own process group (`setsid`) and record
//!      its pgid here, and
//!   2. let the CLI install a `SIGTERM`/`SIGHUP`/`SIGINT` handler that calls
//!      [`kill_all_active_children`] right before the process exits.
//!
//! Interactive (TUI/REPL) mode is left completely unchanged: the flag stays off,
//! so `run` keeps its exact prior behavior (no `setsid`, no registration) and the
//! cleanup handler is never installed.

use crate::process::kill_tree::kill_tree_force;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Set once by the CLI (via [`enable_print_mode_child_cleanup`]) when running a
/// print/SDK turn. Gates the `setsid` + registration in `run`; interactive mode
/// never sets it, so its foreground spawn path is byte-identical to before.
static PRINT_MODE: AtomicBool = AtomicBool::new(false);

/// Process-group ids (== pid of each `setsid` leader) of foreground Bash children
/// currently running under `run`.
fn registry() -> &'static Mutex<HashSet<u32>> {
    static REG: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Enable print/SDK-mode foreground-child tracking + tree-kill-on-signal. Called
/// once by the CLI before running a print/SDK turn. Idempotent.
pub fn enable_print_mode_child_cleanup() {
    PRINT_MODE.store(true, Ordering::SeqCst);
}

/// Whether print/SDK-mode child tracking is active. `run` gates its `setsid` +
/// registration on this — off (the default) means the prior behavior exactly.
#[must_use]
pub fn print_mode_child_cleanup_enabled() -> bool {
    PRINT_MODE.load(Ordering::SeqCst)
}

/// Record a running foreground child's pgid (its pid, since it is a `setsid`
/// group leader) so a print/SDK signal handler can tree-kill it before exit.
pub fn register(pid: u32) {
    if let Ok(mut g) = registry().lock() {
        g.insert(pid);
    }
}

/// Drop a foreground child's pgid once its `run` has completed (or its future was
/// dropped on cancel). Called from the RAII guard in `run`.
pub fn unregister(pid: u32) {
    if let Ok(mut g) = registry().lock() {
        g.remove(&pid);
    }
}

/// SIGKILL every registered foreground child's process group. Best-effort: each
/// registered pid is a `setsid` group leader (pid == pgid), so `killpg` reaches
/// the whole descendant subtree — mirroring claude-code's `process.kill(-pid)`
/// signal-exit cleanup. Called from the CLI's print/SDK signal handler right
/// before the process exits; an already-gone group is not an error.
pub fn kill_all_active_children() {
    let pids: Vec<u32> = registry()
        .lock()
        .map(|g| g.iter().copied().collect())
        .unwrap_or_default();
    for pid in pids {
        let _ = kill_tree_force(pid);
    }
}

#[cfg(test)]
fn is_registered(pid: u32) -> bool {
    registry().lock().map(|g| g.contains(&pid)).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::spawn_unsafe::attach_setsid;
    use nix::sys::signal::killpg;
    use nix::unistd::Pid;
    use std::time::Duration;
    use tokio::process::Command;

    /// A single sequential test (no parallel access to the shared registry): the
    /// empty-registry no-op, register/unregister bookkeeping, and — the core
    /// regression — that [`kill_all_active_children`] tears down a registered
    /// child's WHOLE process group (the setsid leader plus a grandchild it
    /// backgrounded), which is exactly the orphaned-tree case claude-code 2.1.212
    /// fixed for print/SDK mode.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_and_process_tree_kill() {
        // Empty registry: kill_all is a harmless no-op.
        kill_all_active_children();

        // A setsid child that backgrounds a long grandchild then sleeps itself, so
        // the process GROUP has two members. `kill_on_drop`-style single-pid kills
        // would leave the grandchild; a group kill (killpg) reaps both.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 120 & sleep 120");
        attach_setsid(&mut cmd);
        let mut child = cmd.spawn().expect("spawn setsid child");
        let pid = child.id().expect("child pid");
        let pgid = Pid::from_raw(i32::try_from(pid).expect("pid fits i32"));

        // Registration bookkeeping.
        register(pid);
        assert!(is_registered(pid), "pid should be registered");

        // The group is alive: killpg with no signal succeeds (existence probe).
        assert!(
            killpg(pgid, None).is_ok(),
            "child group should be alive before kill_all"
        );

        // The fix: kill_all_active_children SIGKILLs the whole group.
        kill_all_active_children();

        // Poll until the group is gone (SIGKILL is async; reaping the direct
        // child below lets the kernel free the pgid).
        let _ = child.wait().await;
        let mut gone = false;
        for _ in 0..40 {
            if killpg(pgid, None).is_err() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            gone,
            "kill_all_active_children must tear down the whole group"
        );

        // unregister removes the entry so a later kill_all ignores it.
        unregister(pid);
        assert!(!is_registered(pid), "pid should be unregistered");
    }
}
