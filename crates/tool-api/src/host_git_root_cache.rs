//! Host-scoped cache for `.git` root-marker discovery.
//!
//! The cache belongs to the host instance, not to an individual session. Host
//! composition roots put one `Arc<HostGitRootCache>` on their config and clone
//! it into each session's [`crate::BuiltinToolContext`]. Keys are the exact cwd
//! paths supplied by the caller; normalization is deliberately left to the
//! source-specific discovery function.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Mutex;

const MAX_ENTRIES: usize = 50;

#[derive(Default)]
struct CacheState {
    values: HashMap<OsString, bool>,
    lru: VecDeque<OsString>,
}

/// A bounded, no-TTL cache of whether a cwd has a native Git root marker.
///
/// Both positive and negative discoveries are cached. A cache hit refreshes
/// the entry's LRU position and does not execute the discovery closure.
#[derive(Default)]
pub struct HostGitRootCache {
    state: Mutex<CacheState>,
}

impl HostGitRootCache {
    /// Construct an empty cache for one host lifetime.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached result for the exact cwd, or compute and cache it.
    ///
    /// The closure runs once while the cache lock is held on a miss. Holding
    /// the lock prevents concurrent misses for the same host from repeating
    /// filesystem discovery and preserves the source cache's single-operation
    /// behavior.
    pub fn get_or_insert_with<F>(&self, key: &Path, discover: F) -> bool
    where
        F: FnOnce(&Path) -> bool,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key_units = key.as_os_str();
        if let Some(value) = state.values.get(key_units).copied() {
            touch(&mut state.lru, key_units);
            return value;
        }

        let value = discover(key);
        if state.values.len() == MAX_ENTRIES {
            if let Some(evicted) = state.lru.pop_front() {
                state.values.remove(&evicted);
            }
        }
        let owned_key = key_units.to_os_string();
        state.values.insert(owned_key.clone(), value);
        state.lru.push_back(owned_key);
        value
    }

    /// Remove a cached negative result, preserving a positive result.
    ///
    /// This mirrors the source's retry path that explicitly refreshes a
    /// negative root-marker lookup.
    pub fn invalidate_negative(&self, key: &Path) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key_units = key.as_os_str();
        if state.values.get(key_units) != Some(&false) {
            return false;
        }
        state.values.remove(key_units);
        remove_lru(&mut state.lru, key_units);
        true
    }

    /// Remove any cached result for one exact cwd.
    pub fn invalidate(&self, key: &Path) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key_units = key.as_os_str();
        let removed = state.values.remove(key_units).is_some();
        if removed {
            remove_lru(&mut state.lru, key_units);
        }
        removed
    }

    /// Remove entries at `root` and beneath it using component-aware paths.
    ///
    /// This is the root-path invalidation used when a host filesystem subtree
    /// changes. It does not treat textual prefixes such as `/repo-old` as
    /// descendants of `/repo`.
    pub fn invalidate_at_or_below(&self, root: &Path) -> usize {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed_keys = state
            .values
            .keys()
            .filter(|key| Path::new(key.as_os_str()).starts_with(root))
            .cloned()
            .collect::<Vec<_>>();
        for key in &removed_keys {
            state.values.remove(key);
            remove_lru(&mut state.lru, key);
        }
        removed_keys.len()
    }
}

fn touch(lru: &mut VecDeque<OsString>, key: &OsStr) {
    remove_lru(lru, key);
    lru.push_back(key.to_os_string());
}

fn remove_lru(lru: &mut VecDeque<OsString>, key: &OsStr) {
    if let Some(index) = lru.iter().position(|entry| entry.as_os_str() == key) {
        lru.remove(index);
    }
}

#[cfg(test)]
mod tests {
    use super::HostGitRootCache;
    use std::path::Path;

    #[test]
    fn caches_positive_and_negative_results_and_refreshes_lru_on_hits() {
        let cache = HostGitRootCache::new();
        let positive = Path::new("/host/project");
        let negative = Path::new("/host/other");
        let mut discoveries = 0;

        assert!(cache.get_or_insert_with(positive, |_| {
            discoveries += 1;
            true
        }));
        assert!(!cache.get_or_insert_with(negative, |_| {
            discoveries += 1;
            false
        }));
        assert!(cache.get_or_insert_with(positive, |_| {
            discoveries += 1;
            false
        }));
        assert!(!cache.get_or_insert_with(negative, |_| {
            discoveries += 1;
            true
        }));
        assert_eq!(discoveries, 2);
    }

