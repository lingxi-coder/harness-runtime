use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Configure a child command to inherit its working directory from an already
/// opened directory descriptor.
///
/// The descriptor is cloned into the pre-exec closure, so callers do not have
/// to keep the original [`File`] alive until `spawn`. This avoids resolving a
/// mutable pathname between an ownership check and a child-side git probe.
pub fn command_current_dir_from_open_directory(
    command: &mut Command,
    directory: &File,
) -> io::Result<()> {
    let directory = directory.try_clone()?;
    // SAFETY: `fchdir` is async-signal-safe and the closure performs no heap
    // allocation or locking after fork. The cloned descriptor stays alive in
    // the closure until the child has changed directory.
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(directory.as_raw_fd()) == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    Ok(())
}
