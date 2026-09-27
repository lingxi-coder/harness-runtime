//! Cross-platform pseudo-terminal process lifecycle primitives.
//!
//! The design is adapted from `OpenAI` Codex's `codex-rs/utils/pty` at commit
//! `b8c2d29cc23b41fa7c7f5f5483e92fc71099635a` (Apache-2.0). See the crate
//! `NOTICE` file for attribution.

#![allow(unsafe_code)]

mod private_file;
mod process;
pub mod process_group;
mod pty;
#[cfg(unix)]
mod unix_command;
#[cfg(windows)]
mod win;
#[cfg(windows)]
mod windows_file;
#[cfg(windows)]
mod windows_input;
#[cfg(windows)]
mod windows_pipe;
#[cfg(windows)]
mod windows_terminal;

/// Maximum output retained by consumers that choose to buffer PTY data.
pub const DEFAULT_OUTPUT_BYTES_CAP: usize = 1024 * 1024;

pub use private_file::create_current_user_private_file;
pub use process::ProcessHandle;
pub use process::ProcessSignal;
pub use process::SpawnedProcess;
pub use process::TerminalSize;
pub use pty::conpty_supported;
pub use pty::spawn_process as spawn_pty_process;
#[cfg(unix)]
pub use unix_command::command_current_dir_from_open_directory;
#[cfg(windows)]
pub use windows_file::{
    delete_by_handle as delete_windows_path_by_handle, file_identity as windows_file_identity,
    open_reparse_guarded as open_windows_reparse_guarded, WindowsFileIdentity,
};
#[cfg(windows)]
pub use windows_pipe::create_current_user_named_pipe;
#[cfg(windows)]
pub use windows_terminal::WindowsStdinPump;

#[cfg(test)]
mod tests;
