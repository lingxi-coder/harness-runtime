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

/// An OS advisory lock remains attached to the open file description. Never
/// unlink its pathname: doing so would permit another process to lock a new inode.
pub struct DesktopLease {
    file: std::fs::File,
    path: PathBuf,
}
static LOCAL_LEASES: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashSet<PathBuf>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
impl DesktopLease {
    pub fn acquire(home: &Path) -> std::io::Result<Self> {
        use fs2::FileExt;
        std::fs::create_dir_all(home)?;
        let path = std::fs::canonicalize(home)?.join("computer-use.atomic.lock");
        let mut local = LOCAL_LEASES
            .lock()
            .map_err(|_| std::io::Error::other("desktop lock poisoned"))?;
        if local.contains(&path) {
            return Err(std::io::Error::from(std::io::ErrorKind::WouldBlock));
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        file.try_lock_exclusive()?;
        local.insert(path.clone());
        Ok(Self { file, path })
    }

    /// Read only while holding this desktop's exclusive lease. The old empty
    /// lock file represents generation zero; the inode is never replaced.
    pub fn generation(&self) -> std::io::Result<u64> {
        use std::io::{Read, Seek, SeekFrom};
        if self.file.metadata()?.len() == 0 {
            return Ok(0);
        }
        if self.file.metadata()?.len() != 8 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid desktop generation",
            ));
        }
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = [0; 8];
        file.read_exact(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Invalidate observations before input is posted, including partial failures.
    /// Kernel-visible writes suffice: observations never survive an OS restart.
    pub fn advance_generation(&self) -> std::io::Result<u64> {
        use std::io::{Seek, SeekFrom, Write};
        let next = self
            .generation()?
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("desktop generation overflow"))?;
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&next.to_le_bytes())?;
        Ok(next)
    }
}
impl Drop for DesktopLease {
    fn drop(&mut self) {
        use fs2::FileExt;
        let _ = FileExt::unlock(&self.file);
        if let Ok(mut local) = LOCAL_LEASES.lock() {
            local.remove(&self.path);
        }
    }
}

#[cfg(test)]
mod atomic_tests {
    use super::*;
    #[test]
    fn same_process_leases_are_exclusive_until_drop() {
        let home =
            std::env::temp_dir().join(format!("computer-atomic-local-{}", std::process::id()));
        let lease = DesktopLease::acquire(&home).unwrap();
        assert_eq!(
            DesktopLease::acquire(&home).err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(lease);
        assert!(DesktopLease::acquire(&home).is_ok());
    }
    #[test]
    fn child_process_holder() {
        let Some(path) = std::env::var_os("COMPUTER_ATOMIC_TEST_HOME") else {
            return;
        };
        let home = PathBuf::from(path);
        let lease = DesktopLease::acquire(&home).unwrap();
        lease.advance_generation().unwrap();
        std::fs::write(home.join("ready"), "ready").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(10));
    }
    #[test]
    fn another_process_is_excluded_and_crash_releases_lock() {
        let home =
            std::env::temp_dir().join(format!("computer-atomic-process-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let _ = std::fs::remove_file(home.join("ready"));
        let lease = DesktopLease::acquire(&home).unwrap();
        let generation = lease.generation().unwrap();
        drop(lease);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "lock::atomic_tests::child_process_holder",
                "--nocapture",
            ])
            .env("COMPUTER_ATOMIC_TEST_HOME", &home)
            .spawn()
            .unwrap();
        for _ in 0..200 {
            if home.join("ready").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(home.join("ready").exists(), "child did not claim lock");
        assert_eq!(
            DesktopLease::acquire(&home).err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let _ = child.kill();
        let _ = child.wait();
        let lease = DesktopLease::acquire(&home).unwrap();
        assert_eq!(lease.generation().unwrap(), generation + 1);
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    #[test]
    fn a_new_owner_reads_the_previous_owners_generation() {
        let scope =
            std::env::temp_dir().join(format!("computer-generation-lock-{}", std::process::id()));
        let first = DesktopLease::acquire(&scope).unwrap();
        let before = first.generation().unwrap();
        assert_eq!(first.advance_generation().unwrap(), before + 1);
        drop(first);
        let peer = DesktopLease::acquire(&scope).unwrap();
        assert_eq!(peer.generation().unwrap(), before + 1);
        assert_eq!(peer.advance_generation().unwrap(), before + 2);
    }
}
