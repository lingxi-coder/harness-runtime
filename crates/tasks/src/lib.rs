//! Task scheduling and tracking primitives.
//!
//! See spec §6.6 (Tasks subsystem). This crate provides:
//! - The polymorphic [`state::TaskState`] union (10 variants).
//! - The generic [`task_trait::Task`] handler interface.
//! - [`registry::TaskRegistry`] for tracking running tasks.
//! - [`output_manager::TaskOutputManager`] for sandboxed task spool files.
//! - Per-type task handlers under [`handlers`].

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 56 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 2 item(s) rustc could
// reach from nothing when the workspace was measured (2026-09-16). The lint
// stays `warn` at the workspace level so a NEW crate still inherits it; this
// allow is scoped here so the count is per crate and repayable by deleting this
// line. This is the category where "named, computed, never wired" hides — some
// of these read like features that were built and never connected. Each wants a
// decision (delete, or wire), not a blanket deletion.
// ⚠️ The count above is ONE macOS, lib-target measurement. It is not a list of
// deletable items — see docs/HANDOFF-dead-code-adjudication-2026-09-17.md,
// which records two near-misses where it said "dead" about live code.
#![allow(dead_code)]

pub mod cron;
pub mod handle;
pub mod handlers;
pub mod id;
pub mod output_manager;
pub mod registry;
pub mod registry_status_sink;
mod resolve;
pub mod state;
pub mod task_trait;

pub use handlers::{
    escape_xml, fusion_result_xml, DreamHandler, InProcessTeammateHandler, LocalAgentHandler,
    LocalBashHandler, LocalFusionHandler, LocalWorkflowHandler, MonitorHandler, MonitorMcpHandler,
};
pub use id::{generate_task_id, TaskType};
pub use registry::{
    register_agent_handlers, register_dream_handler, register_fusion_handler,
    register_self_contained_handlers,
};
pub use state::*;
pub use task_trait::*;

mod lifecycle_store;

#[cfg(test)]
mod handback_test;
