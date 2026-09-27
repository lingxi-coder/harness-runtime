//! Stable snapshot and decision types for moving a live interactive session
//! into a background worker.
//!
//! Backgrounding is not equivalent to cancelling a turn.  The caller must
//! preserve queued commands and the current draft, and must distinguish a
//! clean turn boundary from model streaming and tool execution.  This module
//! keeps that policy pure so CLI, TUI, SDK hosts, and persisted launch specs
//! share one contract.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How long a between-tools transition waits for the current safe boundary.
pub const DEFAULT_BACKGROUND_DEFER_MS: u64 = 10_000;

/// Data that must survive moving a live session into the background.
///
/// Every payload field is defaulted so launch specs written before 2.1.220
/// remain readable.  A nil `boundary_id` means the older host did not provide a
/// correlation id; it never authorizes an otherwise unsafe restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BackgroundingSnapshot {
    /// No model or tool work is active.  The session can be forked immediately.
    Idle {
        /// Commands waiting behind the current composer.
        #[serde(default)]
        queued_commands: Vec<String>,
        /// Unsubmitted composer text.
        #[serde(default)]
        draft: String,
        /// Stable boundary used to reject stale background requests.
        #[serde(default)]
        boundary_id: Uuid,
    },
    /// Model streaming is complete and one or more tools are still active.
    BetweenTools {
        /// Commands waiting behind the current turn.
        #[serde(default)]
        queued_commands: Vec<String>,
        /// Unsubmitted composer text.
        #[serde(default)]
        draft: String,
        /// Names/kinds of active tool calls, in dispatch order.
        #[serde(default)]
        in_flight_kinds: Vec<String>,
        /// Assistant text received before the active tool boundary.
        #[serde(default)]
        partial_text: String,
        /// Stable boundary used to reject stale background requests.
        #[serde(default)]
        boundary_id: Uuid,
        /// Number of active calls that can be safely restarted after abort.
        #[serde(default)]
        restartable_count: usize,
    },
    /// Assistant text is still arriving.
    Streaming {
        /// Commands waiting behind the current turn.
        #[serde(default)]
        queued_commands: Vec<String>,
        /// Unsubmitted composer text.
        #[serde(default)]
        draft: String,
        /// Tool calls already started while the model continues streaming.
        #[serde(default)]
        in_flight_kinds: Vec<String>,
        /// Assistant text received so far.  It is retained for an abort/fork
        /// handoff and must never be silently discarded.
        #[serde(default)]
        partial_text: String,
        /// Stable boundary used to reject stale background requests.
        #[serde(default)]
        boundary_id: Uuid,
        /// Number of active calls that can be safely restarted after abort.
        #[serde(default)]
        restartable_count: usize,
    },
}

impl Default for BackgroundingSnapshot {
    fn default() -> Self {
        Self::Idle {
            queued_commands: Vec::new(),
            draft: String::new(),
            boundary_id: Uuid::nil(),
        }
    }
}

impl BackgroundingSnapshot {
    /// Commands that must be replayed after the handoff.
    #[must_use]
    pub fn queued_commands(&self) -> &[String] {
        match self {
            Self::Idle {
                queued_commands, ..
            }
            | Self::BetweenTools {
                queued_commands, ..
            }
            | Self::Streaming {
                queued_commands, ..
            } => queued_commands,
        }
    }

    /// The unsubmitted draft that must remain in the destination composer.
    #[must_use]
    pub fn draft(&self) -> &str {
        match self {
            Self::Idle { draft, .. }
            | Self::BetweenTools { draft, .. }
            | Self::Streaming { draft, .. } => draft,
        }
    }

    /// Correlation boundary for this snapshot.
    #[must_use]
    pub fn boundary_id(&self) -> Uuid {
        match self {
            Self::Idle { boundary_id, .. }
            | Self::BetweenTools { boundary_id, .. }
            | Self::Streaming { boundary_id, .. } => *boundary_id,
        }
    }

    /// In-flight tool kinds captured at the handoff boundary.
    #[must_use]
    pub fn in_flight_kinds(&self) -> &[String] {
        match self {
            Self::Idle { .. } => &[],
            Self::BetweenTools {
                in_flight_kinds, ..
            }
            | Self::Streaming {
                in_flight_kinds, ..
            } => in_flight_kinds,
        }
    }

    /// Partial assistant reply retained by a streaming handoff.
    #[must_use]
    pub fn partial_text(&self) -> &str {
        match self {
            Self::BetweenTools { partial_text, .. } | Self::Streaming { partial_text, .. } => {
                partial_text
            }
            Self::Idle { .. } => "",
        }
    }

    /// Number of in-flight calls that are safe to restart.
    #[must_use]
    pub fn restartable_count(&self) -> usize {
        match self {
            Self::Idle { .. } => 0,
            Self::BetweenTools {
                restartable_count, ..
            }
            | Self::Streaming {
                restartable_count, ..
            } => *restartable_count,
        }
    }
}

