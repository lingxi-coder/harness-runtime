//! Process-global URL content cache for `WebFetchTool`.
//!
//! Mirrors `claude-code/src/tools/WebFetchTool/utils.ts:50-83,392-404,505-518`:
//! successful fetches are cached for 15 minutes, keyed by the *original*
//! (pre-upgrade, pre-redirect) URL, so repeat fetches return instantly without a
//! second network round-trip. The TS side uses `lru-cache` with a 15-minute TTL
//! and a 50 MB byte-size budget; this is a hand-rolled, behaviorally-equivalent
//! TTL + size-bounded LRU (no new external crate). The eviction *policy* (true
//! LRU + 50 MB budget) is equivalent in behavior but not a literal port of
//! `lru-cache`'s internals.
//!
//! Wire-locked constants (byte-checked against `utils.ts:63-64`):
//! - `CACHE_TTL_MS = 15 * 60 * 1000` (15 minutes)
//! - `MAX_CACHE_SIZE_BYTES = 50 * 1024 * 1024` (50 MB)

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cache TTL: 15 minutes. Mirrors `utils.ts:63` `CACHE_TTL_MS`.
pub const CACHE_TTL: Duration = Duration::from_secs(15 * 60);

/// Maximum total cached *content* bytes: 50 MB. Mirrors `utils.ts:64`
/// `MAX_CACHE_SIZE_BYTES`.
pub const MAX_CACHE_SIZE_BYTES: usize = 50 * 1024 * 1024;

/// Hard cap on the number of distinct entries. The TS `lru-cache` is bounded
/// purely by `maxSize` (bytes); we add a small entry cap as a defensive bound so
/// a flood of tiny responses can't grow the map unboundedly. Behaviorally
/// equivalent for the realistic case (entries are content-sized).
pub const MAX_CACHE_ENTRIES: usize = 64;

/// A cached successful fetch.
///
/// Mirrors the `CacheEntry` shape in `utils.ts:51-59` (Rust-side field names).
/// `content` holds whatever the fetch pipeline produced (markdown conversion +
/// Haiku land in later batches; the cache stores the produced `content` as-is).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedFetch {
    /// Tool-visible content body. PARITY (#88): this is the FULL converted body —
    /// claude-code caches `content:p` un-sliced; the 100k cap only fires inside
    /// the apply step (`applyPromptToMarkdown`), not before caching.
    pub content: String,
    /// HTTP status code of the fetch.
    pub status: u16,
    /// `Content-Type` header value (empty string if absent).
    pub content_type: String,
    /// Raw response byte length (the `bytes` field echoed in tool output).
    pub bytes: usize,
    /// Path to a persisted binary artifact, if the body was binary (`persistedPath`
    /// in claude-code's cache entry). `None` for non-binary bodies.
    pub persisted_path: Option<String>,
    /// Byte size of the persisted binary artifact (`persistedSize`). `None` when
    /// nothing was persisted. The binary footer prefers this over `bytes`
    /// (`Ma(persistedSize ?? bytes)`).
    pub persisted_size: Option<usize>,
    /// Unix epoch milliseconds when the network body was fetched. Retained in
    /// the cache so Fusion cache-hit results can disclose the original fetch
    /// provenance instead of stamping a new hit time.
    pub fetched_at_ms: u64,
}

impl CachedFetch {
    /// Byte cost charged against [`MAX_CACHE_SIZE_BYTES`].
    ///
    /// `lru-cache` requires a positive integer size, clamping to 1 for empty
    /// responses (`utils.ts:516-517`). We mirror that with `max(1, content.len())`.
    fn size_cost(&self) -> usize {
        self.content.len().max(1)
    }
}

/// One slot in the LRU: the value plus bookkeeping for TTL and recency.
struct Slot {
    entry: CachedFetch,
    /// When this slot was inserted (for TTL expiry).
    inserted_at: Instant,
    /// Monotonic access tick; the lowest tick is the least-recently-used.
    last_used: u64,
}

/// TTL + size-bounded LRU map. Not a literal port of `lru-cache`; behaviorally
/// equivalent for the WebFetch use case.
struct UrlCache {
    map: HashMap<String, Slot>,
    /// Running sum of `size_cost()` across live entries.
    total_bytes: usize,
    /// Monotonic counter used to stamp `last_used` on insert/get.
    tick: u64,
}

impl UrlCache {
    fn new() -> Self {
        Self {
            map: HashMap::new(),
            total_bytes: 0,
            tick: 0,
        }
    }

    fn next_tick(&mut self) -> u64 {
        self.tick = self.tick.wrapping_add(1);
        self.tick
    }

    /// Drop an entry, decrementing the byte accounting.
    fn remove(&mut self, key: &str) {
        if let Some(slot) = self.map.remove(key) {
            self.total_bytes = self.total_bytes.saturating_sub(slot.entry.size_cost());
        }
    }

