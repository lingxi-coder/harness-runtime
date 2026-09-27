//! The single module in `lingxi-platform-posix` that uses `unsafe`.
//!
//! `unsafe` here calls `libc::setsid(3)` from `CommandExt::pre_exec`.
//! `pre_exec` runs in the forked child between `fork()` and `execve()` —
//! the closure MUST be async-signal-safe (POSIX.1-2017 §2.4.3). `setsid` is
//! in POSIX's async-signal-safe function list, so this is sound. No
//! allocation, no logging, no tokio — only the syscall.
//!
//! Why we need it: tree-kill in [`super::kill_tree`] sends `SIGTERM` /
//! `SIGKILL` to a process group via `killpg(2)`. For that to terminate the
//! entire descendant tree of a background process, the child must be a
//! process-group leader (pid == pgid). Calling `setsid()` immediately after
//! `fork()` makes the child the session and process-group leader.
//!
//! Matches claude-code's `detached: provider.detached` spawn option in
//! `Shell.ts:334` — Node's `detached: true` invokes `setsid()` under the
//! hood on POSIX.

#![allow(unsafe_code)]

use std::io;
use tokio::process::Command;

/// Install a `pre_exec` callback on `cmd` that calls `setsid()` in the
/// child. The closure is async-signal-safe and contains exactly one
/// syscall, so it is safe to compose with any other `pre_exec` callbacks
/// callers may add later (later `pre_exec`s run after this one).
///
/// Idempotent in behaviour: calling it twice installs two callbacks, but
/// the second `setsid()` will fail harmlessly with `EPERM` in the child
/// (already a session leader) and abort the spawn — so callers must call
/// it at most once per `Command`.
pub fn attach_setsid(cmd: &mut Command) {
    // SAFETY: setsid() is async-signal-safe per POSIX.1-2017 §2.4.3 Table
    // 2-5, and we make no other calls inside the closure. The closure runs
    // in the forked child between fork() and execve(); no Rust runtime
    // state is shared and no heap allocation happens. The only Rust API
    // touched is `io::Error::last_os_error()` which reads thread-local
    // errno via `__errno_location()` / `errno` macro — itself
    // async-signal-safe and used by the standard library's own `pre_exec`
    // examples.
    unsafe {
        cmd.pre_exec(|| -> io::Result<()> {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Effective owner used to authenticate the private supervisor directory.
pub(super) fn effective_uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}

/// Fork with the ordinary exec-error pipe intact, but do not exec the approved
/// program until the source has received its process capability. Cancellation
/// drops the parent socket; the child sees EOF and leaves without running it.
pub(super) async fn spawn_with_capability(
    mut command: Command,
    binding: Option<&platform_api::process::BackgroundTaskBinding>,
) -> Result<tokio::process::Child, platform_api::ProcessError> {
    use platform_api::ProcessError;
    use std::os::fd::AsRawFd;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sink = binding.and_then(|binding| binding.on_exit.as_ref());
    if !sink.is_some_and(|sink| sink.stop_notify().is_some()) {
        let mut child = command
            .spawn()
            .map_err(|e| ProcessError::Io(e.to_string()))?;
        if let (Some(binding), Some(sink), Some(pid)) = (binding, sink, child.id()) {
            if let Err(error) = sink.on_spawn(&binding.task_id, pid).await {
                let _ = super::kill_tree::kill_tree_force(pid);
                let _ = child.wait().await;
                return Err(error);
            }
        }
        return Ok(child);
    }
    let (parent, child_socket) =
        std::os::unix::net::UnixStream::pair().map_err(|e| ProcessError::Io(e.to_string()))?;
    parent
        .set_nonblocking(true)
        .map_err(|e| ProcessError::Io(e.to_string()))?;
    let parent_fd = parent.as_raw_fd();
    let child_fd = child_socket.as_raw_fd();
    // SAFETY: only close/getpid/write/read and errno access run after fork.
    // The socketpair was created with CLOEXEC. Close the inherited parent end
    // before waiting, otherwise supervisor death could not produce EOF.
    unsafe {
        command.pre_exec(move || {
            // Moves the guard into this scope so it drops at the END of it. The lint sees
            // a `_`-binding with no side effect; the side effect is the Drop deadline.
            #[allow(clippy::no_effect_underscore_binding)]
            let _keep_child_socket_alive = &child_socket;
            libc::close(parent_fd);
            let pid = libc::getpid().to_ne_bytes();
            let mut written = 0;
            while written < pid.len() {
                let count = libc::write(
                    child_fd,
                    pid[written..].as_ptr().cast(),
                    pid.len() - written,
                );
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(error);
                }
                written += count as usize;
            }
            let mut accepted = 0u8;
            loop {
                let count = libc::read(child_fd, (&mut accepted as *mut u8).cast(), 1);
                if count == 1 && accepted == 1 {
                    return Ok(());
                }
                if count < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(libc::EINTR) {
                        continue;
                    }
                    return Err(error);
                }
                return Err(io::Error::from_raw_os_error(libc::ECANCELED));
            }
        });
    }
    let mut parent =
        tokio::net::UnixStream::from_std(parent).map_err(|e| ProcessError::Io(e.to_string()))?;
    let mut spawn = tokio::task::spawn_blocking(move || command.spawn());
    let mut pid = [0u8; std::mem::size_of::<libc::pid_t>()];
    tokio::select! {
        result = &mut spawn => return result.map_err(|e|ProcessError::Io(e.to_string()))?
            .map_err(|e|ProcessError::Io(e.to_string())),
        result = parent.read_exact(&mut pid) => { result.map_err(|e|ProcessError::Io(e.to_string()))?; }
    }
    let pid = libc::pid_t::from_ne_bytes(pid) as u32;
    let binding = binding.expect("gated binding");
    if let Err(error) = sink
        .expect("gated sink")
        .on_spawn(&binding.task_id, pid)
        .await
    {
        drop(parent);
        let _ = spawn.await;
        return Err(error);
    }
    parent
        .write_all(&[1])
        .await
        .map_err(|e| ProcessError::Io(e.to_string()))?;
    spawn
        .await
        .map_err(|e| ProcessError::Io(e.to_string()))?
        .map_err(|e| ProcessError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::attach_setsid;
    use nix::unistd::{getpgid, getsid, Pid};
    use tokio::process::Command;

    /// `attach_setsid` makes the spawned child a session/process-group
    /// leader: `getsid(pid) == pid` AND `getpgid(pid) == pid`. We probe
    /// from the parent via `nix::unistd` so the assertion is portable
    /// across BSD `ps` (macOS) and Linux `ps` (which use different
    /// `-o` keywords for the session id).
    #[allow(clippy::similar_names)] // pid/sid/pgid: standard POSIX names.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn attach_setsid_makes_child_session_leader() {
        // A short-lived child is enough — `setsid` happens between fork
        // and execve. We use `sleep 1` so the child stays alive long
        // enough for us to call getsid/getpgid on it from the parent.
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 1");
        attach_setsid(&mut cmd);

        let mut child = cmd.spawn().expect("spawn");
        let raw = i32::try_from(child.id().expect("pid")).expect("pid fits i32");
        let pid = Pid::from_raw(raw);

        // `spawn` returns once the FORK succeeds; `setsid` runs in the child
        // between fork and execve, so the parent can observe the pre-setsid
        // session for a moment. Reading once made this test fail under a loaded
        // full-workspace run (the child had not been scheduled yet) while
        // passing every time in isolation. Poll until the child has become its
        // own session leader — the child lives ~1s, so this is bounded well
        // inside its lifetime and still fails fast if `attach_setsid` is a
        // no-op.
        let mut sid = getsid(Some(pid)).expect("getsid");
        let mut pgid = getpgid(Some(pid)).expect("getpgid");
        for _ in 0..200 {
            if sid == pid && pgid == pid {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            sid = getsid(Some(pid)).expect("getsid");
            pgid = getpgid(Some(pid)).expect("getpgid");
        }
        assert_eq!(sid, pid, "session leader: sid {sid} should equal pid {pid}");
        assert_eq!(
            pgid, pid,
            "process-group leader: pgid {pgid} should equal pid {pid}"
        );

        // Reap the child to avoid leaking a zombie into other tests.
        child.wait().await.expect("wait child");
    }

    /// Without `attach_setsid`, a child shell inherits the parent's
    /// session id, so `getsid(child) != child_pid`. Guards against the
    /// helper silently becoming a no-op.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_setsid_means_child_inherits_session() {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg("sleep 1");

        let mut child = cmd.spawn().expect("spawn");
        let raw = i32::try_from(child.id().expect("pid")).expect("pid fits i32");
        let pid = Pid::from_raw(raw);

        let sid = getsid(Some(pid)).expect("getsid");
        assert_ne!(
            sid, pid,
            "without attach_setsid the child should NOT be its own session leader"
        );

        child.wait().await.expect("wait child");
    }
}
