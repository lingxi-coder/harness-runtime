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