    /// Get a still-live (non-expired) entry, refreshing its recency.
    ///
    /// Expired entries are evicted on access, mirroring `lru-cache`'s lazy TTL
    /// (`utils.ts:392-393`: "LRUCache handles TTL automatically").
    fn get(&mut self, key: &str, now: Instant) -> Option<CachedFetch> {
        // Expire-on-read: if present but stale, evict and miss.
        let expired = self
            .map
            .get(key)
            .is_some_and(|slot| now.duration_since(slot.inserted_at) >= CACHE_TTL);
        if expired {
            self.remove(key);
            return None;
        }
        let tick = self.next_tick();
        let slot = self.map.get_mut(key)?;
        slot.last_used = tick;
        Some(slot.entry.clone())
    }

    /// Insert/replace an entry, evicting LRU victims until both the byte budget
    /// and the entry cap are satisfied.
    fn insert(&mut self, key: String, entry: CachedFetch, now: Instant) {
        // Replacing an existing key: drop the old accounting first.
        self.remove(&key);

        let cost = entry.size_cost();
        // A single entry larger than the whole budget is still stored (matching
        // lru-cache, which stores then immediately may evict); but we cap its
        // accounting so eviction can make room. Evict LRU until it fits.
        let tick = self.next_tick();
        self.map.insert(
            key,
            Slot {
                entry,
                inserted_at: now,
                last_used: tick,
            },
        );
        self.total_bytes = self.total_bytes.saturating_add(cost);

        self.evict_to_budget();
    }

    /// Evict least-recently-used entries until within byte + entry caps.
    fn evict_to_budget(&mut self) {
        while self.total_bytes > MAX_CACHE_SIZE_BYTES || self.map.len() > MAX_CACHE_ENTRIES {
            let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, slot)| slot.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.remove(&victim);
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.total_bytes = 0;
        // `tick` is intentionally left running; it's a monotonic stamp, not state.
    }
}

static URL_CACHE: Lazy<Mutex<UrlCache>> = Lazy::new(|| Mutex::new(UrlCache::new()));

/// Look up a cached fetch by its *original* URL, honoring the 15-minute TTL.
///
/// Returns `None` on a miss or an expired (lazily-evicted) entry. Uses
/// [`Instant::now`] as the clock; see [`cache_get_at`] for the test seam.
#[must_use]
pub fn cache_get(url: &str) -> Option<CachedFetch> {
    cache_get_at(url, Instant::now())
}

/// [`cache_get`] with an injectable clock (test seam for TTL expiry).
#[must_use]
pub fn cache_get_at(url: &str, now: Instant) -> Option<CachedFetch> {
    let mut cache = URL_CACHE.lock().expect("URL_CACHE poisoned");
    cache.get(url, now)
}

/// Store a successful fetch, keyed by its *original* URL (`utils.ts:505-517`).
///
/// Uses [`Instant::now`] as the insert timestamp; see [`cache_set_at`] for the
/// test seam.
pub fn cache_set(url: String, entry: CachedFetch) {
    cache_set_at(url, entry, Instant::now());
}

/// [`cache_set`] with an injectable insert timestamp (test seam for TTL aging).
pub fn cache_set_at(url: String, entry: CachedFetch, now: Instant) {
    let mut cache = URL_CACHE.lock().expect("URL_CACHE poisoned");
    cache.insert(url, entry, now);
}

