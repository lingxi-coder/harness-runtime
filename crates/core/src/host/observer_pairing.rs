//! Observer pairings — the runtime state behind observer agents, ported from
//! claude-code 2.1.270 (`session.observers`).
//!
//! A pairing binds an OBSERVED agent to the OBSERVER agent watching it. It is
//! keyed by the observed agent (the main session uses the literal key
//! [`MAIN_PAIRING_KEY`], oracle `jW`), and it is the thing `ObserverReport`
//! resolves its destination from — an observer never names a recipient.
//!
//! This is runtime state, not session state: it holds an undelivered digest
//! buffer and the context a delivery runs under, so it is attached to the
//! session with `#[serde(skip)]` rather than persisted. The oracle does the
//! same — what survives a restart is a POINTER (`observerTaskId` +
//! `armingPermissionMode`) written next to the observed task, which the resume
//! path re-arms from.

use crate::host::subagent_spawn::ObserverSpec;
use crate::types::AgentId;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Oracle `jW`: the key the MAIN session's pairing is filed under, as opposed
/// to a subagent's, which is keyed by its own task id.
pub const MAIN_PAIRING_KEY: &str = "main";

/// Where a pairing is in its life. The oracle keeps a terminal pairing in the
/// map rather than deleting it, so a later report is refused with a reason
/// instead of looking like a pairing that never existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingState {
    /// Delivering, and accepting reports.
    Armed,
    /// Permission refused the observer at arm time. Terminal.
    Denied,
    /// The observer was stopped by the user. Terminal.
    Stopped,
    /// The observed agent finished, so the pairing was swept. Terminal.
    Retired,
}

impl PairingState {
    /// Only an armed pairing delivers or accepts a report.
    #[must_use]
    pub const fn is_armed(self) -> bool {
        matches!(self, Self::Armed)
    }
}

/// One queued digest awaiting delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDigest {
    /// The rendered digest.
    pub digest: String,
    /// The user message that triggered this batch, when there was one.
    pub trigger: Option<String>,
}

/// A live observed↔observer binding.
#[derive(Debug, Clone)]
pub struct ObserverPairing {
    /// The observer agent's own task id — what `ObserverReport` is matched on.
    pub observer_task_id: AgentId,
    /// The observer's agent type.
    pub observer_agent_type: String,
    /// The observed agent's task id. `None` for the main session.
    pub observed_task_id: Option<AgentId>,
    /// Slugged display name of the observed agent, used in the digest envelope.
    pub observed_envelope_name: String,
    /// Where a report is delivered. `None` means the main session.
    pub report_target_task_id: Option<AgentId>,
    /// Display name of the report target, for the brief and the refusal text.
    pub report_target_name: String,
    /// Set when the observed agent is a coordinator's WORKER: the report goes
    /// to the coordinator, so the brief tells the observer to name the worker.
    pub via_worker_name: Option<String>,
    /// The declaration's own instruction, appended after the digest postamble.
    pub observer_message: Option<String>,
    /// Whether the declaration propagates to the observed agent's own spawns.
    pub fanout_to_subagents: bool,
    /// How deep observer-only fanout has gone.
    pub fanout_depth: u32,
    /// Lifecycle.
    pub state: PairingState,
    /// Digests queued but not yet delivered.
    pub buffer: Vec<QueuedDigest>,
    /// Whether a delivery loop is already draining [`Self::buffer`].
    pub delivering: bool,
    /// Whether the observer's first run has been spawned.
    pub first_run_done: bool,
}

impl ObserverPairing {
    /// A fresh armed pairing.
    #[must_use]
    pub fn armed(
        observer_task_id: AgentId,
        spec: &ObserverSpec,
        observed_envelope_name: String,
        report_target_name: String,
    ) -> Self {
        Self {
            observer_task_id,
            observer_agent_type: spec.agent.clone(),
            observed_task_id: None,
            observed_envelope_name,
            report_target_task_id: None,
            report_target_name,
            via_worker_name: None,
            observer_message: spec.message.clone(),
            fanout_to_subagents: spec.observe_subagents,
            fanout_depth: 0,
            state: PairingState::Armed,
            buffer: Vec::new(),
            delivering: false,
            first_run_done: false,
        }
    }
}

