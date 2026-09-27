//! Token-usage replay attribution — port of codex's
//! `app-server/src/request_processors/token_usage_replay.rs`.
//!
//! # What this ports, and what it deliberately does not
//!
//! Codex's app-server replays a persisted `TokenCount` snapshot back to a
//! client that re-attaches to an existing thread, emitting a
//! `ThreadTokenUsageUpdated` notification scoped to the right turn. That module
//! has two halves:
//!
//! 1. **Notification plumbing** —
//!    `send_thread_token_usage_update_to_connection` builds a v2
//!    `ThreadTokenUsageUpdatedNotification` and sends it to one connection via
//!    the app-server's `OutgoingMessageSender`. LingXi has **no app-server**, no
//!    v2 `Thread`/`ThreadTokenUsage` protocol, and no per-connection outgoing
//!    sender, so there is no host for this half. It is intentionally omitted —
//!    see the blocker note returned with this port.
//!
//! 2. **Attribution logic** — `latest_token_usage_turn_id_from_rollout_items`
//!    walks the persisted rollout history, finds the turn that was *active* when
//!    the latest `TokenCount` event was persisted, and maps it back to a turn id
//!    in the rebuilt thread (preferring the explicit id, falling back to the
//!    turn *position* when implicit ids were regenerated on reconstruction).
//!    This is the load-bearing, host-independent half and is what this module
//!    ports.
//!
//! # Reconciliation with LingXi's types
//!
//! Codex's attribution leans on `ThreadHistoryBuilder` + the structured v2
//! `Turn`/`TurnStatus`/`EventMsg` enums. LingXi has none of those: rollout
//! `EventMsg`s are opaque [`serde_json::Value`]s (see
//! `session::rollout::record::RolloutItem`), and there is no v2 `Turn`. So this
//! port:
//!
//! - operates on opaque rollout event JSON, detecting `TokenCount` and turn
//!   boundaries via the inner `"type"` discriminant (exactly the way
//!   `session::rollout::policy` already inspects opaque events);
//! - reconstructs turn *positions* with the same boundary rules
//!   `ThreadHistoryBuilder` uses (`turn_started` opens an explicit turn;
//!   `turn_complete` closes the active turn; a `user_message` closes a
//!   non-explicit, non-empty turn and starts a fresh one; `thread_rolled_back`
//!   truncates the last N turns);
//! - takes the rebuilt turn list as a caller-supplied slice of [`ReplayTurn`]
//!   (`id` + [`ReplayTurnStatus`]), the minimal projection of codex's `Turn`
//!   that the attribution rules actually read.
//!
//! The two attribution functions ([`latest_token_usage_turn_id_from_events`] and
//! [`latest_token_usage_turn_id`]) reproduce codex's behaviour 1:1 over those
//! reconciled inputs. The accompanying tests mirror codex's
//! `replay_attribution_*` cases.

use serde_json::Value;

/// Turn status, projected from codex's v2 `TurnStatus`. Only the
/// `Completed`/`Failed` distinction is load-bearing for replay attribution
/// (it selects the fallback owner turn); the remaining arms preserve the wire
/// shape so callers can map their own turn records faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayTurnStatus {
    /// Turn is still running.
    InProgress,
    /// Turn finished normally.
    Completed,
    /// Turn failed.
    Failed,
    /// Turn was interrupted / aborted.
    Interrupted,
}

/// Minimal rebuilt-turn projection the attribution rules read: the turn id and
/// its status. Mirrors the fields of codex's v2 `Turn` that
/// `latest_token_usage_turn_id*` actually touch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTurn {
    /// Turn identifier as it appears in the rebuilt thread.
    pub id: String,
    /// Turn lifecycle status.
    pub status: ReplayTurnStatus,
}

/// Inner `"type"` discriminant of an opaque rollout event payload, if present.
///
/// Matches the convention used by `session::rollout::policy::payload_type`.
fn event_type(event: &Value) -> Option<&str> {
    event.get("type").and_then(Value::as_str)
}

/// Identifies the turn that was active when a `TokenCount` record appeared.
///
/// Port of codex's private `TokenUsageTurnOwner`. The `id` is preferred when it
/// still appears in the rebuilt thread; `position` is the fallback for
/// histories whose implicit turn ids are regenerated during reconstruction.
struct TokenUsageTurnOwner {
    id: String,
    position: Option<usize>,
}

/// Walks persisted rollout events, reconstructing turn boundaries the same way
/// codex's `ThreadHistoryBuilder` does, so the `position` and `id` of the turn
/// active at each `TokenCount` can be recovered.
///
/// LingXi rollout events are opaque JSON; this reproduces only the boundary
/// behaviour the attribution math depends on (open/close/implicit-restart/
/// rollback), not full item materialization.
#[derive(Default)]
struct TurnPositionTracker {
    /// Ids of turns already closed (their final positions, in order).
    finished: Vec<String>,
    /// The currently open turn, if any.
    current: Option<OpenTurn>,
}

