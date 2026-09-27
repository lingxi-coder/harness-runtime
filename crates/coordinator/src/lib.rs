//! Coordinator-mode subsystem.
//!
//! See spec §6.16. The coordinator owns:
//! - the [`CoordinatorMode`] toggle that gates internal tools,
//! - a [`TeamRegistry`] tracking spawned worker agents,
//! - a [`MailboxRouter`] delivering messages between coordinator and workers,
//! - the per-worker [`TeammateMailbox`] inboxes,
//! - swarm-backend trait re-exports for tmux/screen integrations.

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 6 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]

pub mod handle;
pub mod implicit_team;
pub mod internal_tools;
pub mod mailbox;
pub mod mode;
pub mod prompt;
pub mod status_sink;
pub mod swarm;
pub mod team_file;
pub mod team_registry;
pub mod teammate_pump;
pub mod tool_send_message;
pub mod tool_synthetic_output;

pub use handle::worker_status_label;
pub use implicit_team::ImplicitTeammateSpawner;
pub use mailbox::{MailboxError, MailboxRouter, MessageSender, TeammateMailbox, TeammateMessage};
pub use mode::CoordinatorMode;
pub use prompt::{coordinator_system_prompt, coordinator_user_context, is_env_truthy};
pub use status_sink::CoordinatorStatusSink;
pub use team_registry::{TeamRegistry, WorkerAgent, WorkerStatus};
pub use teammate_pump::run_teammate_pump;
pub use tool_send_message::{SendMessageTool, SEND_MESSAGE_TOOL_NAME};
pub use tool_synthetic_output::{SyntheticOutputTool, SYNTHETIC_OUTPUT_TOOL_NAME};
