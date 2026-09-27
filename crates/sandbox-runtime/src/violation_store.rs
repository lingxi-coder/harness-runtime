//! In-memory tail of sandbox violations (`SandboxViolationStore`,
//! `sandbox-violation-store.js`). A bounded ring buffer (`max_size = 100`) plus
//! a synchronous pub/sub: every mutation notifies subscribers with the full
//! current tail, and `subscribe` fires once immediately on registration.
//!
//! The TS `Violation` is a structural object the proxy/manager build; the only
//! field the store itself reads is `encodedCommand` (used by
//! `getViolationsForCommand`). We keep a faithful minimal shape: the
//! `encoded_command` discriminator plus the optional network/filesystem
//! descriptors a violation carries (host/port/path/operation), all `Option`
//! so callers populate only what applies.

use std::sync::Mutex;

use crate::env::encode_sandboxed_command;

/// Max retained violations (`maxSize = 100`).
const MAX_SIZE: usize = 100;

/// A single recorded sandbox violation. `encoded_command` is the base64 of the
/// (truncated) command that triggered it — the key `get_violations_for_command`
/// filters on. The remaining fields describe what was blocked; each is
/// `Option` because a network violation carries host/port while a filesystem
/// violation carries path/operation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Violation {
    /// base64 of the sandboxed command (`encodeSandboxedCommand`).
    pub encoded_command: String,
    /// The user-facing violation line surfaced by Claude Code.
    pub line: Option<String>,
    /// Destination host (network violations).
    pub host: Option<String>,
    /// Destination port (network violations).
    pub port: Option<u16>,
    /// Filesystem path (filesystem violations).
    pub path: Option<String>,
    /// Operation that was blocked (e.g. `"read"`, `"write"`, `"connect"`).
    pub operation: Option<String>,
}

/// A subscriber callback. Receives the full current tail on every notification.
type Listener = Box<dyn Fn(&[Violation]) + Send>;

/// Mutable interior of the store (behind one `Mutex`).
#[derive(Default)]
struct Inner {
    violations: Vec<Violation>,
    total_count: usize,
    /// Listeners keyed by a monotonic id so `unsubscribe` can remove the exact
    /// one (closures aren't comparable, so a `Set`-by-identity needs an id).
    listeners: Vec<(u64, Listener)>,
    next_listener_id: u64,
}

/// In-memory tail for sandbox violations (`SandboxViolationStore`).
#[derive(Default)]
pub struct SandboxViolationStore {
    inner: Mutex<Inner>,
}

/// Handle returned by [`SandboxViolationStore::subscribe`]. Calling [`Unsubscribe::call`]
/// (or dropping it after calling) removes the listener — mirrors the TS
/// `() => this.listeners.delete(listener)` returned closure.
#[must_use = "drop or call the unsubscribe handle to remove the listener"]
pub struct Unsubscribe<'a> {
    store: &'a SandboxViolationStore,
    id: u64,
}

impl Unsubscribe<'_> {
    /// Remove the associated listener. Idempotent.
    pub fn call(self) {
        let mut inner = self.store.lock();
        inner.listeners.retain(|(id, _)| *id != self.id);
    }
}

