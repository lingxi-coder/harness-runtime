//! Append-only transcript entries (spec §22.3).
//!
//! `TranscriptEntry` is the union of every kind of record the engine writes
//! to disk. JSONL serialization uses an internal `type` tag to keep entries
//! self-describing.

use protocol::{ConversationMessage, HookId, MessageId, SessionId, ToolUseId};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// One entry in `transcript.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TranscriptEntry {
    /// A conversation message (user, assistant, or tool result).
    Message {
        /// Stable identifier for this message.
        uuid: MessageId,
        /// When the message was added.
        timestamp: SystemTime,
        /// The wrapped message.
        message: ConversationMessage,
    },
    /// A tool-use summary kept after eviction of the full tool output.
    ToolUseSummary {
        /// Tool use the summary refers to.
        tool_use_id: ToolUseId,
        /// Human/agent-readable summary of the tool's effect.
        summary: String,
    },
    /// Boundary inserted by the compaction subsystem.
    CompactBoundary {
        /// Summary of compacted content.
        summary: String,
    },
    /// Tombstone replacing a previously-recorded message.
    Tombstone {
        /// Message that was replaced.
        replaced_uuid: MessageId,
        /// Why the message was tombstoned.
        reason: String,
    },
    /// Result of a hook invocation.
    HookResult {
        /// Hook that produced the result.
        hook_id: HookId,
        /// Outcome description.
        outcome: String,
    },
    /// Marker emitted whenever a session is resumed.
    SessionResumed {
        /// Session id we resumed from.
        previous_session_id: SessionId,
        /// When the resume happened.
        resumed_at: SystemTime,
    },
}
