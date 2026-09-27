//! Cross-session `computer` tool lock (parity with the binary's
//! `cu_lock_held` gate + `wrapper.tsx`'s `formatLockHeld`): only one Lingxi
//! process may drive the computer at a time. A single well-known lock file
//! (NOT per-session — the whole point is exclusion ACROSS sessions) holds
//! the PID of whichever process currently owns it.
//!
//! A lock is "held" only while that PID is still alive, so a crashed or
//! exited session's lock is automatically treated as free by the very next
//! process that checks it — no explicit release/turn-end hook is needed for
//! correctness (release is still attempted best-effort when a session ends
//! cleanly, purely so a fresh session doesn't pay the stale-PID detection
//! cost). Mirrors `apps/cli`'s `daemon_lock` module's PID-liveness pattern,
//! simplified: no daemon-cmdline/start-time recycled-PID guard, since a
//! computer-use holder is just an ordinary interactive session, not a
//! specifically-spawned daemon subcommand — `tools/*` also cannot depend on
//! `apps/cli` (crate-layering: tools never depend on apps), so this is a
//! self-contained rebuild of just the primitive this crate needs.

use std::path::{Path, PathBuf};

/// Lock file name under the Lingxi config-home directory.
pub const LOCK_FILE: &str = "computer-use.lock";

/// Resolve the Lingxi config-home directory (port of claude-code's
/// `getClaudeConfigHomeDir` — `$LINGXI_CONFIG_DIR` honored verbatim when
/// set, else `$HOME/.lingxi`), independent of the sandboxed
/// `BuiltinToolContext::fs` (which is scoped to the workspace, not a
/// cross-session system location). Mirrors `tool-task`'s
/// `lingxi_config_home_dir`.
#[must_use]
pub fn lingxi_config_home_dir() -> PathBuf {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    branding::config_home(
        &home.map_or_else(PathBuf::new, PathBuf::from),
        std::env::var_os(branding::CONFIG_DIR_ENV),
    )
}

/// `{lingxi_home}/computer-use.lock`.
#[must_use]
pub fn lock_path(lingxi_home: &Path) -> PathBuf {
    lingxi_home.join(LOCK_FILE)
}

/// Whether `pid` is a live, signal-reachable process (POSIX `kill(pid, 0)` —
/// sends no actual signal, just probes existence; `EPERM` still means alive,
/// just owned by another user).
#[cfg(unix)]
#[must_use]
fn pid_is_alive(pid: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    matches!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None),
        Ok(()) | Err(nix::errno::Errno::EPERM)
    )
}

#[cfg(not(unix))]
#[must_use]
fn pid_is_alive(pid: i32) -> bool {
    pid > 1
}

/// Who currently holds the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Holder {
    /// No valid or live lock — free to claim.
    Free,
    /// This process already holds it (re-entrant — every action re-checks).
    Ourselves,
    /// A different, still-live process holds it.
    Other {
        /// The other process's id.
        pid: i32,
    },
}

/// Read the lock file (if any) and classify its holder against `my_pid`. A
/// missing, unparseable, or dead-PID lock is [`Holder::Free`] — the caller
/// may claim it.
#[must_use]
pub fn check(lingxi_home: &Path, my_pid: i32) -> Holder {
    let Ok(raw) = std::fs::read_to_string(lock_path(lingxi_home)) else {
        return Holder::Free;
    };
    let Ok(pid) = raw.trim().parse::<i32>() else {
        return Holder::Free;
    };
    if pid == my_pid {
        return Holder::Ourselves;
    }
    if pid_is_alive(pid) {
        Holder::Other { pid }
    } else {
        Holder::Free
    }
}

/// Claim the lock for `my_pid` — call only once [`check`] has confirmed
/// [`Holder::Free`] or [`Holder::Ourselves`]. Best-effort: a write failure
/// (e.g. a read-only config-home) is swallowed rather than blocking the
/// underlying action — bookkeeping must never be the reason a computer-use
/// call fails when no OTHER session is actually contending.
pub fn claim(lingxi_home: &Path, my_pid: i32) {
    let _ = std::fs::create_dir_all(lingxi_home);
    let _ = std::fs::write(lock_path(lingxi_home), my_pid.to_string());
}

/// Release the lock — only if THIS process currently holds it (never clobber
/// a peer that claimed it after our lock went stale). Best-effort.
pub fn release(lingxi_home: &Path, my_pid: i32) {
    if matches!(check(lingxi_home, my_pid), Holder::Ourselves) {
        let _ = std::fs::remove_file(lock_path(lingxi_home));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "lingxi-computeruse-lock-test-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn absent_lock_is_free() {
        let dir = tmpdir();
        assert_eq!(check(&dir, 1234), Holder::Free);
    }

    #[test]
    fn claim_then_check_from_the_same_pid_is_ourselves() {
        let dir = tmpdir();
        let me = std::process::id() as i32;
        claim(&dir, me);
        assert_eq!(check(&dir, me), Holder::Ourselves);
    }

    /// Spawn a real, cheap, briefly-lived child process to stand in for "a
    /// live pid that isn't us" — `pid_is_alive` special-cases `pid <= 1`
    /// (never a real computer-use holder), so a hardcoded low pid like `1`
    /// can't exercise the live-and-other branch; a real child sidesteps that
    /// entirely. The caller must keep the returned `Child` alive for as long
    /// as the pid needs to stay live (dropping it does NOT kill it, since
    /// `std::process::Child`'s `Drop` only closes handles, not the process).
    fn spawn_other_process() -> std::process::Child {
        std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" })
            .args(if cfg!(windows) {
                vec!["/C", "ping -n 5 127.0.0.1 >NUL"]
            } else {
                vec!["5"]
            })
            .spawn()
            .expect("spawn a short-lived child process")
    }

    #[test]
    fn a_live_other_pid_blocks() {
        let dir = tmpdir();
        let mut other = spawn_other_process();
        claim(&dir, other.id() as i32);
        assert_eq!(
            check(&dir, std::process::id() as i32),
            Holder::Other {
                pid: other.id() as i32
            }
        );
        let _ = other.kill();
        let _ = other.wait();
    }

    #[test]
    fn a_dead_pid_is_treated_as_free() {
        let dir = tmpdir();
        let mut other = spawn_other_process();
        let pid = other.id() as i32;
        let _ = other.kill();
        let _ = other.wait(); // reap it — now genuinely dead, not a zombie
        claim(&dir, pid);
        assert_eq!(check(&dir, std::process::id() as i32), Holder::Free);
    }

    #[test]
    fn garbage_lock_contents_are_treated_as_free() {
        let dir = tmpdir();
        std::fs::write(lock_path(&dir), b"not-a-pid").unwrap();
        assert_eq!(check(&dir, std::process::id() as i32), Holder::Free);
    }

    #[test]
    fn release_only_removes_our_own_lock() {
        let dir = tmpdir();
        let me = std::process::id() as i32;
        let mut other = spawn_other_process();
        let other_pid = other.id() as i32;
        claim(&dir, other_pid); // someone else's (a live pid, never us)
        release(&dir, me); // not ours — must not touch it
        assert_eq!(check(&dir, me), Holder::Other { pid: other_pid });
        let _ = other.kill();
        let _ = other.wait();

        claim(&dir, me);
        release(&dir, me);
        assert_eq!(check(&dir, me), Holder::Free);
    }

    #[test]
    fn release_is_idempotent_on_an_absent_lock() {
        let dir = tmpdir();
        release(&dir, std::process::id() as i32);
        assert_eq!(check(&dir, std::process::id() as i32), Holder::Free);
    }
}
