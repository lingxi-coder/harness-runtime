//! Rollout RECORD capability — a faithful port of codex's `rollout` crate
//! recorder semantics, reconciled into LingXi's `session` crate.
//!
//! # Why a new module rather than a duplicate crate
//!
//! Codex persists every session as a date-partitioned JSONL "rollout"
//! (`sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`). Each line is a
//! [`RolloutLine`] = a `timestamp` plus a flattened [`RolloutItem`] tagged
//! `{ "type": …, "payload": … }`. A background writer task owns the file
//! handle, buffers items, and defers file creation until the first
//! `persist()`/`flush()` so empty sessions never touch disk. This is the
//! "record" half of codex's rollout subsystem.
//!
//! LingXi's pre-existing `session` modules (`jsonl`, `storage`, `metadata`,
//! `resumer`, `transcript`) speak a *different* on-disk format — the
//! byte-locked claude-code transcript schema (`type`/`uuid`/`parentUuid`/…).
//! Those are left untouched. This module adds codex's rollout record format
//! and recorder mechanics alongside them, so the `session` crate now hosts
//! *both* persistence styles without a shadow `rollout` crate.
//!
//! # Reconciliation with codex types
//!
//! Codex's [`RolloutItem`] embeds `codex_protocol::ResponseItem` /
//! `EventMsg` / `InterAgentCommunication` — entire protocol subsystems that
//! do not exist in LingXi. To stay faithful to the *on-disk bytes* (which is
//! what resume/inspection actually depends on) without dragging in those
//! enums, the model-coupled arms carry an opaque [`serde_json::Value`]
//! payload. The `{ "type": …, "payload": … }` envelope, the session-meta
//! line, the legacy ghost-snapshot stripping, and the persist/flush/recovery
//! mechanics are reproduced exactly.

pub mod compression;
pub mod initial_history;
pub mod metadata;
pub mod policy;
pub mod record;
pub mod recorder;
pub mod session_index;

pub use compression::{
    existing_rollout_path, open_rollout_line_reader, spawn_rollout_compression_worker,
    RolloutLineReader, COMPRESSION_LEVEL,
};
pub use initial_history::{InitialHistory, ResumedHistory};
pub use metadata::{
    builder_from_items, parse_timestamp_uuid_from_filename, plain_rollout_path, rollout_date_parts,
    ThreadMetadataBuilder,
};
pub use policy::{is_persisted_rollout_item, persisted_rollout_items};
pub use record::{GitInfo, RolloutItem, RolloutLine, SessionMeta, SessionMetaLine, SessionSource};
pub use recorder::{append_rollout_item_to_path, RolloutRecorder, RolloutRecorderParams};
pub use session_index::{
    append_thread_name, find_thread_name_by_id, find_thread_names_by_ids,
    remove_thread_name_entries, SessionIndexEntry,
};

/// Subdirectory under `codex_home` that holds active session rollouts.
pub const SESSIONS_SUBDIR: &str = "sessions";
/// Subdirectory under `codex_home` that holds archived session rollouts.
pub const ARCHIVED_SESSIONS_SUBDIR: &str = "archived_sessions";

#[cfg(test)]
mod recorder_tests;
#[cfg(test)]
mod tests;
