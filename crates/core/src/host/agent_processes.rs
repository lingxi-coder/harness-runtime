//! Host-independent ownership of live OS processes.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

fn groups() -> &'static Mutex<HashMap<String, HashMap<u32, u64>>> {
    static GROUPS: OnceLock<Mutex<HashMap<String, HashMap<u32, u64>>>> = OnceLock::new();
    GROUPS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One registration from an owner snapshot; the token is local to this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationEntry {
    pub pid: u32,
    pub token: u64,
}

pub struct Registration {
    owner: String,
    entry: RegistrationEntry,
}
impl Registration {
    pub fn entry(&self) -> RegistrationEntry {
        self.entry
    }
}
impl Drop for Registration {
    fn drop(&mut self) {
        let mut owners = groups()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pids) = owners.get_mut(&self.owner) {
            if pids.get(&self.entry.pid) == Some(&self.entry.token) {
                pids.remove(&self.entry.pid);
            }
            if pids.is_empty() {
                owners.remove(&self.owner);
            }
        }
    }
}

pub fn register(owner: Option<&str>, pid: Option<u32>) -> Option<Registration> {
    let (owner, pid) = (owner?, pid?);
    static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);
    let token = NEXT_TOKEN
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .expect("process registration tokens exhausted");
    groups()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(owner.to_owned())
        .or_default()
        .insert(pid, token);
    Some(Registration {
        owner: owner.to_owned(),
        entry: RegistrationEntry { pid, token },
    })
}

/// Capture identities once before an asynchronous owner-stop operation.
pub fn snapshot_entries(owner: &str) -> Vec<RegistrationEntry> {
    groups()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(owner)
        .into_iter()
        .flat_map(|pids| {
            pids.iter()
                .map(|(&pid, &token)| RegistrationEntry { pid, token })
        })
        .collect()
}

/// Recheck a direct-process snapshot before falling back to native signaling.
pub fn is_current(owner: &str, entry: RegistrationEntry) -> bool {
    groups()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(owner)
        .and_then(|pids| pids.get(&entry.pid))
        .copied()
        == Some(entry.token)
}

/// Snapshot the live registered PIDs for one owner without consuming them.
pub fn snapshot(owner: &str) -> Vec<u32> {
    snapshot_entries(owner)
        .into_iter()
        .map(|entry| entry.pid)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reused_pid_keeps_new_registration_when_old_guard_drops() {
        let owner = "registration-generation-reuse";
        let old = register(Some(owner), Some(1234)).unwrap();
        let stale = old.entry();
        let new = register(Some(owner), Some(1234)).unwrap();
        assert_ne!(stale, new.entry());
        assert!(!is_current(owner, stale));
        assert!(is_current(owner, new.entry()));
        drop(old);
        assert_eq!(snapshot_entries(owner), vec![new.entry()]);
        drop(new);
        assert!(snapshot(owner).is_empty());
    }
}

// ---------------------------------------------------------------------------
// Stop-pending agent ids (claude-code `stopPendingAgentIds` on the same
// `PDt` registry that owns the live-process map above).
// ---------------------------------------------------------------------------

fn stop_pending() -> &'static Mutex<HashMap<String, u32>> {
    static STOP_PENDING: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    STOP_PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Marks an agent's stop as IN PROGRESS for as long as this value lives
/// (claude-code `_Ve` / `oSe` around the `Cre` kill-escalation window).
///
/// A kill here is cooperative before it is forced: the drop of a spawn future
/// sends `UserInterrupt` and then polls for up to `SPAWN_CANCEL_GRACE` before
/// deallocating, and the persistent path sends `UserExit` and awaits MCP
/// teardown before it does. The runner only *observes* those events where it
/// races them — at the model round-trip — so a runner that is part-way through
/// dispatching one assistant turn's `tool_use` blocks keeps dispatching the
/// rest. Without this window a dying agent can still launch new agents, skills
/// and workflows, which then outlive it.
///
/// Upstream additionally arms a 10s escalation and a 30s overdue timer, because
/// nothing else forces its loop to settle. This port's grace-poll-then-
/// `deallocate` already bounds the window, so the timers have no work to do
/// here; the observable — a stopping agent launches nothing — is the same.
#[derive(Debug)]
pub struct StopPending {
    agent_id: String,
}