struct OpenTurn {
    id: String,
    /// True when opened by an explicit `turn_started` boundary.
    opened_explicitly: bool,
    /// True once the turn has accumulated at least one renderable item.
    has_items: bool,
    /// True when the turn saw a persisted `Compacted` marker.
    saw_compaction: bool,
}

impl TurnPositionTracker {
    /// Snapshot of the turn that is "active" right now: the open turn if one
    /// exists, otherwise the most recently finished turn. Mirrors
    /// `ThreadHistoryBuilder::active_turn_snapshot().id`.
    fn active_turn_id(&self) -> Option<String> {
        if let Some(open) = self.current.as_ref() {
            Some(open.id.clone())
        } else {
            self.finished.last().cloned()
        }
    }

    /// Position the active turn occupies (or will occupy) in the finished list.
    /// Mirrors `ThreadHistoryBuilder::active_turn_position`.
    fn active_turn_position(&self) -> Option<usize> {
        if self.current.is_some() {
            Some(self.finished.len())
        } else if self.finished.is_empty() {
            None
        } else {
            Some(self.finished.len() - 1)
        }
    }

    fn finish_current_turn(&mut self) {
        if let Some(turn) = self.current.take() {
            // Codex drops empty turns that were neither opened explicitly nor
            // saw a compaction marker.
            if !turn.has_items && !turn.opened_explicitly && !turn.saw_compaction {
                return;
            }
            self.finished.push(turn.id);
        }
    }

    fn ensure_turn(&mut self, rollout_index: usize) -> &mut OpenTurn {
        if self.current.is_none() {
            self.current = Some(OpenTurn {
                id: format!("rollout-{rollout_index}"),
                opened_explicitly: false,
                has_items: false,
                saw_compaction: false,
            });
        }
        self.current.as_mut().expect("current turn just ensured")
    }

