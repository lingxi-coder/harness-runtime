//! Process-group helpers used by PTY lifecycle management.

use std::io;

#[cfg(unix)]
fn signal_process_group_id(process_group_id: u32, signal: libc::c_int) -> io::Result<bool> {
    let process_group_id = libc::pid_t::try_from(process_group_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process group ID overflow"))?;
    let result = unsafe { libc::killpg(process_group_id, signal) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        return Err(error);
    }
    Ok(true)
}

/// Send `SIGINT` to a Unix process group.
#[cfg(unix)]
pub fn interrupt_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id, libc::SIGINT).map(|_| ())
}

/// Report unsupported group interrupts on non-Unix platforms.
#[cfg(not(unix))]
pub fn interrupt_process_group(_process_group_id: u32) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "numeric process groups are not supported on this platform",
    ))
}

/// Send `SIGTERM` to a Unix process group.
#[cfg(unix)]
pub fn terminate_process_group(process_group_id: u32) -> io::Result<bool> {
    signal_process_group_id(process_group_id, libc::SIGTERM)
}

/// Report no numeric process group on non-Unix platforms.
#[cfg(not(unix))]
pub fn terminate_process_group(_process_group_id: u32) -> io::Result<bool> {
    Ok(false)
}

/// Send `SIGKILL` to a Unix process group.
#[cfg(unix)]
pub fn kill_process_group(process_group_id: u32) -> io::Result<()> {
    signal_process_group_id(process_group_id, libc::SIGKILL).map(|_| ())
}

/// Report no numeric process group on non-Unix platforms.
#[cfg(not(unix))]
pub fn kill_process_group(_process_group_id: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
/// Arrange for a child to receive `SIGTERM` if its original parent dies.
///
/// This helper is intended for use from a `pre_exec` closure. The caller must
/// capture the expected parent PID before spawning.
pub fn set_parent_death_signal(expected_parent_pid: libc::pid_t) -> io::Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) } == -1 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::getppid() } != expected_parent_pid {
        unsafe {
            libc::raise(libc::SIGTERM);
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
/// No-op parent-death setup outside Linux.
pub fn set_parent_death_signal(_expected_parent_pid: i32) -> io::Result<()> {
    Ok(())
}

/// Return the POSIX session identifier for a live process.
///
/// Unlike parent/group identifiers, this remains stable for interactive jobs
/// after the login shell exits and the jobs are reparented. A vanished process
/// returns `None`; inaccessible processes return the platform error.
#[cfg(unix)]
pub fn process_session_id(process_id: u32) -> io::Result<Option<u32>> {
    let pid = libc::pid_t::try_from(process_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "process ID overflow"))?;
    let session = unsafe { libc::getsid(pid) };
    if session == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(None);
        }
        return Err(error);
    }
    Ok(u32::try_from(session).ok())
}

#[cfg(all(test, unix))]
mod session_tests {
    use super::process_session_id;

    #[test]
    fn session_lookup_handles_live_and_reaped_processes() {
        assert!(process_session_id(std::process::id()).unwrap().is_some());
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert_eq!(process_session_id(pid).unwrap(), None);
        assert!(process_session_id(u32::MAX).is_err());
    }
}
