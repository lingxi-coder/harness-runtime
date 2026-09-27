//! Product-A V2 todo-task store — the file-backed 1:1 port of claude-code
//! `src/utils/tasks.ts` plus its embedded `proper-lockfile` dependency.
//!
//! Extracted from `tool-task` (which re-exports both modules for its existing
//! consumers) into a LEAF crate so the `tasks` crate's in-process teammate
//! runner and the `coordinator` crate's shutdown path can reach the store
//! without the `tool-task → cron → tasks` dependency cycle.
//!
//! Module identity is preserved 1:1 with the oracle:
//! - [`todo_store`] ↔ `src/utils/tasks.ts`
//! - [`proper_lockfile`] ↔ the embedded npm `proper-lockfile`

#![forbid(unsafe_code)]
#![allow(clippy::doc_markdown)]

pub mod proper_lockfile;
pub mod todo_store;

pub use todo_store::{
    format_unassignment_notification, ClaimOptions, ClaimResult, TeammateEndReason, TodoStore,
    TodoTask, UnassignError, UnassignOutcome, UnassignedTask,
};
