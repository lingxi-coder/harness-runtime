//! In-memory cache of recently-read file contents (spec §23.1 / B8).
//!
//! Edits use this cache to detect external modifications between a Read and
//! a subsequent Edit (Read↔Edit verification, see [`super::verify`]).
//!
//! The cache is bounded by both entry count and total byte count; both
//! counters live under the same mutex so eviction stays consistent with the
//! byte total (B8 — byte counter under same lock).

#![allow(clippy::unwrap_used)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

/// Snapshot of a file the engine has Read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileState {
    /// File body at read time.
    pub content: String,
    /// Wall-clock time at which the read occurred.
    pub timestamp: SystemTime,
    /// Optional byte offset of the window relative to the full file.
    pub offset: Option<u64>,
    /// Optional byte limit of the window.
    pub limit: Option<u64>,
    /// True when `content` is a partial view (offset/limit set) — Edits then
    /// require an exact match against the partial view, not the whole file.
    pub is_partial_view: bool,
}

/// Maximum number of cached file entries.
pub const MAX_ENTRIES: usize = 100;

/// Maximum total bytes across all cached file contents (16 MiB).
pub const MAX_BYTES: u64 = 16 * 1024 * 1024;

struct CacheInner {
    map: HashMap<PathBuf, FileState>,
    order: Vec<PathBuf>,
    byte_count: u64,
}

/// LRU-ish cache of file states with combined entry + byte caps.
pub struct FileStateCache {
    inner: Mutex<CacheInner>,
    max_entries: usize,
    max_bytes: u64,
}

impl FileStateCache {
    /// Construct an empty cache with the supplied caps.
    #[must_use]
    pub fn new(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            inner: Mutex::new(CacheInner {
                map: HashMap::new(),
                order: Vec::new(),
                byte_count: 0,
            }),
            max_entries,
            max_bytes,
        }
    }

    /// Look up a cached file state by path. Returns `None` if not cached.
    ///
    /// A hit PROMOTES the entry to most-recently-used (moves it to the tail of
    /// `order`), mirroring `lru-cache`'s `moveToTail` on get (default
    /// `updateRecencyOnGet`), confirmed in the v2.1.193 binary. Without this, a
    /// file Read once then re-Read many times could be evicted under cache
    /// pressure while claude-code retains it — changing which Edit gets a
    /// NotInCache/ModifiedSinceRead outcome.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<FileState> {
        let mut inner = self.inner.lock().unwrap();
        let p = normalize(path);
        let state = inner.map.get(&p).cloned();
        if state.is_some() {
            // Promote to MRU (tail).
            inner.order.retain(|k| k != &p);
            inner.order.push(p);
        }
        state
    }

    /// Insert or update a cached file state, evicting older entries as needed
    /// to stay under both caps.
    pub fn set(&self, path: &str, state: FileState) {
        let mut inner = self.inner.lock().unwrap();
        let p = normalize(path);
        let new_size = u64::try_from(state.content.len()).unwrap_or(u64::MAX);
        if let Some(old) = inner.map.remove(&p) {
            let old_size = u64::try_from(old.content.len()).unwrap_or(u64::MAX);
            inner.byte_count = inner.byte_count.saturating_sub(old_size);
            inner.order.retain(|k| k != &p);
        }
        inner.map.insert(p.clone(), state);
        inner.order.push(p);
        inner.byte_count = inner.byte_count.saturating_add(new_size);
        // Evict until under both limits.
        while inner.map.len() > self.max_entries || inner.byte_count > self.max_bytes {
            if let Some(victim) = inner.order.first().cloned() {
                if let Some(old) = inner.map.remove(&victim) {
                    let old_size = u64::try_from(old.content.len()).unwrap_or(u64::MAX);
                    inner.byte_count = inner.byte_count.saturating_sub(old_size);
                }
                inner.order.remove(0);
            } else {
                break;
            }
        }
    }

    /// Remove a cached entry. Returns `true` iff something was removed.
    pub fn delete(&self, path: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let p = normalize(path);
        if let Some(s) = inner.map.remove(&p) {
            let old_size = u64::try_from(s.content.len()).unwrap_or(u64::MAX);
            inner.byte_count = inner.byte_count.saturating_sub(old_size);
            inner.order.retain(|k| k != &p);
            true
        } else {
            false
        }
    }

    /// Take a snapshot of all (path, state) pairs currently cached.
    #[must_use]
    pub fn dump(&self) -> Vec<(PathBuf, FileState)> {
        self.inner
            .lock()
            .unwrap()
            .map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Bulk-insert entries (used to restore from a snapshot or to merge).
    pub fn load(&self, entries: Vec<(PathBuf, FileState)>) {
        for (k, v) in entries {
            self.set(k.to_str().unwrap_or(""), v);
        }
    }

    /// Clone the cache by snapshotting and replaying entries — used by
    /// [`super::merge::merge_caches`] and by `ForkedAgent`'s copy-on-fork path.
    #[must_use]
    pub fn clone_cache(&self) -> Self {
        let out = Self::new(self.max_entries, self.max_bytes);
        let entries: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .map
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.load(entries);
        out
    }
}

impl Default for FileStateCache {
    fn default() -> Self {
        Self::new(MAX_ENTRIES, MAX_BYTES)
    }
}

fn normalize(path: &str) -> PathBuf {
    use std::path::Component;
    let p = PathBuf::from(path);
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        return PathBuf::new();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_when_over_byte_limit() {
        let c = FileStateCache::new(100, 100);
        c.set(
            "a.txt",
            FileState {
                content: "x".repeat(80),
                timestamp: SystemTime::now(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        c.set(
            "b.txt",
            FileState {
                content: "y".repeat(80),
                timestamp: SystemTime::now(),
                offset: None,
                limit: None,
                is_partial_view: false,
            },
        );
        // a was evicted to keep under 100 bytes
        assert!(c.get("b.txt").is_some());
    }
}