/// Empty the URL content cache. Mirrors `clearWebFetchCache` (`utils.ts:80-83`).
///
/// The TS function also clears `DOMAIN_CHECK_CACHE`; that cache lands in Batch 2,
/// so only `URL_CACHE` is cleared here.
pub fn clear_web_fetch_cache() {
    let mut cache = URL_CACHE.lock().expect("URL_CACHE poisoned");
    cache.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(content: &str, status: u16) -> CachedFetch {
        CachedFetch {
            content: content.to_string(),
            status,
            content_type: "text/html".to_string(),
            bytes: content.len(),
            persisted_path: None,
            persisted_size: None,
            fetched_at_ms: 0,
        }
    }

    #[test]
    fn locked_constants_match_ts() {
        // utils.ts:63 — 15 * 60 * 1000 ms.
        assert_eq!(CACHE_TTL, Duration::from_millis(15 * 60 * 1000));
        // utils.ts:64 — 50 * 1024 * 1024 bytes.
        assert_eq!(MAX_CACHE_SIZE_BYTES, 50 * 1024 * 1024);
        assert_eq!(MAX_CACHE_SIZE_BYTES, 52_428_800);
    }

    // ---- public-API smoke tests against the process-global cache -----------
    // These use UNIQUE keys per test (the global URL_CACHE is shared across all
    // tests in the binary and they run in parallel), so they never collide.

    #[test]
    fn roundtrip_set_then_get() {
        let now = Instant::now();
        cache_set_at(
            "https://global.example/roundtrip".into(),
            entry("body-a", 200),
            now,
        );
        let got = cache_get_at("https://global.example/roundtrip", now).expect("hit");
        assert_eq!(got, entry("body-a", 200));
    }

    #[test]
    fn miss_for_absent_key() {
        assert!(cache_get_at("https://global.example/definitely-absent", Instant::now()).is_none());
    }

    #[test]
    fn clear_empties_cache() {
        // Sync test: block on the shared async guard rather than skipping it —
        // this test clears the same process-global cache the async ones use.
        let _cache_guard = crate::testsupport::block_on_web_globals_lock();
        let now = Instant::now();
        cache_set_at(
            "https://global.example/clear-1".into(),
            entry("1", 200),
            now,
        );
        assert!(cache_get_at("https://global.example/clear-1", now).is_some());
        clear_web_fetch_cache();
        assert!(cache_get_at("https://global.example/clear-1", now).is_none());
    }

    // ---- eviction / TTL policy tested on a LOCAL UrlCache instance ----------
    // The policy depends on entry counts and total bytes, which the shared
    // global cache cannot guarantee under parallel tests — so exercise the
    // map's logic directly on a fresh instance.

    #[test]
    fn local_keyed_by_exact_url() {
        let mut c = UrlCache::new();
        let now = Instant::now();
        c.insert("https://h.example/a".into(), entry("A", 200), now);
        assert!(c.get("https://h.example/a", now).is_some());
        // Different path is a distinct key.
        assert!(c.get("https://h.example/b", now).is_none());
    }

    #[test]
    fn local_ttl_expiry_evicts_on_get() {
        let mut c = UrlCache::new();
        let t0 = Instant::now();
        c.insert("https://ttl.example/".into(), entry("body", 200), t0);

        // Just under the TTL: still a hit.
        let almost = t0 + CACHE_TTL.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(c.get("https://ttl.example/", almost).is_some());

        // At/after the TTL: miss, and the entry is evicted.
        let after = t0 + CACHE_TTL;
        assert!(c.get("https://ttl.example/", after).is_none());
        // Subsequent lookups (even back at t0) also miss — it was removed.
        assert!(c.get("https://ttl.example/", t0).is_none());
    }

    #[test]
    fn local_replacing_key_updates_accounting_and_value() {
        let mut c = UrlCache::new();
        let now = Instant::now();
        c.insert("https://r.example/".into(), entry("short", 200), now);
        c.insert("https://r.example/".into(), entry("longer-body", 404), now);
        // One key, not two; accounting reflects only the replacement.
        assert_eq!(c.map.len(), 1);
        assert_eq!(c.total_bytes, "longer-body".len());
        let got = c.get("https://r.example/", now).expect("hit");
        assert_eq!(got.content, "longer-body");
        assert_eq!(got.status, 404);
    }

    #[test]
    fn local_entry_cap_evicts_lru() {
        let mut c = UrlCache::new();
        let now = Instant::now();
        // Insert one more than the entry cap; the never-touched oldest must go.
        for i in 0..=MAX_CACHE_ENTRIES {
            c.insert(format!("https://cap.example/{i}"), entry("x", 200), now);
        }
        assert_eq!(c.map.len(), MAX_CACHE_ENTRIES);
        // The very first inserted key (least-recently-used) should be evicted.
        assert!(c.get("https://cap.example/0", now).is_none());
        // The most recent must still be present.
        assert!(c
            .get(&format!("https://cap.example/{MAX_CACHE_ENTRIES}"), now)
            .is_some());
    }

    #[test]
    fn local_lru_recency_protects_touched_entry() {
        let mut c = UrlCache::new();
        let now = Instant::now();
        c.insert("https://lru.example/keep".into(), entry("k", 200), now);
        // Fill the rest of the cap with throwaway entries (cap-1 of them).
        for i in 0..MAX_CACHE_ENTRIES - 1 {
            c.insert(format!("https://lru.example/{i}"), entry("x", 200), now);
        }
        assert_eq!(c.map.len(), MAX_CACHE_ENTRIES);
        // Touch "keep" so it becomes most-recently-used.
        assert!(c.get("https://lru.example/keep", now).is_some());
        // One more insert overflows the cap; the LRU victim must NOT be "keep".
        c.insert("https://lru.example/overflow".into(), entry("o", 200), now);
        assert!(c.get("https://lru.example/keep", now).is_some());
    }

    #[test]
    fn local_byte_budget_evicts() {
        let mut c = UrlCache::new();
        let now = Instant::now();
        // One entry just under the budget, then another that overflows it: the
        // older (LRU) entry must be evicted to keep total_bytes within budget.
        let big = "x".repeat(MAX_CACHE_SIZE_BYTES - 10);
        c.insert("https://big.example/first".into(), entry(&big, 200), now);
        assert_eq!(c.map.len(), 1);
        let big2 = "y".repeat(100);
        c.insert("https://big.example/second".into(), entry(&big2, 200), now);
        // First (LRU) evicted; total within budget.
        assert!(c.total_bytes <= MAX_CACHE_SIZE_BYTES);
        assert!(c.get("https://big.example/first", now).is_none());
        assert!(c.get("https://big.example/second", now).is_some());
    }

    #[test]
    fn empty_content_costs_at_least_one() {
        // Mirrors lru-cache `Math.max(1, contentBytes)` clamp (utils.ts:516-517):
        // an empty body must not be free — otherwise the byte budget is meaningless.
        let e = entry("", 204);
        assert_eq!(e.size_cost(), 1);
    }
}