    /// Apply one opaque rollout event at `rollout_index`, updating turn
    /// boundaries. `rollout_index` is 0-based over the full rollout-item list,
    /// matching codex's `current_rollout_index`.
    fn handle_event(&mut self, event: &Value, rollout_index: usize) {
        match event_type(event) {
            Some("turn_started") => {
                self.finish_current_turn();
                let id = event
                    .get("turn_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("rollout-{rollout_index}"));
                self.current = Some(OpenTurn {
                    id,
                    opened_explicitly: true,
                    has_items: false,
                    saw_compaction: false,
                });
            }
            Some("turn_complete") => {
                // Codex matches the completing turn id; for position tracking it
                // is sufficient to close the active turn.
                self.finish_current_turn();
            }
            Some("user_message") => {
                // A user message closes an implicit, non-empty (and not
                // compaction-only) turn before starting a fresh one, mirroring
                // `handle_user_message`.
                let should_close = self.current.as_ref().is_some_and(|open| {
                    !open.opened_explicitly && !(open.saw_compaction && !open.has_items)
                });
                if should_close {
                    self.finish_current_turn();
                }
                let turn = self.ensure_turn(rollout_index);
                turn.has_items = true;
            }
            Some("thread_rolled_back") => {
                self.finish_current_turn();
                let n = event
                    .get("num_turns")
                    .and_then(Value::as_u64)
                    .map(|n| usize::try_from(n).unwrap_or(usize::MAX))
                    .unwrap_or(0);
                let len = self.finished.len();
                if n >= len {
                    self.finished.clear();
                } else {
                    self.finished.truncate(len - n);
                }
            }
            Some("token_count") => {
                // TokenCount itself produces no item and opens no turn (codex's
                // `handle_event` is a no-op for it); attribution is read by the
                // caller via `active_turn_*` before this is applied.
            }
            Some(_) => {
                // Any other persisted event materializes an item in the current
                // turn, so an implicit turn must exist and is marked non-empty.
                let turn = self.ensure_turn(rollout_index);
                turn.has_items = true;
            }
            None => {}
        }
    }
}

/// Chooses the turn id that should own a replayed token-usage update, given the
/// persisted rollout events and the rebuilt turn list.
///
/// Port of codex's `latest_token_usage_turn_id_from_rollout_items`, reconciled
/// to LingXi's opaque rollout events ([`serde_json::Value`]) and minimal
/// [`ReplayTurn`] projection.
///
/// The id of the turn active at the *latest* persisted `TokenCount` is preferred
/// when it still appears in `turns`; otherwise the active turn *position* is
/// used to index `turns` (the fallback for histories whose implicit turn ids
/// were regenerated on reconstruction).
#[must_use]
pub fn latest_token_usage_turn_id_from_events(
    rollout_events: &[Value],
    turns: &[ReplayTurn],
) -> Option<String> {
    let mut tracker = TurnPositionTracker::default();
    let mut owner: Option<TokenUsageTurnOwner> = None;

    for (index, event) in rollout_events.iter().enumerate() {
        if matches!(event_type(event), Some("token_count")) {
            owner = tracker.active_turn_id().map(|id| TokenUsageTurnOwner {
                id,
                position: tracker.active_turn_position(),
            });
        }
        tracker.handle_event(event, index);
    }

    let owner = owner?;
    if turns.iter().any(|turn| turn.id == owner.id) {
        Some(owner.id)
    } else {
        owner
            .position
            .and_then(|position| turns.get(position))
            .map(|turn| turn.id.clone())
    }
}

/// Chooses a fallback turn id that should own a replayed token-usage update when
/// the rollout positions cannot be read.
///
/// Port of codex's private `latest_token_usage_turn_id(thread)`. Prefers the
/// last `Completed`/`Failed` turn, else the last turn, else the empty string —
/// preserving a stable wire shape for unusual histories.
#[must_use]
pub fn latest_token_usage_turn_id(turns: &[ReplayTurn]) -> String {
    turns
        .iter()
        .rev()
        .find(|turn| {
            matches!(
                turn.status,
                ReplayTurnStatus::Completed | ReplayTurnStatus::Failed
            )
        })
        .or_else(|| turns.last())
        .map(|turn| turn.id.clone())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Rebuilds the turn list the way a resumer would: each persisted
    /// `user_message`/`turn_started` boundary yields one turn, in order. This
    /// mirrors the turn ids `TurnPositionTracker` synthesizes so the
    /// "id matches" path is exercised.
    fn build_turns_from_events(events: &[Value]) -> Vec<ReplayTurn> {
        let mut tracker = TurnPositionTracker::default();
        for (index, event) in events.iter().enumerate() {
            tracker.handle_event(event, index);
        }
        tracker.finish_current_turn();
        tracker
            .finished
            .into_iter()
            .map(|id| ReplayTurn {
                id,
                status: ReplayTurnStatus::Completed,
            })
            .collect()
    }

    fn token_usage_history() -> Vec<Value> {
        vec![
            json!({ "type": "user_message", "message": "first turn" }),
            json!({ "type": "agent_message", "message": "first answer" }),
            json!({ "type": "token_count", "info": null, "rate_limits": null }),
            json!({ "type": "user_message", "message": "second turn" }),
        ]
    }

    #[test]
    fn replay_attribution_uses_already_loaded_history() {
        // Port of codex's `replay_attribution_uses_already_loaded_history`:
        // the TokenCount is attributed to the first turn (the one active when
        // it was persisted), whose id still appears in the rebuilt thread.
        let events = token_usage_history();
        let turns = build_turns_from_events(&events);
        assert_eq!(turns.len(), 2, "two user-message turns");

        assert_eq!(
            latest_token_usage_turn_id_from_events(&events, turns.as_slice()),
            Some(turns[0].id.clone())
        );
    }

    #[test]
    fn replay_attribution_falls_back_to_rebuilt_turn_position() {
        // Port of codex's `replay_attribution_falls_back_to_rebuilt_turn_position`:
        // when the original id no longer matches (regenerated on rebuild), the
        // active-turn position selects the owner instead.
        let events = token_usage_history();
        let mut turns = build_turns_from_events(&events);
        turns[0].id = "rebuilt-turn-id".to_string();

        assert_eq!(
            latest_token_usage_turn_id_from_events(&events, turns.as_slice()),
            Some("rebuilt-turn-id".to_string())
        );
    }

    #[test]
    fn no_token_count_yields_no_owner() {
        let events = vec![
            json!({ "type": "user_message", "message": "hi" }),
            json!({ "type": "agent_message", "message": "yo" }),
        ];
        let turns = build_turns_from_events(&events);
        assert_eq!(
            latest_token_usage_turn_id_from_events(&events, turns.as_slice()),
            None
        );
    }

    #[test]
    fn latest_token_usage_turn_id_prefers_completed_then_last() {
        // Empty -> empty string.
        assert_eq!(latest_token_usage_turn_id(&[]), String::new());

        // Last Completed/Failed wins over a trailing in-progress turn.
        let turns = vec![
            ReplayTurn {
                id: "t0".into(),
                status: ReplayTurnStatus::Completed,
            },
            ReplayTurn {
                id: "t1".into(),
                status: ReplayTurnStatus::Failed,
            },
            ReplayTurn {
                id: "t2".into(),
                status: ReplayTurnStatus::InProgress,
            },
        ];
        assert_eq!(latest_token_usage_turn_id(&turns), "t1".to_string());

        // No completed/failed -> last turn.
        let turns = vec![
            ReplayTurn {
                id: "a".into(),
                status: ReplayTurnStatus::InProgress,
            },
            ReplayTurn {
                id: "b".into(),
                status: ReplayTurnStatus::Interrupted,
            },
        ];
        assert_eq!(latest_token_usage_turn_id(&turns), "b".to_string());
    }
}