/// What a pairing needs about the OBSERVED agent, captured while its spawn
/// request is still intact.
///
/// The observer is spawned from a REWRITTEN copy of that request — its
/// `observer` declaration and `name` are cleared and its `description` is
/// rewritten to `"<observer>@<observed>"` — so nothing about the observed
/// agent can be recovered from it afterwards. Reading the observed identity
/// off that copy names the wrong agent and arms nothing; this type exists so
/// that is not possible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObserverPairingSeed {
    /// The declaration, which the observer's own request no longer carries.
    pub spec: ObserverSpec,
    /// The OBSERVED agent's display name, before slugging.
    pub observed_name: String,
    /// The coordinator that spawned the observed agent, when it is a worker.
    pub observed_creator: Option<AgentId>,
    /// That coordinator's DISPLAY name. The brief says "it delivers to X" and
    /// the refusal names X, so an agent id here would put a uuid in front of a
    /// model. `None` falls back to the id.
    pub observed_creator_name: Option<String>,
}

/// The session's pairing table (oracle `session.observers`).
#[derive(Debug, Default)]
pub struct ObserverPairings {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    pairings: HashMap<String, ObserverPairing>,
    /// Oracle `mainSlotBlocked`: once the main pairing is stopped, the slot
    /// stays blocked so it is not silently re-armed for the same session.
    main_slot_blocked: bool,
}

