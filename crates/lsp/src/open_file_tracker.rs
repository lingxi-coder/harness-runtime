//! `OpenFileTracker` — server-aware dedup of `textDocument/didOpen`.
//!
//! Many LSP servers return empty results from requests like
//! `textDocument/hover` unless the document has been opened via
//! `textDocument/didOpen` first. claude-code's `LSPServerManager`
//! (`claude-code/src/services/lsp/LSPServerManager.ts:64,277`) tracks open
//! files in a `Map<string, string>` keyed by URI. We key the *tuple*
//! `(server_name, uri)` so the same URI can be tracked independently on
//! multiple servers (multi-LSP setups).

use lsp_types::Url;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

use jsonrpc::Connection;

/// Maximum number of open documents retained per registry, matching Claude Code
/// 2.1.208's didOpen LRU cap.
pub const MAX_OPEN_DOCUMENTS: usize = 50;

#[derive(Default)]
struct OpenFileTrackerInner {
    open: HashSet<(String, Url)>,
    order: VecDeque<(String, Url)>,
    versions: HashMap<Url, i32>,
    contents: HashMap<(String, Url), String>,
    connections: HashMap<String, Arc<Connection>>,
    blocked_servers: HashSet<String>,
}

/// Notification required to synchronize the current on-disk text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocumentSync {
    /// First open (or reopen after LRU eviction).
    Open {
        /// Monotonic document version.
        version: i32,
        /// Documents displaced by the global 50-document LRU.
        evicted: Vec<(String, Url)>,
    },
    /// Already open, but the on-disk text changed since the last request.
    Change {
        /// Monotonic document version.
        version: i32,
    },
    /// Already open with byte-identical UTF-8 text.
    Unchanged,
}

/// Thread-safe `(server_name, uri)` open-file set.
#[derive(Default, Clone)]
pub struct OpenFileTracker {
    inner: Arc<RwLock<OpenFileTrackerInner>>,
    sync_gate: Arc<Mutex<()>>,
}

impl std::fmt::Debug for OpenFileTracker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenFileTracker")
            .finish_non_exhaustive()
    }
}

impl OpenFileTracker {
    /// Empty tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `true` when `uri` has been opened on `server_name`.
    pub async fn is_open(&self, server_name: &str, uri: &Url) -> bool {
        let key = (server_name.to_string(), uri.clone());
        self.inner.read().await.open.contains(&key)
    }

    /// Serialize the state transition and its corresponding wire
    /// notifications. Without this gate, a concurrent request can observe an
    /// `Open` transition before `didOpen` has actually been queued.
    pub(crate) async fn lock_sync(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.sync_gate).lock_owned().await
    }

    /// Remember the live connection for cross-server LRU `didClose` routing.
    pub(crate) async fn register_connection(&self, server_name: &str, connection: Arc<Connection>) {
        let mut guard = self.inner.write().await;
        if guard.blocked_servers.contains(server_name) {
            return;
        }
        guard
            .connections
            .entry(server_name.to_string())
            .or_insert(connection);
    }

    /// Mark `connection` as the current live owner for `server_name`.
    pub(crate) async fn activate_server(&self, server_name: &str, connection: Arc<Connection>) {
        let mut guard = self.inner.write().await;
        guard.blocked_servers.remove(server_name);
        guard
            .connections
            .insert(server_name.to_string(), connection);
    }

    /// Resolve the connection that owns an evicted `(server, URI)` entry.
    pub(crate) async fn connection_for_server(&self, server_name: &str) -> Option<Arc<Connection>> {
        self.inner
            .read()
            .await
            .connections
            .get(server_name)
            .cloned()
    }

    /// Whether `connection` is still the live owner for `server_name`.
    pub(crate) async fn is_active_connection(
        &self,
        server_name: &str,
        connection: &Arc<Connection>,
    ) -> bool {
        self.inner
            .read()
            .await
            .connections
            .get(server_name)
            .is_some_and(|current| Arc::ptr_eq(current, connection))
    }

    /// Atomically compare and record a document snapshot, returning the exact
    /// LSP sync notification the caller must send. Versions are keyed by URI
    /// and intentionally survive `didClose`, matching Claude Code's manager.
    pub async fn plan_sync(&self, server_name: &str, uri: Url, text: &str) -> DocumentSync {
        let key = (server_name.to_string(), uri.clone());
        let mut guard = self.inner.write().await;

        if guard.open.contains(&key) {
            guard.order.retain(|existing| existing != &key);
            guard.order.push_back(key.clone());
            match guard.contents.get(&key) {
                Some(old) if old == text => return DocumentSync::Unchanged,
                // Compatibility for callers/tests that pre-marked a URI
                // before snapshot tracking existed.
                None => {
                    guard.contents.insert(key, text.to_string());
                    return DocumentSync::Unchanged;
                }
                Some(_) => {}
            }
            let version = next_version(&mut guard.versions, &uri);
            guard.contents.insert(key, text.to_string());
            return DocumentSync::Change { version };
        }

        let version = next_version(&mut guard.versions, &uri);
        guard.open.insert(key.clone());
        guard.contents.insert(key.clone(), text.to_string());
        guard.order.push_back(key);
        let mut evicted = Vec::new();
        while guard.open.len() > MAX_OPEN_DOCUMENTS {
            let Some(oldest) = guard.order.pop_front() else {
                break;
            };
            if guard.open.remove(&oldest) {
                guard.contents.remove(&oldest);
                evicted.push(oldest);
            }
        }
        DocumentSync::Open { version, evicted }
    }

    /// Mark `uri` as open on `server_name`. Idempotent.
    ///
    /// Returns entries evicted by the 50-document LRU cap; callers should send
    /// `textDocument/didClose` for each returned URI.
    pub async fn mark_open(&self, server_name: &str, uri: Url) -> Vec<(String, Url)> {
        let key = (server_name.to_string(), uri);
        let mut guard = self.inner.write().await;
        if guard.open.contains(&key) {
            guard.order.retain(|existing| existing != &key);
            guard.order.push_back(key);
            return Vec::new();
        }

        guard.open.insert(key.clone());
        guard.order.push_back(key);
        let mut evicted = Vec::new();
        while guard.open.len() > MAX_OPEN_DOCUMENTS {
            let Some(oldest) = guard.order.pop_front() else {
                break;
            };
            if guard.open.remove(&oldest) {
                guard.contents.remove(&oldest);
                evicted.push(oldest);
            }
        }
        evicted
    }

    /// Remove the open-file tracking for one specific `(server, uri)` pair.
    pub async fn clear(&self, server_name: &str, uri: &Url) {
        let key = (server_name.to_string(), uri.clone());
        let mut guard = self.inner.write().await;
        guard.open.remove(&key);
        guard.contents.remove(&key);
        guard.order.retain(|existing| existing != &key);
    }

    /// Remove every open-file entry for `server_name` (e.g. when the
    /// server is shutting down or restarting). Returns the number of
    /// entries dropped.
    pub async fn clear_server(&self, server_name: &str) -> usize {
        // Clearing a crashed server must be ordered after any document sync
        // already using its old connection. Otherwise that sync can recreate
        // an `Open` snapshot after this method returns, causing the replacement
        // server to skip its first `didOpen`.
        let _sync_guard = self.lock_sync().await;
        let mut guard = self.inner.write().await;
        let before = guard.open.len();
        guard.open.retain(|(s, _)| s != server_name);
        guard.order.retain(|(s, _)| s != server_name);
        guard.contents.retain(|(s, _), _| s != server_name);
        guard.connections.remove(server_name);
        guard.blocked_servers.insert(server_name.to_string());
        before - guard.open.len()
    }

    /// Total entries (across all servers); for diagnostics / tests.
    pub async fn len(&self) -> usize {
        self.inner.read().await.open.len()
    }

    /// Whether the tracker is empty.
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.open.is_empty()
    }

    /// Latest synced document version for `uri`, if one was ever assigned.
    pub async fn current_version(&self, uri: &Url) -> Option<i32> {
        self.inner.read().await.versions.get(uri).copied()
    }
}

