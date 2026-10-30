//! Desktop process execution and shell lifetime support.
//!
//! [`runner`] implements `ProcessRunner`; [`kill_tree`] signals owned process
//! groups, and [`wrap`] supplies shell environment/cwd tracking. Private Team
//! shells use [`team_guardian`] so cleanup survives model worker death.
//! [`spawn_unsafe`] contains the audited spawning and process-observation FFI.

pub mod active_children;
mod agent_processes;
pub mod kill_tree;
pub mod runner;
pub mod spawn_unsafe;
pub mod supervisor;
pub mod team_guardian;
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
