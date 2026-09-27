//! Resume-from-rollout reconstruction — faithful port of codex's
//! `InitialHistory` / `ResumedHistory` (`codex_protocol::protocol`) plus the
//! `RolloutRecorder::get_rollout_history` entry point (`rollout/src/recorder.rs`).
//!
//! Codex turns a loaded rollout file into an [`InitialHistory`]: the canonical
//! "reconstruct a session from a persisted rollout" capability. The variants
//! and the accessor helpers operate purely over the loaded [`RolloutItem`]
//! stream, so this port is host-independent — it reuses the already-ported
//! [`RolloutRecorder::load_rollout_items`] loader and reconstructs the same
//! resumable view.
//!
//! # Reconciliation with codex
//!
//! - Codex's `ResumedHistory.history` is `Arc<Vec<RolloutItem>>`; we keep the
//!   `Arc` so callers share the loaded history cheaply (matches codex).
//! - Accessors that depend on codex-only structured `SessionMeta` fields
//!   (`base_instructions`, `dynamic_tools`, `thread_source`,
//!   `multi_agent_version`) are intentionally NOT ported — LingXi's
//!   [`SessionMeta`] carries those as opaque `extra` JSON, and the higher-level
//!   subsystems that consume them (app-server / multi-agent) are out of scope.
//!   The host-independent accessors (`forked_from_id`, `session_cwd`,
//!   `get_rollout_items`, `get_event_msgs`, source/originator/parent lookups)
//!   ARE ported, operating on the opaque items already present.

use crate::rollout::record::{RolloutItem, SessionMeta, SessionSource, ThreadId};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Resumed-session view reconstructed from a rollout file.
///
/// Faithful analog of codex's `ResumedHistory` minus the codex-only protocol
/// coupling. `history` is shared via [`Arc`] so resume callers can hand the
/// loaded items to multiple consumers without cloning the vector.
#[derive(Debug, Clone)]
pub struct ResumedHistory {
    /// The canonical thread id taken from the first `SessionMeta` line.
    pub conversation_id: ThreadId,
    /// Every loaded rollout item (parse-tolerant; ghost snapshots already
    /// stripped by the loader), in file order.
    pub history: Arc<Vec<RolloutItem>>,
    /// Plain on-disk path the rollout was loaded from.
    pub rollout_path: Option<PathBuf>,
}

/// How a session's initial history was produced — faithful port of codex's
/// `InitialHistory`.
///
/// `New`/`Cleared` carry no items; `Resumed` reconstructs from a persisted
/// rollout; `Forked` carries an explicitly-provided item slice (e.g. a session
/// branched from another's history).
#[derive(Debug, Clone)]
pub enum InitialHistory {
    /// A brand-new session with no prior history.
    New,
    /// History was explicitly cleared mid-session.
    Cleared,
    /// History reconstructed from a persisted rollout file.
    Resumed(ResumedHistory),
    /// History branched from an explicitly-provided item slice.
    Forked(Vec<RolloutItem>),
}

impl InitialHistory {
    /// Whether any rollout item satisfies `predicate` (codex parity).
    pub fn scan_rollout_items(&self, mut predicate: impl FnMut(&RolloutItem) -> bool) -> bool {
        match self {
            InitialHistory::New | InitialHistory::Cleared => false,
            InitialHistory::Resumed(resumed) => resumed.history.iter().any(&mut predicate),
            InitialHistory::Forked(items) => items.iter().any(predicate),
        }
    }

    /// The thread this history forked from, if recorded on the session-meta
    /// line. For `Forked`, returns the meta's own id (codex parity).
    pub fn forked_from_id(&self) -> Option<ThreadId> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => {
                resumed.history.iter().find_map(|item| match item {
                    RolloutItem::SessionMeta(meta_line) => meta_line.meta.forked_from_id,
                    _ => None,
                })
            }
            InitialHistory::Forked(items) => items.iter().find_map(|item| match item {
                RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.id),
                _ => None,
            }),
        }
    }

    /// The session cwd recorded on the first session-meta line (codex parity).
    pub fn session_cwd(&self) -> Option<PathBuf> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => session_cwd_from_items(&resumed.history),
            InitialHistory::Forked(items) => session_cwd_from_items(items),
        }
    }

    /// All rollout items backing this history (empty for `New`/`Cleared`).
    pub fn get_rollout_items(&self) -> &[RolloutItem] {
        match self {
            InitialHistory::New | InitialHistory::Cleared => &[],
            InitialHistory::Resumed(resumed) => &resumed.history,
            InitialHistory::Forked(items) => items,
        }
    }

    /// The opaque `EventMsg` payloads carried in the history (codex parity).
    ///
    /// Codex returns typed `EventMsg`s; LingXi carries them as opaque
    /// [`Value`]s, so the returned payloads are the raw `EventMsg` bodies.
    pub fn get_event_msgs(&self) -> Option<Vec<Value>> {
        let items = match self {
            InitialHistory::New | InitialHistory::Cleared => return None,
            InitialHistory::Resumed(resumed) => resumed.history.as_slice(),
            InitialHistory::Forked(items) => items.as_slice(),
        };
        Some(
            items
                .iter()
                .filter_map(|ri| match ri {
                    RolloutItem::EventMsg(ev) => Some(ev.clone()),
                    _ => None,
                })
                .collect(),
        )
    }

    /// Originator string from the session-meta line, if non-empty.
    pub fn get_session_originator(&self) -> Option<String> {
        self.get_session_meta()
            .map(|meta| meta.originator.clone())
            .filter(|originator| !originator.is_empty())
    }

    /// Parent thread id, only for a resumed history (codex parity).
    pub fn get_resumed_parent_thread_id(&self) -> Option<ThreadId> {
        self.get_resumed_session_meta()
            .and_then(|meta| meta.parent_thread_id)
    }

    /// Session source, only for a resumed history (codex parity).
    pub fn get_resumed_session_source(&self) -> Option<SessionSource> {
        self.get_resumed_session_meta()
            .map(|meta| meta.source.clone())
    }

    fn get_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            InitialHistory::New | InitialHistory::Cleared => None,
            InitialHistory::Resumed(resumed) => session_meta_from_items(&resumed.history),
            InitialHistory::Forked(items) => session_meta_from_items(items),
        }
    }

    fn get_resumed_session_meta(&self) -> Option<&SessionMeta> {
        match self {
            InitialHistory::New | InitialHistory::Cleared | InitialHistory::Forked(_) => None,
            InitialHistory::Resumed(resumed) => session_meta_from_items(&resumed.history),
        }
    }
}

fn session_cwd_from_items(items: &[RolloutItem]) -> Option<PathBuf> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.cwd.clone()),
        _ => None,
    })
}

fn session_meta_from_items(items: &[RolloutItem]) -> Option<&SessionMeta> {
    items.iter().find_map(|item| match item {
        RolloutItem::SessionMeta(meta_line) => Some(&meta_line.meta),
        _ => None,
    })
}
