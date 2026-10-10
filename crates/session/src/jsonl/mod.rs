//! On-disk JSONL transcript format — byte-equivalent to claude-code's
//! `~/.lingxi/projects/<sanitized-cwd>[-<djb2>]/<session-uuid>.jsonl`.
//!
//! Submodules:
//! - `djb2` — modified-djb2 hash (1:1 port of `claude-code/src/utils/hash.ts`).
//! - `uuid` — UUID v4 validation regex from `sessionStoragePortable.ts:23-24`.
//! - `path` — project-dir resolver (sanitize-path + djb2-suffix fallback).
//! - `schema` — `JsonlMessage` struct with the locked outer field set.
//! - `writer` — append-only `JsonlWriter` (one line = one JSON object + `\n`).
//! - `reader` — full `read_all` + 64 KB-head `read_lite` byte-byte algorithms.
//! - `recover` — pre-existing crash-recovery reader (M1/M3 surface, unchanged).
//! - `loader` — M5-08 resume enumeration + chain validation + interactive picker.
//! - `title` — M5-08 first-user-message title extraction.
//! - `transcript_compact` — SC-08 transcript-file rewrite (`performCompactTranscript`).

/// Persisted capability profile for one immutable session transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionMode {
    /// Conversational profile: device, read, web and inline-visualization
    /// tools plus `Write` and `Shell`, each call confirmed through the
    /// permission prompt. Skills never get shell expansion here.
    Chat,
    /// Full development profile and legacy fallback.
    Code,
}

impl SessionMode {
    /// Stable JSONL wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Code => "code",
        }
    }

    /// Parse a stable JSONL wire value.
    #[must_use]
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "code" => Some(Self::Code),
            _ => None,
        }
    }
}

pub mod djb2;
pub mod durable_writer;
pub mod exact_json;
pub mod journal;
pub mod loader;
pub mod message_identity;
pub mod path;
pub mod re_append;
pub mod reader;
pub mod recover;
pub mod schema;
pub mod title;
pub mod transcript_compact;
pub mod uuid;
pub mod writer;

// Re-export pre-existing public surface so dependents keep their imports.
pub use recover::{read_recover, RecoveryResult, StorageError};

// New M5-07 public surface.
pub use durable_writer::{
    DurableTranscriptTransaction, DurableTranscriptWriter, TranscriptAppendOutcome,
    TranscriptWriterError, DEFAULT_MAX_TRANSCRIPT_SCAN_BYTES, TRANSCRIPT_LOCK_FILE_NAME,
};
pub use journal::{
    DurableJournal, JournalAppend, JournalEntry, JournalEnvelope, JournalError, JournalReplay,
    JournalSnapshot, QuarantineReport, JOURNAL_FILE_NAME, JOURNAL_LOCK_FILE_NAME,
    JOURNAL_SCHEMA_VERSION, SNAPSHOT_FILE_NAME,
};
pub use path::{project_dir_name, session_path, tool_results_dir};
// `reader::SessionMetadata` (lite head-only struct) is intentionally NOT
// re-exported as `jsonl::SessionMetadata`; M5-08 introduces a different
// `SessionMetadata` for the resume picker (uuid + title + mtime + line
// count). Callers that need the M5-07 type reach it via
// `session::JsonlSessionMetadata` (crate-root alias) or the
// fully-qualified `session::jsonl::reader::SessionMetadata`.
pub use reader::JsonlReader;
// Metadata re-append (`reAppendSessionMetadata`, 2.1.220 offset 237852347).
pub use re_append::{
    extract_quoted_field, find_last_typed_field, format_iso_millis, normalize_last_prompt,
    plan_re_append, read_tail, ReAppendPlan, SessionMetadataState,
    METADATA_REAPPEND_BACKSTOP_BYTES,
};
// Tolerant-reader surface (real-transcript gap fix): the two-phase routed
// loader output + its line-router + the transcript-message type predicate.
pub use reader::{is_transcript_message_type, parse_pr_number, route_lines, LoadedTranscript};
pub use schema::{session_kind, JsonlMessage, SESSION_KIND_ENV, SESSION_KIND_KEY};
// SC-08 — the reclamation half of the metadata backstop.
pub use transcript_compact::{
    build_compact_plan, compact_persistence, local_gc_enabled, next_backstop,
    perform_compact_transcript, strip_leading_nuls, CompactFailure, CompactOutcome,
    CompactPersistence, CompactPlan, CompactStats, PlanAbort, PlanOutcome, COMPACT_BACKSTOP_BYTES,
    MAX_COMPACT_BACKSTOP_BYTES, MIN_COMPACT_FILE_BYTES, MIN_RECLAIM_FRACTION,
    TRANSCRIPT_LOCAL_GC_ENV,
};
pub use uuid::validate_uuid;
pub use writer::JsonlWriter;

// New M5-08 public surface.
pub use loader::{
    build_conversation_chain, discovered_tool_names, find_tip, list_recent_sessions,
    list_recent_sessions_with_diagnostics, load_session, load_session_across_worktrees,
    load_session_entries, load_session_entries_across_worktrees, pre_compact_discovered_tools,
    read_agent_resume_state, read_agent_snapshot, resolve_session_path_across_worktrees,
    search_sessions_by_custom_title, select_session_interactive, LoaderError, SessionCatalog,
    SessionMetadata,
};
pub use message_identity::SessionMessageIdentitySnapshot;
pub use title::{derive_fork_name, extract_title, FORK_NAME_FALLBACK};

/// Size of the head buffer for lite metadata reads — 64 KiB.
/// Byte-locked to `claude-code/src/utils/sessionStoragePortable.ts:17`
/// (`LITE_READ_BUF_SIZE = 65536`).
pub const LITE_READ_BUF_SIZE: usize = 65_536;

mod recovery;