    #[test]
    fn negative_refresh_keeps_positive_entries_and_recomputes_only_the_negative() {
        let cache = HostGitRootCache::new();
        let negative = Path::new("/host/negative");
        let positive = Path::new("/host/positive");
        assert!(!cache.get_or_insert_with(negative, |_| false));
        assert!(cache.get_or_insert_with(positive, |_| true));

        assert!(cache.invalidate_negative(negative));
        assert!(!cache.invalidate_negative(positive));
        assert!(cache.get_or_insert_with(negative, |_| true));
        assert!(cache.get_or_insert_with(positive, |_| false));
    }

    #[test]
    fn exact_and_subtree_invalidation_are_component_aware() {
        let cache = HostGitRootCache::new();
        for path in ["/repo", "/repo/child", "/repo-old", "/repo2/nested"] {
            let _ = cache.get_or_insert_with(Path::new(path), |_| true);
        }

        assert_eq!(cache.invalidate_at_or_below(Path::new("/repo")), 2);
        assert!(cache.get_or_insert_with(Path::new("/repo-old"), |_| false));
        assert!(cache.invalidate(Path::new("/repo-old")));
        assert!(!cache.invalidate(Path::new("/missing")));
        assert!(cache.get_or_insert_with(Path::new("/repo2/nested"), |_| false));
    }

    #[test]
    fn cwd_keys_preserve_original_separator_and_dot_component_spelling() {
        let cache = HostGitRootCache::new();
        let normalized = Path::new("/host/project");
        let repeated_separator = Path::new("/host//project");
        let dot_component = Path::new("/host/./project");
        let mut discoveries = 0;

        for (path, expected) in [
            (normalized, true),
            (repeated_separator, false),
            (dot_component, true),
        ] {
            assert_eq!(
                cache.get_or_insert_with(path, |_| {
                    discoveries += 1;
                    discoveries != 2
                }),
                expected
            );
        }

        assert!(cache.get_or_insert_with(normalized, |_| panic!("must hit exact key")));
        assert!(!cache.get_or_insert_with(repeated_separator, |_| { panic!("must hit exact key") }));
        assert!(cache.get_or_insert_with(dot_component, |_| panic!("must hit exact key")));
        assert_eq!(discoveries, 3);
    }

    #[test]
    fn cloned_host_arc_shares_positive_and_negative_entries() {
        let host_cache = std::sync::Arc::new(HostGitRootCache::new());
        let session_cache = std::sync::Arc::clone(&host_cache);
        let cwd = Path::new("/host/shared-session-root");
        let mut discoveries = 0;

        assert!(!host_cache.get_or_insert_with(cwd, |_| {
            discoveries += 1;
            false
        }));
        assert!(!session_cache.get_or_insert_with(cwd, |_| {
            discoveries += 1;
            true
        }));
        assert_eq!(discoveries, 1);
    }

    #[test]
    fn evicts_the_least_recently_used_entry_after_fifty_distinct_cwds() {
        let cache = HostGitRootCache::new();
        let keys = (0..51)
            .map(|index| format!("/host/project-{index}"))
            .collect::<Vec<_>>();
        let mut discoveries = 0;

        for key in &keys[..50] {
            assert!(cache.get_or_insert_with(Path::new(key.as_str()), |_| {
                discoveries += 1;
                true
            }));
        }
        // Touch the original oldest entry, making project-1 the next eviction.
        assert!(cache.get_or_insert_with(Path::new(keys[0].as_str()), |_| false));
        assert!(cache.get_or_insert_with(Path::new(keys[50].as_str()), |_| {
            discoveries += 1;
            true
        }));
        assert!(cache.get_or_insert_with(Path::new(keys[0].as_str()), |_| {
            discoveries += 1;
            false
        }));
        assert!(cache.get_or_insert_with(Path::new(keys[1].as_str()), |_| {
            discoveries += 1;
            true
        }));
        assert_eq!(discoveries, 52);
    }
}
