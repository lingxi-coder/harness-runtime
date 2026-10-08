//! Session storage + crash-safe transcript reader + resumer.
//!
//! See spec §22 (`SessionMetadata`, append-only JSONL with fsync + flock,
//! crash-safe reader, `SessionResumer`).

#![forbid(unsafe_code)]
// Documentation debt, not a decision that docs do not matter: this crate had
// 87 undocumented public item(s) when `missing_docs` was measured across the
// workspace (2026-09-16). The lint stays `warn` at the workspace level so a NEW
// crate still inherits the requirement; this allow is scoped here so the debt
// is visible per crate and can be repaid one crate at a time by deleting this
// line.
#![allow(missing_docs)]
// Dead code kept visible, not swept: this crate had 1 item(s) rustc could
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

pub mod agent_color;
pub mod agent_rows;
pub mod branch;
pub mod file_history;
pub mod filestate;
pub mod forked_skill;
pub mod jsonl;
pub mod metadata;
pub mod prompt_history;
pub mod rate_limit_checkpoint;
pub mod resumer;
pub mod rewind;
pub mod rollout;
pub mod storage;
pub mod transcript;

// Pre-existing surface — preserved bit-for-bit so M1-M4 downstream compiles.
pub use jsonl::{read_recover, RecoveryResult, StorageError};

// `/color` persistence (claude-code `saveAgentColor`).
pub use agent_color::{agent_color_entry, last_agent_color, save_agent_color};
pub use branch::{create_branch, BranchError, BranchResult};
pub use file_history::{FileHistory, FileHistoryBackup, SnapshotRecord};
pub use metadata::SessionMetadata;
// Rate-limit resume checkpoint (SC-02): the `RESUME.md` document, the git
// executor (`q4v`) and the once-per-session latch (`z4v`). The `rate_limited`
// hard-429 trigger lives in `orchestrator::turn_loop`; the near-limit twin is
// dispatched by the subagent runner through `ProviderApiAdapter`.
pub use rate_limit_checkpoint::{
    checkpoint_ref, checkpoint_result_for_session, checkpoint_session_key,
    checkpoint_started_for_session, clear_checkpoint_result_for_session,
    clear_last_checkpoint_result, dispatch_rate_limit_checkpoint, last_checkpoint_result,
    local_checkpoint_commit_allowed, perform_rate_limit_checkpoint, render_resume_md,
    sanitize_todo_line, CheckpointGates, CheckpointRequest, CheckpointResult, CheckpointSkipReason,
    CheckpointTrigger, OwnedCheckpointRequest, ResumeDoc, ALLOW_LOCAL_CHECKPOINT_COMMIT_ENV,
    CHECKPOINT_REF_PREFIX, MAX_CHECKPOINT_FILE_COUNT, MAX_CHECKPOINT_TOTAL_BYTES,
    RESUME_MD_REPO_PATH,
};
pub use resumer::{ResumeError, ResumedRollout, ResumedSession, SessionResumer};
pub use rewind::rewind_conversation;
pub use storage::{LoadedSession, SessionStorage};
pub use transcript::TranscriptEntry;

// New M5-07 surface — distinct name (`JsonlSessionMetadata`) so it does NOT
// collide with the pre-existing `metadata::SessionMetadata` re-export.
pub use jsonl::reader::SessionMetadata as JsonlSessionMetadata;
pub use jsonl::{
    project_dir_name, session_path, validate_uuid, JsonlMessage, JsonlReader, JsonlWriter,
    SessionMessageIdentitySnapshot, LITE_READ_BUF_SIZE,
};
// SC-08 — transcript-file compaction (`performCompactTranscript`). The trigger
// lives on `JsonlWriter::maybe_compact_transcript`; these are the surface an
// embedder needs to inspect or drive one directly.
pub use jsonl::{
    perform_compact_transcript, CompactFailure, CompactOutcome, CompactStats,
    COMPACT_BACKSTOP_BYTES, MIN_COMPACT_FILE_BYTES, TRANSCRIPT_LOCAL_GC_ENV,
};

// Rollout RECORD capability (codex `rollout` crate recorder semantics merged
// into the `session` crate). Re-exported under the `rollout` module name to
// avoid colliding with the byte-locked claude-code transcript surface above.
pub use rollout::{
    append_rollout_item_to_path, append_thread_name, builder_from_items, find_thread_name_by_id,
    find_thread_names_by_ids, is_persisted_rollout_item, parse_timestamp_uuid_from_filename,
    persisted_rollout_items, plain_rollout_path, remove_thread_name_entries, rollout_date_parts,
    GitInfo, InitialHistory, ResumedHistory, RolloutItem, RolloutLine, RolloutRecorder,
    RolloutRecorderParams, SessionMeta as RolloutSessionMeta, SessionMetaLine, SessionSource,
    ThreadMetadataBuilder, ARCHIVED_SESSIONS_SUBDIR, SESSIONS_SUBDIR,
};