impl Drop for StopPending {
    fn drop(&mut self) {
        let mut pending = stop_pending()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(depth) = pending.get_mut(&self.agent_id) {
            // Guards are 1:1 with markings by construction, so this cannot
            // underflow — but a panic unwinding out of a `Drop` aborts the
            // process, which is never the right way to report a bookkeeping
            // slip. Saturate and let the gate reopen instead.
            *depth = depth.saturating_sub(1);
            if *depth == 0 {
                pending.remove(&self.agent_id);
            }
        }
    }
}

/// Open the stop-pending window for `agent_id`. Hold the returned guard until
/// the stop has settled; the window closes when the LAST guard drops.
///
/// Nesting is the normal case, not an edge case: killing a persistent agent
/// runs the dropped-spawn cleanup AND `stop()`, so two windows overlap on one
/// agent id and can settle in either order. A refcount is what keeps the gate
/// shut until both are done — a single owner token would let whichever settled
/// first reopen it under the other.
#[must_use]
pub fn mark_stop_pending(agent_id: &str) -> StopPending {
    *stop_pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(agent_id.to_owned())
        .or_insert(0) += 1;
    StopPending {
        agent_id: agent_id.to_owned(),
    }
}

/// `a0(e)` — is this agent stopped with its stop still completing?
#[must_use]
pub fn is_stop_pending(agent_id: &str) -> bool {
    stop_pending()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains_key(agent_id)
}

/// The refusal a tool raises when its caller is stopping. Upstream spells the
/// tail per tool; the sentence up to it is shared.
#[must_use]
pub fn stop_pending_refusal(tail: &str) -> String {
    format!("This agent has been stopped and its stop is still completing; it cannot {tail}")
}

#[cfg(test)]
mod stop_pending_tests {
    use super::*;

    /// Every test picks a fresh id: the registry is process-global and the test
    /// binary runs these on parallel threads.
    fn fresh_id(tag: &str) -> String {
        static N: AtomicU64 = AtomicU64::new(0);
        format!("agent-{tag}-{}", N.fetch_add(1, Ordering::Relaxed))
    }

    #[test]
    fn the_window_opens_on_mark_and_closes_when_the_guard_drops() {
        let id = fresh_id("basic");
        assert!(!is_stop_pending(&id), "clean id starts open");
        {
            let _guard = mark_stop_pending(&id);
            assert!(is_stop_pending(&id));
        }
        assert!(!is_stop_pending(&id), "settling reopens the agent");
    }

    /// Killing a persistent agent opens two windows on one id — the dropped
    /// spawn's cleanup and `stop()` — and they can settle in either order. The
    /// gate must stay shut until BOTH are done.
    #[test]
    fn overlapping_stops_keep_the_gate_shut_until_the_last_one_settles() {
        for reversed in [false, true] {
            let id = fresh_id("overlap");
            let first = mark_stop_pending(&id);
            let second = mark_stop_pending(&id);
            assert!(is_stop_pending(&id));

            if reversed {
                drop(second);
                assert!(is_stop_pending(&id), "the first stop is still settling");
                drop(first);
            } else {
                drop(first);
                assert!(is_stop_pending(&id), "the second stop is still settling");
                drop(second);
            }
            assert!(!is_stop_pending(&id), "both settled ⇒ open again");
        }
    }

    #[test]
    fn a_stopping_agent_does_not_gate_its_siblings() {
        let stopping = fresh_id("stopping");
        let sibling = fresh_id("sibling");
        let _guard = mark_stop_pending(&stopping);
        assert!(is_stop_pending(&stopping));
        assert!(!is_stop_pending(&sibling));
    }

    /// The refusal reads as one sentence: upstream spells only the tail per
    /// tool, so a tail that forgets its period or repeats the subject shows up
    /// here rather than in six separate call sites.
    #[test]
    fn the_refusal_matches_the_oracle_sentence() {
        assert_eq!(
            stop_pending_refusal("launch new agents."),
            "This agent has been stopped and its stop is still completing; \
             it cannot launch new agents."
        );
        assert_eq!(
            stop_pending_refusal("launch workflows or act on existing runs."),
            "This agent has been stopped and its stop is still completing; \
             it cannot launch workflows or act on existing runs."
        );
    }
}
