//! `tengu_session_*` event schemas — 19 events emitted by the session layer
//! (M5 owner; M3-06 schema lock + M5-07 jsonl-persistence triplet + M5-08
//! resume pair).
//!
//! Spec §7 line 763-767. Covers session lifecycle (start/resume/complete/abort),
//! persistence (load/save), clear, and export/import round-trips. User-derived
//! string fields are typed [`Verified`](crate::Verified); no PII columns in this
//! category (all identifiers + counts).

use crate::pii::Verified;
use serde::{Deserialize, Serialize};

// -- Event name constants (byte-locked) ---------------------------------------

/// `tengu_session_started` — new session begun (or first turn of a resumed one).
pub const STARTED: &str = "tengu_session_started";
/// `tengu_session_resumed` — `/resume` reloaded a prior session into the loop.
pub const RESUMED: &str = "tengu_session_resumed";
/// `tengu_session_completed` — session exited normally (`/exit` or stop).
pub const COMPLETED: &str = "tengu_session_completed";
/// `tengu_session_aborted` — session ended abnormally (crash, killswitch, signal).
pub const ABORTED: &str = "tengu_session_aborted";
/// `tengu_session_persisted` — session state was flushed to disk.
pub const PERSISTED: &str = "tengu_session_persisted";
/// `tengu_session_load_failed` — restoring a session from disk errored.
pub const LOAD_FAILED: &str = "tengu_session_load_failed";
/// `tengu_session_id_generated` — a fresh session id was minted.
pub const ID_GENERATED: &str = "tengu_session_id_generated";
/// `tengu_session_clear_requested` — `/clear` was invoked by the user.
pub const CLEAR_REQUESTED: &str = "tengu_session_clear_requested";
/// `tengu_session_clear_completed` — `/clear` finished wiping in-memory history.
pub const CLEAR_COMPLETED: &str = "tengu_session_clear_completed";
/// `tengu_session_export_started` — `/export` began.
pub const EXPORT_STARTED: &str = "tengu_session_export_started";
/// `tengu_session_export_completed` — `/export` produced the output artefact.
pub const EXPORT_COMPLETED: &str = "tengu_session_export_completed";
/// `tengu_session_export_failed` — `/export` errored mid-flight.
pub const EXPORT_FAILED: &str = "tengu_session_export_failed";
/// `tengu_session_import_started` — `/import` began.
pub const IMPORT_STARTED: &str = "tengu_session_import_started";
/// `tengu_session_import_completed` — `/import` rehydrated the session.
pub const IMPORT_COMPLETED: &str = "tengu_session_import_completed";
/// `tengu_session_import_failed` — `/import` errored mid-flight.
pub const IMPORT_FAILED: &str = "tengu_session_import_failed";
/// `tengu_session_appended` — one message was appended to the on-disk JSONL.
pub const APPENDED: &str = "tengu_session_appended";
/// `tengu_session_rotated` — the JSONL file rolled over (size cap, manual rotate).
pub const ROTATED: &str = "tengu_session_rotated";
/// `tengu_session_corrupted` — writer or reader detected an unrecoverable I/O / parse error.
pub const CORRUPTED: &str = "tengu_session_corrupted";
/// `tengu_session_persistence_failed` — transcript persistence failed during a
/// best-effort write, but the live session continued.
pub const PERSISTENCE_FAILED: &str = "tengu_session_persistence_failed";
// NOTE: claude emits a single `tengu_session_resumed` ([`RESUMED`]) on resume —
// the started/completed pair (`tengu_session_resume_started/_completed`) has 0
// hits in the 2.1.195 binary, so it was removed for strict parity.

/// Order-locked array of all 19 names; consumed by `tengu::ALL_EVENT_NAMES`.
pub(crate) const NAMES: &[&str] = &[
    STARTED,
    RESUMED,
    COMPLETED,
    ABORTED,
    PERSISTED,
    LOAD_FAILED,
    ID_GENERATED,
    CLEAR_REQUESTED,
    CLEAR_COMPLETED,
    EXPORT_STARTED,
    EXPORT_COMPLETED,
    EXPORT_FAILED,
    IMPORT_STARTED,
    IMPORT_COMPLETED,
    IMPORT_FAILED,
    APPENDED,           // M5-07
    ROTATED,            // M5-07
    CORRUPTED,          // M5-07
    PERSISTENCE_FAILED, // 2.1.252 persistence-failure parity
];

// -- Payload structs (deny_unknown_fields locked) -----------------------------

/// Session export / import format selector.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ExportFormat {
    /// Single JSON document (full transcript).
    Json,
    /// Markdown-rendered transcript (human-readable).
    Markdown,
    /// Newline-delimited JSON (one message per line).
    Jsonl,
}

/// Payload for [`STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Set when the session was created by resuming a prior session.
    pub resumed_from: Option<Verified>,
}

/// Payload for [`RESUMED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResumedPayload {
    /// Stable identifier for the resumed session.
    pub session_id: Verified,
    /// Session id this resumed from.
    pub from_session_id: Verified,
}

/// Payload for [`COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Wall-clock duration of the session in seconds.
    pub duration_secs: u64,
    /// Total agent turns across the session.
    pub total_turns: u32,
}

/// Payload for [`ABORTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AbortedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Whitelisted abort reason (no PII).
    pub reason: Verified,
}

/// Payload for [`PERSISTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Bytes written to the on-disk session record.
    pub bytes_written: u64,
}

/// Payload for [`LOAD_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadFailedPayload {
    /// Stable identifier for the session that failed to load.
    pub session_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

/// Payload for [`ID_GENERATED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdGeneratedPayload {
    /// Newly minted session id.
    pub session_id: Verified,
}

/// Payload for [`CLEAR_REQUESTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClearRequestedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
}

/// Payload for [`CLEAR_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClearCompletedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Count of messages wiped from in-memory history.
    pub messages_cleared: u32,
}

/// Payload for [`EXPORT_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportStartedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Output format requested.
    pub format: ExportFormat,
}

/// Payload for [`EXPORT_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportCompletedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Bytes written to the output artefact.
    pub bytes_written: u64,
}

/// Payload for [`EXPORT_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportFailedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

/// Payload for [`IMPORT_STARTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportStartedPayload {
    /// Stable identifier for this session (the target).
    pub session_id: Verified,
    /// Input format of the artefact being imported.
    pub format: ExportFormat,
}

/// Payload for [`IMPORT_COMPLETED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportCompletedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Number of messages rehydrated into the session.
    pub messages_imported: u32,
}

/// Payload for [`IMPORT_FAILED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportFailedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

/// Payload for [`APPENDED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppendedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// UUID of the message just appended.
    pub message_uuid: Verified,
}

/// Payload for [`ROTATED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotatedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// File size at the moment of rotation, in bytes.
    pub bytes_before_rotation: u64,
}

/// Payload for [`CORRUPTED`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorruptedPayload {
    /// Stable identifier for this session.
    pub session_id: Verified,
    /// Whitelisted error description (no PII).
    pub error: Verified,
}

/// Payload for [`PERSISTENCE_FAILED`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistenceFailedPayload {}
