//! Process spawn for Windows hosts.
//!
//! Mirrors the layout of [`crate::process`] on POSIX: a [`runner`] module
//! that implements the `tokio::process` foreground/background spawn and a
//! [`kill_tree`] module that shells out to `taskkill /T /F /PID` for
//! whole-tree termination. Exit code `128` from taskkill ("process not
//! found") is treated as success, matching the POSIX `ESRCH` handling.

mod background;
pub mod kill_tree;
pub mod runner;

pub use runner::WindowsProcess;

#[cfg(windows)]
pub mod supervisor;
#[cfg(windows)]
mod supervisor_native;

#[cfg(windows)]
mod supervisor_gate;

#[cfg(windows)]
pub mod owned_job;

/// Test-only access to one retained Windows process identity.
#[cfg(all(windows, feature = "test-support"))]
pub mod test_support {
    pub struct HeldProcess(std::os::windows::io::OwnedHandle);

    impl HeldProcess {
        pub fn open(pid: u32) -> std::io::Result<Self> {
            super::supervisor_native::hold_process_handle_for_test(pid).map(Self)
        }

        pub fn is_alive(&self) -> Option<bool> {
            super::supervisor_native::process_handle_is_alive_for_test(&self.0)
        }
    }
}