/// The operation the host performs for a background request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundingDecision {
    /// Snapshot and fork immediately at an idle boundary.
    IdleFork,
    /// Keep the foreground turn alive while waiting for a safe boundary.
    DeferThenFork {
        /// Remaining wait budget.
        remaining_ms: u64,
    },
    /// Abort the active restartable work, persist the partial state, then fork.
    AbortThenFork,
    /// Refuse instead of dropping non-restartable work.
    Refuse {
        /// Stable, user-facing explanation.
        reason: &'static str,
    },
}

/// Classify a background request after `elapsed_ms` spent waiting.
///
/// Streaming work aborts only when every active tool is restartable.  Work
/// between tools first receives the standard defer window; after the timeout
/// it follows the same restartability rule.  This deliberately fails closed
/// when a legacy snapshot has active tools but no restartability metadata.
#[must_use]
pub fn classify_backgrounding(
    snapshot: &BackgroundingSnapshot,
    elapsed_ms: u64,
) -> BackgroundingDecision {
    match snapshot {
        BackgroundingSnapshot::Idle { .. } => BackgroundingDecision::IdleFork,
        BackgroundingSnapshot::BetweenTools { .. } if elapsed_ms < DEFAULT_BACKGROUND_DEFER_MS => {
            BackgroundingDecision::DeferThenFork {
                remaining_ms: DEFAULT_BACKGROUND_DEFER_MS - elapsed_ms,
            }
        }
        BackgroundingSnapshot::BetweenTools { .. } | BackgroundingSnapshot::Streaming { .. } => {
            let active = snapshot.in_flight_kinds().len();
            if active == 0 || snapshot.restartable_count() == active {
                BackgroundingDecision::AbortThenFork
            } else {
                BackgroundingDecision::Refuse {
                    reason: "Cannot background while non-restartable tools are running",
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boundary() -> Uuid {
        Uuid::parse_str("12345678-1234-5678-9234-567812345678").unwrap()
    }

    #[test]
    fn idle_forks_without_losing_queued_input() {
        let snapshot = BackgroundingSnapshot::Idle {
            queued_commands: vec!["/compact".into(), "continue".into()],
            draft: "unfinished".into(),
            boundary_id: boundary(),
        };
        assert_eq!(
            classify_backgrounding(&snapshot, 0),
            BackgroundingDecision::IdleFork
        );
        assert_eq!(snapshot.queued_commands().len(), 2);
        assert_eq!(snapshot.draft(), "unfinished");
        assert_eq!(snapshot.boundary_id(), boundary());
    }

    #[test]
    fn between_tools_defers_for_ten_seconds_then_aborts_restartable_work() {
        let snapshot = BackgroundingSnapshot::BetweenTools {
            queued_commands: Vec::new(),
            draft: String::new(),
            in_flight_kinds: vec!["Read".into(), "Grep".into()],
            partial_text: "before tools".into(),
            boundary_id: boundary(),
            restartable_count: 2,
        };
        assert_eq!(
            classify_backgrounding(&snapshot, 1_500),
            BackgroundingDecision::DeferThenFork {
                remaining_ms: 8_500
            }
        );
        assert_eq!(
            classify_backgrounding(&snapshot, DEFAULT_BACKGROUND_DEFER_MS),
            BackgroundingDecision::AbortThenFork
        );
    }

    #[test]
    fn non_restartable_work_fails_closed_after_defer() {
        let snapshot = BackgroundingSnapshot::BetweenTools {
            queued_commands: vec!["next".into()],
            draft: "draft".into(),
            in_flight_kinds: vec!["Bash".into()],
            partial_text: String::new(),
            boundary_id: boundary(),
            restartable_count: 0,
        };
        assert!(matches!(
            classify_backgrounding(&snapshot, DEFAULT_BACKGROUND_DEFER_MS),
            BackgroundingDecision::Refuse { .. }
        ));
        assert_eq!(snapshot.queued_commands(), &["next"]);
        assert_eq!(snapshot.draft(), "draft");
    }

    #[test]
    fn streaming_aborts_only_when_every_tool_is_restartable() {
        let mut snapshot = BackgroundingSnapshot::Streaming {
            queued_commands: Vec::new(),
            draft: "preserve me".into(),
            in_flight_kinds: vec!["Read".into(), "Bash".into()],
            partial_text: "partial assistant".into(),
            boundary_id: boundary(),
            restartable_count: 1,
        };
        assert!(matches!(
            classify_backgrounding(&snapshot, 0),
            BackgroundingDecision::Refuse { .. }
        ));
        if let BackgroundingSnapshot::Streaming {
            restartable_count, ..
        } = &mut snapshot
        {
            *restartable_count = 2;
        }
        assert_eq!(
            classify_backgrounding(&snapshot, 0),
            BackgroundingDecision::AbortThenFork
        );
    }

    #[test]
    fn old_or_minimal_payload_defaults_safely() {
        let idle: BackgroundingSnapshot = serde_json::from_str(r#"{"state":"idle"}"#).unwrap();
        assert_eq!(idle, BackgroundingSnapshot::default());

        let legacy_active: BackgroundingSnapshot =
            serde_json::from_str(r#"{"state":"between_tools","in_flight_kinds":["Bash"]}"#)
                .unwrap();
        assert!(matches!(
            classify_backgrounding(&legacy_active, DEFAULT_BACKGROUND_DEFER_MS),
            BackgroundingDecision::Refuse { .. }
        ));
    }
}
