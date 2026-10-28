//! Tree-kill via `killpg(2)` on Unix.
//!
//! Children spawned with `setsid()` (see [`super::spawn_unsafe::attach_setsid`])
//! are process-group leaders — their PID equals their PGID. We pass the
//! POSITIVE PGID to `killpg(2)` to request a signal for group members,
//! including descendants that remain in the group. Delivery is subject to
//! OS permissions; descendants can also leave the original group.
//!
//! Two exported variants:
//! - [`kill_tree_force`]: immediate SIGKILL, no grace period. Used by
//!   `ProcessRunner::kill()` — matches `treeKill(pid, 'SIGKILL')` in
//!   `src/utils/ShellCommand.ts:337-343` which sends SIGKILL directly.
//! - [`kill_tree_unix`]: SIGTERM → 5 s grace → SIGKILL. Kept for callers
//!   (e.g. shutdown paths) that want a polite drain before force-killing.

use mobile_linux_api::ProcessError;
use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use std::time::Duration;

/// Default grace period between SIGTERM and the SIGKILL escalation.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);

/// Request `SIGTERM` for the process group `pid`, wait [`DEFAULT_GRACE`],
/// and request `SIGKILL`. A successful return is not proof that all members
/// or all descendants exited: signal delivery can be partial, and descendants
/// may have moved to another group.
///
/// `pid` MUST be the PGID — typically the PID of a child spawned with
/// `setsid()`, in which case PID == PGID. Passing another process's PID
/// instead targets the process group with that numeric ID if one exists;
/// it does not selectively target that process.
///
/// # Errors
/// Returns [`ProcessError::Io`] for signal errors other than `ESRCH`, which
/// indicates that no matching process group was present at that moment.
pub async fn kill_tree_unix(pid: u32) -> Result<(), ProcessError> {
    kill_tree_with_grace(pid, DEFAULT_GRACE).await
}

/// Like [`kill_tree_unix`] but skips the graceful `SIGTERM` and sends
/// `SIGKILL` directly. Used in shutdown paths where the parent cannot
/// afford the 5 s grace period. A successful return confirms only that the
/// signal request was accepted or the group was absent; it does not verify
/// that every group member or descendant has exited.
///
/// # Errors
/// Returns [`ProcessError::Io`] when `killpg(SIGKILL)` fails with
/// anything other than [`group_already_gone`].
#[allow(clippy::similar_names)] // pid/pgid: standard POSIX names.
pub fn kill_tree_force(pid: u32) -> Result<(), ProcessError> {
    let pgid = pgid_from(pid)?;
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(e) if group_already_gone(e) => Ok(()),
        Err(e) => Err(ProcessError::Io(format!("killpg SIGKILL: {e}"))),
    }
}

/// Lower-level variant exposed for the integration test so it can use a
/// short grace and keep the test fast.
#[allow(clippy::similar_names)] // pid/pgid: standard POSIX names, deliberately close.
pub async fn kill_tree_with_grace(pid: u32, grace: Duration) -> Result<(), ProcessError> {
    let pgid = pgid_from(pid)?;

    // SIGTERM first. Already-gone group is a success.
    match killpg(pgid, Signal::SIGTERM) {
        Ok(()) => {}
        Err(e) if group_already_gone(e) => return Ok(()),
        Err(e) => return Err(ProcessError::Io(format!("killpg SIGTERM: {e}"))),
    }

    tokio::time::sleep(grace).await;

    // SIGKILL escalation. Only ESRCH means the group is no longer present.
    match killpg(pgid, Signal::SIGKILL) {
        Ok(()) => Ok(()),
        Err(e) if group_already_gone(e) => Ok(()),
        Err(e) => Err(ProcessError::Io(format!("killpg SIGKILL: {e}"))),
    }
}

/// `ESRCH` indicates that this process group is absent at signal time.
/// `EPERM` may instead mean that the group still exists but cannot be signaled,
/// so it is never evidence that the group has exited.
/// An absent process group is not proof that all descendants have exited.
fn group_already_gone(e: nix::errno::Errno) -> bool {
    matches!(e, nix::errno::Errno::ESRCH)
}

fn pgid_from(pid: u32) -> Result<Pid, ProcessError> {
    let raw = i32::try_from(pid)
        .map_err(|_| ProcessError::Io(format!("pid {pid} does not fit in i32")))?;
    Ok(Pid::from_raw(raw))
}

#[cfg(test)]
mod tests {
    use super::{group_already_gone, kill_tree_force, kill_tree_unix};
    use mobile_linux_api::ProcessError;

    #[test]
    fn only_esrch_means_no_matching_group() {
        assert!(group_already_gone(nix::errno::Errno::ESRCH));
        assert!(!group_already_gone(nix::errno::Errno::EPERM));
        assert!(!group_already_gone(nix::errno::Errno::EINVAL));
    }

    /// Killing a non-existent PGID returns `Ok(())` — `ESRCH` on the first
    /// signal is treated as success because the caller's only goal is "the
    /// group is gone".
    #[tokio::test]
    async fn kill_tree_unix_nonexistent_pgid_is_ok() {
        // PID 0x7fff_ffff is well outside any pgid the kernel would issue
        // and overwhelmingly likely to be ESRCH.
        let res = kill_tree_unix(0x7fff_ffff).await;
        assert!(res.is_ok(), "non-existent pgid should be ok, got {res:?}");
    }

    /// `kill_tree_force` likewise treats `ESRCH` as success.
    #[test]
    fn kill_tree_force_nonexistent_pgid_is_ok() {
        let res = kill_tree_force(0x7fff_ffff);
        assert!(
            res.is_ok(),
            "non-existent pgid (force) should be ok, got {res:?}"
        );
    }

    /// A `u32` value that overflows `i32` is reported as an IO error
    /// instead of silently truncating.
    #[tokio::test]
    async fn kill_tree_unix_rejects_overflow_pid() {
        let res = kill_tree_unix(u32::MAX).await;
        match res {
            Err(ProcessError::Io(msg)) => assert!(
                msg.contains("does not fit in i32"),
                "unexpected error message: {msg}"
            ),
            other => panic!("expected Io overflow error, got {other:?}"),
        }
    }
}
