//! Process-wide compliance taint state used by provider policy gates.
//!
//! This mirrors Native's initialized-empty current/latched taint sets. The
//! host starts clear, replaces the active set when a collector publishes a
//! snapshot, and keeps allowlisted compliance taints latched for the process.
//! No collector is currently wired in this runtime; this module provides the
//! real state/read path without deriving compliance from subscription,
//! traffic-mode, or feature-flag state.

use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};

const ALLOWLISTED_TAINTS: &[&str] = &["hipaa"];

static TAINTS: OnceLock<RwLock<TaintState>> = OnceLock::new();

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TaintState {
    current: BTreeSet<String>,
    latched: BTreeSet<String>,
}

impl TaintState {
    fn replace_current<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let current = supported_names(names);
        self.latched.extend(current.iter().cloned());
        self.current = current;
    }

    fn latch<I, S>(&mut self, names: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.latched.extend(supported_names(names));
    }

    fn contains(&self, name: &str) -> bool {
        self.current.contains(name) || self.latched.contains(name)
    }

    fn snapshot(&self) -> TaintSnapshot {
        TaintSnapshot {
            current: self.current.iter().cloned().collect(),
            latched: self.latched.iter().cloned().collect(),
            effective: self.current.union(&self.latched).cloned().collect(),
        }
    }
}

fn supported_names<I, S>(names: I) -> BTreeSet<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    names
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .filter(|name| ALLOWLISTED_TAINTS.contains(&name.as_str()))
        .collect()
}

/// Snapshot of active, latched, and effective Native-equivalent taints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaintSnapshot {
    /// Taints in the latest collector replacement.
    pub current: Vec<String>,
    /// Allowlisted taints that remain active for the process lifetime.
    pub latched: Vec<String>,
    /// Sorted union of current and latched taints.
    pub effective: Vec<String>,
}

fn state() -> &'static RwLock<TaintState> {
    TAINTS.get_or_init(|| RwLock::new(TaintState::default()))
}

/// Replace the current compliance-taint snapshot and latch every supported
/// compliance taint it contains. The current set starts empty and this
/// operation is intended for the host's policy collector when one is wired.
pub fn replace_current_taints<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    state()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .replace_current(names);
}

/// Add supported compliance taints to the process-latched set.
pub fn latch_taints<I, S>(names: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    state()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .latch(names);
}

/// Read whether a compliance taint is currently active or process-latched.
#[must_use]
pub fn is_tainted(name: &str) -> bool {
    state()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(name)
}

/// Read the Native-equivalent current/latched state for host policy consumers.
#[must_use]
pub fn snapshot() -> TaintSnapshot {
    state()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_store_starts_initialized_empty() {
        assert_eq!(snapshot(), TaintSnapshot::default());
        assert!(!is_tainted("hipaa"));
    }

    #[test]
    fn initialized_empty_replacement_and_latching_match_native_state() {
        let mut state = TaintState::default();
        assert_eq!(state.snapshot(), TaintSnapshot::default());
        assert!(!state.contains("hipaa"));

        state.replace_current(["hipaa", "not-allowlisted"]);
        assert_eq!(state.snapshot().current, vec!["hipaa".to_owned()]);
        assert_eq!(state.snapshot().latched, vec!["hipaa".to_owned()]);
        assert!(state.contains("hipaa"));

        state.replace_current(["not-allowlisted"]);
        assert!(state.snapshot().current.is_empty());
        assert_eq!(state.snapshot().latched, vec!["hipaa".to_owned()]);
        assert!(state.contains("hipaa"));

        state.latch(["unknown"]);
        assert_eq!(state.snapshot().effective, vec!["hipaa".to_owned()]);

        let mut explicitly_latched = TaintState::default();
        explicitly_latched.latch(["hipaa"]);
        assert!(explicitly_latched.snapshot().current.is_empty());
        assert_eq!(
            explicitly_latched.snapshot().latched,
            vec!["hipaa".to_owned()]
        );
    }
}
