//! Host-owned conversation profiles and the model's current context inventory.
//!
//! Profiles describe behavior. Tool eligibility, authorization, model selection,
//! workspace identity and the user's request remain independent inputs.

use serde::{Deserialize, Serialize};

/// The behavior profile selected for a conversation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConversationMode {
    /// Research, explain, write and perform requested work with available tools.
    Chat,
    /// Inspect, implement and validate engineering work when requested.
    #[default]
    Code,
}

impl ConversationMode {
    /// Stable transcript and client wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Code => "code",
        }
    }

    /// Parse a persisted profile without guessing an unknown value.
    #[must_use]
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "chat" => Some(Self::Chat),
            "code" => Some(Self::Code),
            _ => None,
        }
    }
}

/// Read-only snapshot of the mode fixed for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionModeSnapshot {
    /// Owning conversation.
    pub session_id: String,
    /// Profile fixed when this session was created or migrated.
    pub active: ConversationMode,
    /// Equal to `active` in current sessions.
    pub selected: ConversationMode,
    /// Revision retained from the session's persisted profile.
    pub revision: u64,
    /// Always false in current sessions.
    pub pending: bool,
}

/// Durable profile and epoch. Historical records can carry a pending choice;
/// restore resolves those records to their admitted mode and epoch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModeTransition {
    /// Most recently selected profile.
    pub selected: ConversationMode,
    /// Profile admitted for the interrupted query.
    pub active: ConversationMode,
    /// Monotonic selection revision.
    pub revision: u64,
    /// Instruction epoch of the selected profile.
    pub selected_epoch: u64,
    /// Instruction epoch of the admitted profile.
    pub active_epoch: u64,
    /// Whether admission of the selection is still pending.
    pub pending: bool,
}

/// Why a context block is present; documents and tool output are evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextBlockKind {
    Instruction,
    Evidence,
    Capability,
    State,
}

/// Host-side metadata. Full prompt bodies and secrets are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBlockManifest {
    pub id: String,
    pub source: String,
    pub version: String,
    pub kind: ContextBlockKind,
    pub scope: String,
    pub reason: String,
    pub bytes: u64,
}

/// The inventory of the most recently assembled request, independent of the log.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextManifest {
    pub session: SessionModeSnapshot,
    /// True only after a request has been assembled for this revision.
    pub projected: bool,
    pub blocks: Vec<ContextBlockManifest>,
}