fn next_version(versions: &mut HashMap<Url, i32>, uri: &Url) -> i32 {
    let version = versions.get(uri).copied().unwrap_or(0) + 1;
    versions.insert(uri.clone(), version);
    version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clear_server_waits_for_inflight_sync_and_removes_its_snapshot() {
        let tracker = OpenFileTracker::new();
        let uri = Url::parse("file:///workspace/src/lib.rs").expect("valid URI");
        let sync_guard = tracker.lock_sync().await;
        let clearing_tracker = tracker.clone();
        let clear_task =
            tokio::spawn(async move { clearing_tracker.clear_server("rust-analyzer").await });

        tokio::task::yield_now().await;
        assert!(
            !clear_task.is_finished(),
            "server clearing must wait behind the document sync gate"
        );
        assert!(matches!(
            tracker
                .plan_sync("rust-analyzer", uri.clone(), "fn old_connection() {}")
                .await,
            DocumentSync::Open { .. }
        ));
        drop(sync_guard);

        assert_eq!(clear_task.await.expect("clear task"), 1);
        assert!(matches!(
            tracker
                .plan_sync("rust-analyzer", uri, "fn old_connection() {}")
                .await,
            DocumentSync::Open { .. }
        ));
    }

    #[tokio::test]
    async fn clear_server_blocks_stale_connection_re_registration_until_reactivated() {
        let tracker = OpenFileTracker::new();
        let (first_io, _first_peer) = tokio::io::duplex(64);
        let (first_read, first_write) = tokio::io::split(first_io);
        let first = Arc::new(Connection::new_line_delimited(first_read, first_write));
        let (next_io, _next_peer) = tokio::io::duplex(64);
        let (next_read, next_write) = tokio::io::split(next_io);
        let next = Arc::new(Connection::new_line_delimited(next_read, next_write));

        tracker
            .register_connection("rust-analyzer", Arc::clone(&first))
            .await;
        assert!(tracker.is_active_connection("rust-analyzer", &first).await);

        tracker.clear_server("rust-analyzer").await;
        tracker
            .register_connection("rust-analyzer", Arc::clone(&first))
            .await;
        assert!(
            tracker
                .connection_for_server("rust-analyzer")
                .await
                .is_none(),
            "a stale request must not republish the cleared connection"
        );

        tracker
            .activate_server("rust-analyzer", Arc::clone(&next))
            .await;
        assert!(tracker.is_active_connection("rust-analyzer", &next).await);
    }
}
