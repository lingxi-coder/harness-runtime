//! Bounded ring buffer of sandbox violation events. Matches claude-code's
//! `SandboxViolationStore` from `@anthropic-ai/sandbox-runtime`.
//!
//! Events are produced by the platform-side sandbox backend when a wrapped
//! command tries to read/write outside policy, hit a denied network domain,
//! etc. The UI consumer (`/sandbox doctor`, telemetry) drains via
//! `snapshot()`. Once `SANDBOX_VIOLATION_STORE_CAP` events are stored, new
//! events evict the oldest.

use std::collections::VecDeque;
use tokio::sync::RwLock;

/// Maximum events retained. Matches claude-code's circular buffer cap.
pub const SANDBOX_VIOLATION_STORE_CAP: usize = 1000;

/// Discriminator for `SandboxViolationEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SandboxViolationKind {
    /// Sandboxed process attempted to read a file outside its read allow-list.
    FileRead,
    /// Sandboxed process attempted to write a file outside its write allow-list.
    FileWrite,
    /// Sandboxed process attempted to reach a denied network domain.
    NetworkDomain,
    /// Sandboxed process attempted a denied unix socket or other socket op.
    NetworkSocket,
    /// Anything else surfaced by the backend.
    Other,
}

/// One sandbox violation. Fields mirror claude-code's `SandboxViolationEvent`
/// shape closely enough that the `/sandbox doctor` renderer can be ported
/// without further translation.
#[derive(Debug, Clone)]
pub struct SandboxViolationEvent {
    /// `Date.now()`-equivalent epoch millis.
    pub timestamp_ms: u64,
    /// Command line that triggered the violation.
    pub command: String,
    /// Categorical kind.
    pub violation_type: SandboxViolationKind,
    /// Human-readable detail.
    pub message: String,
}

/// Bounded ring-buffer store of [`SandboxViolationEvent`].
///
/// Clone-on-snapshot rather than expose the internal `VecDeque`. Callers
/// hold an `Arc<SandboxViolationStore>` to share between async tasks.
#[derive(Debug, Default)]
pub struct SandboxViolationStore {
    inner: RwLock<VecDeque<SandboxViolationEvent>>,
}

impl SandboxViolationStore {
    /// Construct an empty store with capacity [`SANDBOX_VIOLATION_STORE_CAP`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(VecDeque::with_capacity(SANDBOX_VIOLATION_STORE_CAP)),
        }
    }

    /// Push `event`, evicting the oldest if the store is full.
    pub async fn record(&self, event: SandboxViolationEvent) {
        let mut guard = self.inner.write().await;
        if guard.len() == SANDBOX_VIOLATION_STORE_CAP {
            guard.pop_front();
        }
        guard.push_back(event);
    }

    /// Return a cloned snapshot, in insertion order (oldest first).
    pub async fn snapshot(&self) -> Vec<SandboxViolationEvent> {
        self.inner.read().await.iter().cloned().collect()
    }

    /// Drop all events.
    pub async fn clear(&self) {
        self.inner.write().await.clear();
    }

    /// Current count.
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// `true` iff the store has no events.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}