impl SandboxViolationStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `addViolation`: push, bump `total_count`, truncate to the last
    /// `MAX_SIZE`, then notify all listeners with the full tail.
    pub fn add_violation(&self, violation: Violation) {
        let snapshot = {
            let mut inner = self.lock();
            inner.violations.push(violation);
            inner.total_count += 1;
            if inner.violations.len() > MAX_SIZE {
                let start = inner.violations.len() - MAX_SIZE;
                inner.violations.drain(..start);
            }
            inner.violations.clone()
        };
        self.notify(&snapshot);
    }

    /// `getViolations(limit)`: the whole tail (`None`) or its last `limit`
    /// entries.
    #[must_use]
    pub fn get_violations(&self, limit: Option<usize>) -> Vec<Violation> {
        let inner = self.lock();
        match limit {
            None => inner.violations.clone(),
            Some(n) => {
                let start = inner.violations.len().saturating_sub(n);
                inner.violations[start..].to_vec()
            }
        }
    }

    /// `getCount`: number of currently-retained violations (≤ `MAX_SIZE`).
    #[must_use]
    pub fn get_count(&self) -> usize {
        self.lock().violations.len()
    }

    /// `getTotalCount`: lifetime count, never decremented (survives `clear`).
    #[must_use]
    pub fn get_total_count(&self) -> usize {
        self.lock().total_count
    }

    /// `getViolationsForCommand`: retained violations whose `encoded_command`
    /// equals `encode_sandboxed_command(command)`.
    #[must_use]
    pub fn get_violations_for_command(&self, command: &str) -> Vec<Violation> {
        let encoded = encode_sandboxed_command(command);
        let inner = self.lock();
        inner
            .violations
            .iter()
            .filter(|v| v.encoded_command == encoded)
            .cloned()
            .collect()
    }

    /// User-facing violation lines for `command`, in stored order.
    #[must_use]
    pub fn get_violation_lines_for_command(&self, command: &str) -> Vec<String> {
        self.get_violations_for_command(command)
            .into_iter()
            .filter_map(|violation| violation.line)
            .collect()
    }

    /// `clear`: empty the retained violations but KEEP `total_count`, then
    /// notify (with the now-empty tail).
    pub fn clear(&self) {
        {
            let mut inner = self.lock();
            inner.violations.clear();
            // Don't reset total_count when clearing.
        }
        self.notify(&[]);
    }

    /// `subscribe`: register `listener`, immediately invoke it with the current
    /// tail, and return an [`Unsubscribe`] handle. Subsequent mutations call it
    /// with the full tail until unsubscribed.
    pub fn subscribe<F>(&self, listener: F) -> Unsubscribe<'_>
    where
        F: Fn(&[Violation]) + Send + 'static,
    {
        let (id, snapshot) = {
            let mut inner = self.lock();
            let id = inner.next_listener_id;
            inner.next_listener_id += 1;
            inner.listeners.push((id, Box::new(listener)));
            (id, inner.violations.clone())
        };
        // Fire immediately with the current violations (TS calls the listener
        // synchronously inside subscribe).
        {
            let inner = self.lock();
            if let Some((_, l)) = inner.listeners.iter().find(|(lid, _)| *lid == id) {
                l(&snapshot);
            }
        }
        Unsubscribe { store: self, id }
    }

    /// `notifyListeners`: invoke every listener with the supplied tail.
    fn notify(&self, violations: &[Violation]) {
        let inner = self.lock();
        for (_, l) in &inner.listeners {
            l(violations);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn v(cmd: &str) -> Violation {
        Violation {
            encoded_command: encode_sandboxed_command(cmd),
            ..Default::default()
        }
    }

    #[test]
    fn ring_buffer_retains_last_100_total_grows() {
        let store = SandboxViolationStore::new();
        for i in 0..150 {
            store.add_violation(v(&format!("cmd-{i}")));
        }
        assert_eq!(store.get_count(), 100);
        assert_eq!(store.get_total_count(), 150);
        // The retained tail is the LAST 100 (cmd-50..cmd-149).
        let all = store.get_violations(None);
        assert_eq!(
            all.first().unwrap().encoded_command,
            encode_sandboxed_command("cmd-50")
        );
        assert_eq!(
            all.last().unwrap().encoded_command,
            encode_sandboxed_command("cmd-149")
        );
        // limit returns the last N.
        let last3 = store.get_violations(Some(3));
        assert_eq!(last3.len(), 3);
        assert_eq!(
            last3[2].encoded_command,
            encode_sandboxed_command("cmd-149")
        );
    }

    #[test]
    fn filter_by_command_uses_encoded_command() {
        let store = SandboxViolationStore::new();
        store.add_violation(v("git status"));
        store.add_violation(v("npm install"));
        store.add_violation(v("git status"));
        let git = store.get_violations_for_command("git status");
        assert_eq!(git.len(), 2);
        assert!(git
            .iter()
            .all(|x| x.encoded_command == encode_sandboxed_command("git status")));
        assert_eq!(store.get_violations_for_command("nothing here").len(), 0);
    }

    #[test]
    fn violation_lines_for_command_keep_only_matching_lines() {
        let store = SandboxViolationStore::new();
        store.add_violation(Violation {
            encoded_command: encode_sandboxed_command("git status"),
            line: Some(
                "deny network-outbound github.com:443 (host is not on the allow list)".into(),
            ),
            ..Default::default()
        });
        store.add_violation(Violation {
            encoded_command: encode_sandboxed_command("npm install"),
            line: Some(
                "deny network-outbound registry.npmjs.org:443 (host is not on the allow list)"
                    .into(),
            ),
            ..Default::default()
        });
        assert_eq!(
            store.get_violation_lines_for_command("git status"),
            vec![
                "deny network-outbound github.com:443 (host is not on the allow list)".to_string()
            ]
        );
    }

    #[test]
    fn clear_empties_but_keeps_total() {
        let store = SandboxViolationStore::new();
        store.add_violation(v("a"));
        store.add_violation(v("b"));
        assert_eq!(store.get_count(), 2);
        store.clear();
        assert_eq!(store.get_count(), 0);
        assert_eq!(store.get_total_count(), 2);
        assert!(store.get_violations(None).is_empty());
    }

    #[test]
    fn subscribe_fires_immediately_on_add_and_unsubscribe_stops() {
        let store = SandboxViolationStore::new();
        store.add_violation(v("pre-existing"));

        let calls = Arc::new(AtomicUsize::new(0));
        let last_len = Arc::new(AtomicUsize::new(usize::MAX));
        let c = Arc::clone(&calls);
        let ll = Arc::clone(&last_len);
        let unsub = store.subscribe(move |vs| {
            c.fetch_add(1, Ordering::SeqCst);
            ll.store(vs.len(), Ordering::SeqCst);
        });
        // Fired immediately with the current tail (1 pre-existing).
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(last_len.load(Ordering::SeqCst), 1);

        // Fires on add.
        store.add_violation(v("more"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(last_len.load(Ordering::SeqCst), 2);

        // Fires on clear too (with empty tail).
        store.clear();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(last_len.load(Ordering::SeqCst), 0);

        // Unsubscribe stops further notifications.
        unsub.call();
        store.add_violation(v("after"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
