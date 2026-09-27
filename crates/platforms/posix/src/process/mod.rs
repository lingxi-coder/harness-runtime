//! Process spawn for desktop hosts.
//!
//! [`runner`] holds the v0.2.0 `tokio::process`-backed `ProcessRunner`
//! implementation. The Phase-C helper modules — [`spawn_unsafe`],
//! [`kill_tree`], and [`wrap`] — implement claude-code's `setsid`+`killpg`
//! tree-kill semantics and the env-vars / cwd-tracking wrappers that
//! `BashShell` relies on. `spawn_unsafe` is the only module in the crate
//! that allows `unsafe` (single `libc::setsid` call from `pre_exec`).

pub mod active_children;
mod agent_processes;
pub mod kill_tree;
pub mod runner;
pub mod spawn_unsafe;
pub mod supervisor;
mod watchdog;
pub mod wrap;

pub use active_children::{
    enable_print_mode_child_cleanup, kill_all_active_children, print_mode_child_cleanup_enabled,
};
pub use runner::PosixProcess;
pub use wrap::{
    task_output_path, wrap_command_for_cwd_tracking, DEFAULT_TIMEOUT, ENV_GIT_EDITOR,
    ENV_LINGXI_MARKER, ENV_LINGXI_SESSION_ID, ENV_SHELL,
};