impl ObserverPairings {
    /// Empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Oracle `KJe`: the pairing filed under `key`, whatever its state.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<ObserverPairing> {
        self.lock().pairings.get(key).cloned()
    }

    /// File a pairing under `key`, replacing any previous one.
    pub fn insert(&self, key: impl Into<String>, pairing: ObserverPairing) {
        self.lock().pairings.insert(key.into(), pairing);
    }

    /// Oracle `Wsr`: the ARMED pairing whose observer is `agent`.
    ///
    /// This is what `ObserverReport` resolves its destination from, which is
    /// why it insists on `Armed` — a report from a stopped or retired observer
    /// is refused with a reason rather than delivered into a dead target.
    #[must_use]
    pub fn armed_for_observer(&self, agent: &AgentId) -> Option<ObserverPairing> {
        self.lock()
            .pairings
            .values()
            .find(|p| &p.observer_task_id == agent && p.state.is_armed())
            .cloned()
    }

    /// Oracle `qAt`: is `agent` the observer of ANY pairing, armed or not?
    ///
    /// Deliberately broader than [`Self::armed_for_observer`]: it gates
    /// `SendMessage`, and an observer whose pairing has gone terminal must
    /// still be told to use `ObserverReport` rather than handed a messaging
    /// path it never had.
    #[must_use]
    pub fn is_observer(&self, agent: &AgentId) -> bool {
        self.lock()
            .pairings
            .values()
            .any(|p| &p.observer_task_id == agent)
    }

    /// Whether the main pairing slot has been blocked by a stop.
    #[must_use]
    pub fn main_slot_blocked(&self) -> bool {
        self.lock().main_slot_blocked
    }

    /// Oracle `KAt`: sweep the pairings that belong to finished subagents.
    ///
    /// Keeps the main pairing and anything with an explicit report target;
    /// everything else is retired (armed ones flipped first, so a late digest
    /// sees a terminal state) and dropped. Also unblocks the main slot.
    pub fn retire_finished(&self) {
        let mut inner = self.lock();
        let doomed: Vec<String> = inner
            .pairings
            .iter()
            .filter(|(key, p)| {
                key.as_str() == MAIN_PAIRING_KEY || p.report_target_task_id.is_none()
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in doomed {
            if let Some(p) = inner.pairings.get_mut(&key) {
                if p.state.is_armed() {
                    p.state = PairingState::Retired;
                }
            }
            inner.pairings.remove(&key);
        }
        inner.main_slot_blocked = false;
    }

    /// Mark a pairing terminal without removing it, so a later report is
    /// refused with a reason.
    pub fn set_state(&self, key: &str, state: PairingState) {
        if let Some(p) = self.lock().pairings.get_mut(key) {
            p.state = state;
        }
    }

    /// Oracle `zsr`: stop the main pairing and block its slot. Returns whether
    /// anything was actually stopped.
    pub fn stop_main(&self) -> bool {
        let mut inner = self.lock();
        let stopped = inner.pairings.get_mut(MAIN_PAIRING_KEY).is_some_and(|p| {
            if p.state.is_armed() {
                p.state = PairingState::Stopped;
                true
            } else {
                false
            }
        });
        if stopped {
            inner.main_slot_blocked = true;
        }
        stopped
    }

    /// Queue a digest for delivery. Returns false when the pairing is missing
    /// or terminal, which is the oracle's early `return` in `cBn`.
    pub fn enqueue(&self, key: &str, digest: QueuedDigest) -> bool {
        let mut inner = self.lock();
        let Some(p) = inner.pairings.get_mut(key) else {
            return false;
        };
        if !p.state.is_armed() {
            return false;
        }
        p.buffer.push(digest);
        true
    }

    /// Take everything queued for `key`, marking the pairing as delivering.
    /// Returns `None` when another delivery already owns the buffer — the
    /// oracle's `if (n.delivering) return`.
    pub fn take_batch(&self, key: &str) -> Option<Vec<QueuedDigest>> {
        let mut inner = self.lock();
        let p = inner.pairings.get_mut(key)?;
        if p.delivering || !p.state.is_armed() || p.buffer.is_empty() {
            return None;
        }
        p.delivering = true;
        Some(std::mem::take(&mut p.buffer))
    }

    /// Release the delivery flag.
    pub fn finish_delivery(&self, key: &str) {
        if let Some(p) = self.lock().pairings.get_mut(key) {
            p.delivering = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ObserverSpec {
        ObserverSpec::new("reviewer")
    }

    fn pairing(observer: AgentId) -> ObserverPairing {
        ObserverPairing::armed(observer, &spec(), "worker-1".into(), "main".into())
    }

    #[test]
    fn a_report_resolves_only_through_an_armed_pairing() {
        let t = ObserverPairings::new();
        let obs = AgentId::new();
        t.insert(MAIN_PAIRING_KEY, pairing(obs));
        assert!(t.armed_for_observer(&obs).is_some());

        t.set_state(MAIN_PAIRING_KEY, PairingState::Stopped);
        assert!(
            t.armed_for_observer(&obs).is_none(),
            "a stopped pairing must not deliver"
        );
        // …but it is still recognisably an observer, so SendMessage stays shut.
        assert!(t.is_observer(&obs));
    }

    /// The two lookups differ on purpose. If `is_observer` narrowed to armed,
    /// a stopped observer would get SendMessage back.
    #[test]
    fn is_observer_is_broader_than_armed_for_observer() {
        let t = ObserverPairings::new();
        let obs = AgentId::new();
        t.insert("k", pairing(obs));
        t.set_state("k", PairingState::Retired);
        assert!(t.armed_for_observer(&obs).is_none());
        assert!(t.is_observer(&obs));
        assert!(!t.is_observer(&AgentId::new()));
    }

    #[test]
    fn stopping_main_blocks_the_slot_and_only_fires_once() {
        let t = ObserverPairings::new();
        t.insert(MAIN_PAIRING_KEY, pairing(AgentId::new()));
        assert!(!t.main_slot_blocked());
        assert!(t.stop_main());
        assert!(t.main_slot_blocked());
        assert!(!t.stop_main(), "already stopped: nothing more to stop");
    }

    #[test]
    fn retiring_clears_the_pairings_and_unblocks_the_slot() {
        let t = ObserverPairings::new();
        t.insert(MAIN_PAIRING_KEY, pairing(AgentId::new()));
        t.stop_main();
        t.retire_finished();
        assert!(t.get(MAIN_PAIRING_KEY).is_none());
        assert!(!t.main_slot_blocked());
    }

    /// A pairing with an explicit report target belongs to a coordinator's
    /// worker and is not swept by the subagent sweep.
    #[test]
    fn a_pairing_with_a_report_target_survives_the_sweep() {
        let t = ObserverPairings::new();
        let mut p = pairing(AgentId::new());
        p.report_target_task_id = Some(AgentId::new());
        t.insert("worker-key", p);
        t.retire_finished();
        assert!(t.get("worker-key").is_some());
    }

    #[test]
    fn only_an_armed_pairing_queues_and_one_delivery_owns_the_buffer() {
        let t = ObserverPairings::new();
        t.insert("k", pairing(AgentId::new()));
        let d = QueuedDigest {
            digest: "d1".into(),
            trigger: None,
        };
        assert!(t.enqueue("k", d.clone()));
        assert!(!t.enqueue("missing", d.clone()));

        let batch = t.take_batch("k").expect("first taker gets the batch");
        assert_eq!(batch.len(), 1);
        t.enqueue("k", d.clone());
        assert!(
            t.take_batch("k").is_none(),
            "a second taker is locked out while the first is delivering"
        );
        t.finish_delivery("k");
        assert!(t.take_batch("k").is_some());

        t.set_state("k", PairingState::Stopped);
        assert!(!t.enqueue("k", d), "a terminal pairing stops queueing");
    }
}
