//! `read_file_state` — the per-path read-state registry shared between the
//! orchestrator and the file tools.
//!
//! 1:1 port of claude-code's `readFileState` cache
//! (`FileReadTool.ts:1032` `readFileState.set(fullFilePath, {content,
//! timestamp, offset, limit})`). Each successful `Read` records the file's
//! decoded content, its floor-truncated mtime (in milliseconds, matching TS
//! `Math.floor(mtimeMs)`), and the `offset`/`limit` the read was performed
//! with. Future staleness guards (Edit/Write/NotebookEdit) and the Read
//! dedup path consume this registry.
//!
//! ## Byte-budgeted LRU (1:1 with `FileStateCache`)
//!
//! Backed by a byte-budgeted LRU matching claude-code's `FileStateCache`
//! (`utils/fileStateCache.ts`), constructed
//! `new LRUCache({ max, maxSize, sizeCalculation: n => Math.max(1,
//! Buffer.byteLength(n.content)) })`:
//! - `max` entries = `READ_FILE_STATE_MAX_ENTRIES` (`fileStateCache.ts:18`,
//!   `READ_FILE_STATE_CACHE_SIZE = 100`).
//! - `maxSize` bytes = `READ_FILE_STATE_MAX_BYTES` (`fileStateCache.ts:22`,
//!   `DEFAULT_MAX_CACHE_SIZE_BYTES = 16 * 1024 * 1024`).
//! - per-entry size = `entry.content.len().max(1)` (UTF-8 byte length, the Rust
//!   equal of `Math.max(1, Buffer.byteLength(content))`).
//! - MRU-promote on [`ReadFileStateLru::get`]; on [`ReadFileStateLru::set`],
//!   least-recently-used entries are evicted while the entry count exceeds
//!   `max` OR the accumulated bytes exceed `maxSize`. A lone entry larger than
//!   `maxSize` is retained (lru-cache never evicts the key it just set).
//!
//! Zero new dependency: a `HashMap` + monotonic recency counter (the cap is 100
//! entries, so the O(n) LRU scan on eviction is trivial and deterministic).
//!
//! DEFERRED follow-ups (separate file-tracking items, out of scope here): the
//! `dump()`/`load()` + `cloneFileStateCache` persistence used to snapshot
//! read-state into forked agents (`AgentTool/runAgent.ts:377`),
//! `mergeFileStateCaches` (timestamp merge), the `normalize(key)` path
//! normalization (Rust callers already pass canonicalized absolute keys — see
//! `tools/file/src/lib.rs`), and the `file_state_cache:{entries, bytes}`
//! telemetry.
//!
//! [`ReadFileStateLru::strip_seeded_from_context`] belongs to that deferred
//! group: it is the `Gxe(e,{stripSeededFromContext})` half of the clone seam
//! and therefore has NO caller yet. It is implemented and tested rather than
//! omitted so the seam does not have to re-derive the semantics — a named
//! pending consumer, not dead code.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default max entries (claude-code `READ_FILE_STATE_CACHE_SIZE`,
/// `fileStateCache.ts:18`).
pub const READ_FILE_STATE_MAX_ENTRIES: usize = 100;

/// Default byte budget: 16 MiB (claude-code `DEFAULT_MAX_CACHE_SIZE_BYTES`,
/// `fileStateCache.ts:22` — `16 * 1024 * 1024`).
pub const READ_FILE_STATE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// A single read-state entry — the faithful port of TS
/// `readFileState.set(fullFilePath, {content, timestamp, offset, limit})`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileEntry {
    /// The decoded file content as read (the same string the model saw).
    pub content: String,
    /// File modification time in **milliseconds**, floor-truncated to match
    /// TS `Math.floor(mtimeMs)`.
    pub mtime_ms: i64,
    /// The `offset` the read used (`None` when the read had no `offset`).
    pub offset: Option<u64>,
    /// The `limit` the read used (`None` when the read had no `limit`).
    pub limit: Option<u64>,
    /// Whether this entry was written by the `Read` tool (as opposed to
    /// `Write`/`Edit`/`NotebookEdit`, which update the registry post-write).
    ///
    /// This is the Rust stand-in for TS's `existingState.offset !== undefined`
    /// dedup gate (`FileReadTool.ts:550`). TS distinguishes Read entries from
    /// Edit/Write entries because Read always stores a defaulted numeric
    /// `offset` while Edit/Write store `offset: undefined`. The Rust `Read`
    /// records the *un-defaulted* `offset` (so a full read stores `None`,
    /// matching the staleness guard's full-read discriminator), making
    /// `offset` unusable for the Read-vs-write distinction — hence this
    /// explicit flag. The Read-dedup path (`FileReadTool.ts:547-573`) consults
    /// it to avoid deduping a re-read against a post-edit entry (which would
    /// wrongly point the model at pre-edit content). The staleness guard does
    /// NOT read this field.
    pub from_read: bool,
    /// TS `seededFromContext` — the entry was seeded from a memory file
    /// (LINGXI.md / rules) whose content the model ALREADY received inside the
    /// system prompt's memory block, rather than from a tool call.
    ///
    /// Set by the startup memory-load loop `xCt` (2.1.220 @245883373:
    /// `seededFromContext: jn` where `jn = MLu(Fr)` — i.e. TRUE only when the
    /// file's content is actually rendered into model context; a
    /// `paths:`-gated conditional rule seeds with FALSE) and by the
    /// nested-memory attachment path (@237715046, unconditionally `!0`).
    ///
    /// Consumed by the Read tool's SEEDED dedup branch (@235741459:
    /// `_.seededFromContext && !_.isPartialView && t===1 && r===void 0`) and by
    /// `tengu_file_read_reread`'s `priorOp` (@235740900:
    /// `m.seededFromContext?"seeded":m.offset===void 0?"edit_write":"read"`).
    ///
    /// Cleared wholesale by [`ReadFileStateLru::strip_seeded_from_context`]
    /// (`Gxe(e,{stripSeededFromContext})` @232454658).
    pub seeded_from_context: bool,
    /// TS `isPartialView` — the recorded content is NOT the file's on-disk
    /// bytes.
    ///
    /// Seeded memory files set this to `contentDiffersFromDisk` (@245883373 /
    /// @237715046): the memory loader strips YAML frontmatter and HTML
    /// comments, so what the model saw differs from disk. `bn_` (@230803364)
    /// computes it as `p = d !== e` — an EXACT string compare of the stripped
    /// content against the raw disk text, with no trim.
    ///
    /// Consumed by `Aze`/`wMe` (@232452900, LingXi
    /// `tool_file::read_covers_full_file`: `if((e.offset??1)>1||e.isPartialView)
    /// return false`) and by both Read-dedup gates (@235741459).
    ///
    /// Two producers, both live: the memory seeding site (from
    /// `content_differs_from_disk`) and the Read tool's token-truncation path
    /// (`tools/file/src/read.rs`, `partial_note.is_some()` — the oracle's
    /// `...x!==void 0&&{isPartialView:!0}` @235732534, where `x` is the
    /// truncation note produced only on a token-cap overflow).
    pub is_partial_view: bool,
}

/// One LRU slot: the entry plus its recency stamp and cached byte size.
#[derive(Clone, Debug)]
struct Node {
    entry: ReadFileEntry,
    /// Original and translated pre-canonicalization spellings for each model
    /// route that reached this canonical key, in first-observed order. Keeping
    /// route forms grouped lets reminders require every coordinate spelling to
    /// pass the live Read-deny check without treating the I/O key as an
    /// alternate authorization route.
    requested_path_groups: Vec<Vec<PathBuf>>,
    /// Whether this cache slot represents content the model has actually seen.
    /// Host-provided seed snapshots participate in staleness/dedup but must
    /// stay out of model-context consumers such as `/files` and post-compact
    /// restore.
    in_model_context: bool,
    /// Monotonic recency stamp; higher = more recently used.
    last_used: u64,
    /// Cached `entry.content.len().max(1)` (the lru-cache `sizeCalculation`).
    size: u64,
}

/// The byte-budgeted LRU behind the shared read-state registry.
///
/// Eviction happens on [`Self::set`]: while the entry count exceeds
/// `max_entries` OR the accumulated bytes exceed `max_bytes`, the
/// least-recently-used entry (lowest `last_used`) is dropped — never the key
/// just inserted (it always carries the highest stamp), so a lone over-budget
/// entry is retained, matching lru-cache.
#[derive(Debug)]
pub struct ReadFileStateLru {
    map: HashMap<PathBuf, Node>,
    /// Monotonic clock; every `get`/`set` bumps it to stamp recency.
    clock: u64,
    /// Running sum of every entry's `size` (kept in sync with `map`).
    total_bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl ReadFileStateLru {
    /// Construct with explicit limits.
    #[must_use]
    fn with_limits(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            map: HashMap::new(),
            clock: 0,
            total_bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    /// Fetch a clone of the entry for `path`, promoting it to most-recently-used
    /// (1:1 with lru-cache's MRU-on-`get`).
    pub fn get(&mut self, path: &Path) -> Option<ReadFileEntry> {
        let next = self.clock + 1;
        let node = self.map.get_mut(path)?;
        self.clock = next;
        node.last_used = next;
        Some(node.entry.clone())
    }

    /// Fetch a clone of the entry for `path` WITHOUT promoting it to
    /// most-recently-used — the read-only counterpart of [`Self::get`].
    ///
    /// The Rust equal of iterating `readFileState.entries()`, which lru-cache
    /// performs with `updateRecency:false`. The changed-files (`edited_text_file`)
    /// producer scans EVERY entry once per turn (`Izm` @296537358); doing that
    /// through [`Self::get`] would rewrite the whole recency order behind the
    /// model's back and silently change which entry the LRU evicts next.
    #[must_use]
    pub fn peek(&self, path: &Path) -> Option<ReadFileEntry> {
        self.map.get(path).map(|n| n.entry.clone())
    }

    /// Return the file-tool route groups captured for an entry, in
    /// first-observed order, without changing LRU recency. Each group contains
    /// the model spelling plus any translated filesystem spelling. An empty
    /// list means no model route was captured.
    #[must_use]
    pub fn requested_path_groups(&self, path: &Path) -> Vec<Vec<PathBuf>> {
        self.map
            .get(path)
            .map(|node| node.requested_path_groups.clone())
            .unwrap_or_default()
    }

    /// Whether `path` has an entry, WITHOUT promoting it to most-recently-used.
    ///
    /// The Rust equal of TS `readFileState.has(path)` — the guard on the
    /// nested-memory seeding site (@237715046 `if(!t.readFileState.has(i.path))`)
    /// and on LingXi's startup memory seeding. Deliberately NOT implemented via
    /// [`Self::get`]: seeding runs over every memory file at session start, and
    /// an MRU bump there would silently reorder eviction (and `/files`) behind
    /// the model's back. `lru-cache`'s `has()` likewise defaults to
    /// `updateRecency: false`.
    #[must_use]
    pub fn contains(&self, path: &Path) -> bool {
        self.map.contains_key(path)
    }

    /// Clear [`ReadFileEntry::seeded_from_context`] on EVERY entry, leaving all
    /// other state untouched.
    ///
    /// The Rust equal of `Gxe(e,{stripSeededFromContext})` (2.1.220
    /// @232454658), which clones the LRU dump and rewrites each slot
    /// `value = {...value, seededFromContext:!1}`.
    ///
    /// CURRENTLY UNUSED in LingXi: the oracle only reaches this through the
    /// `dump()`/`load()` + `cloneFileStateCache` persistence seam used to
    /// snapshot read-state into forked agents, which is on this module's
    /// DEFERRED list (see the module docs). It is implemented and tested here
    /// so that seam does not have to re-derive the semantics — it is a named
    /// pending consumer, not dead code.
    pub fn strip_seeded_from_context(&mut self) {
        for node in self.map.values_mut() {
            node.entry.seeded_from_context = false;
        }
    }

    /// Insert (or overwrite) the entry for `path` as most-recently-used, then
    /// evict least-recently-used entries while over the entry/byte budget.
    pub fn set(&mut self, path: PathBuf, entry: ReadFileEntry) {
        self.set_with_model_context(path, entry, true);
    }

    /// Insert (or overwrite) the entry for `path` as most-recently-used with
    /// explicit model-context provenance, then evict least-recently-used
    /// entries while over the entry/byte budget. Model-visible callers also
    /// capture the supplied path as the changed-file route.
    pub fn set_with_model_context(
        &mut self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
    ) {
        self.set_with_requested_path(path, entry, in_model_context, None);
    }

    /// Insert an entry while retaining its current changed-file source route.
    /// Model-visible callers with no distinct route capture `path` itself;
    /// callers with a pre-canonicalized key pass the original path spelling.
    /// Non-model seed producers pass `None` only when Native would not scan
    /// that seed.
    pub fn set_with_requested_path(
        &mut self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        requested_path: Option<PathBuf>,
    ) {
        let requested_path = requested_path.or_else(|| in_model_context.then(|| path.clone()));
        self.set_with_requested_aliases(
            path,
            entry,
            in_model_context,
            requested_path.into_iter().collect(),
        );
    }

    /// Insert or refresh a cache entry with one source route represented by
    /// multiple spellings, such as model and translated host paths. If this is
    /// a model-visible insertion with no aliases, `path` is the observed route.
    /// New routes append once to the existing ordered groups.
    pub fn set_with_requested_aliases(
        &mut self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        new_requested_paths: Vec<PathBuf>,
    ) {
        // The canonical cache key may be reached through more than one model
        // spelling. Keep prior spellings across every refresh: unlike Native's
        // per-spelling Map entries, this cache folds aliases into one key, so a
        // generic update cannot prove an earlier route was removed. Append a
        // newly observed spelling once, preserving first-observed order.
        let previous = self.map.get(&path);
        let mut requested_path_groups = previous
            .map(|old| old.requested_path_groups.clone())
            .unwrap_or_default();
        let new_requested_paths = if new_requested_paths.is_empty() && in_model_context {
            vec![path.clone()]
        } else {
            new_requested_paths
        };
        // A non-model seed may share this canonical slot with an earlier
        // model-visible alias. Preserve that visibility: Native keeps the
        // alias in a separate Map entry, so this folded slot cannot let the
        // seed erase the still-live changed-file candidate.
        let in_model_context = in_model_context || previous.is_some_and(|old| old.in_model_context);
        let mut route = Vec::new();
        for requested_path in new_requested_paths {
            if !route.contains(&requested_path) {
                route.push(requested_path);
            }
        }
        if !route.is_empty() && !requested_path_groups.contains(&route) {
            requested_path_groups.push(route);
        }
        let size = (entry.content.len() as u64).max(1);
        self.clock += 1;
        let last_used = self.clock;
        if let Some(old) = self.map.insert(
            path,
            Node {
                entry,
                requested_path_groups,
                in_model_context,
                last_used,
                size,
            },
        ) {
            // Overwrite: drop the replaced entry's bytes before adding the new.
            self.total_bytes = self.total_bytes.saturating_sub(old.size);
        }
        self.total_bytes += size;
        self.evict();
    }

    /// Remove the entry for `path`, returning it if present and keeping byte
    /// accounting in sync. Used when a shell command writes a file that may have
    /// been cached from a previous read.
    pub fn remove(&mut self, path: &Path) -> Option<ReadFileEntry> {
        let node = self.map.remove(path)?;
        self.total_bytes = self.total_bytes.saturating_sub(node.size);
        Some(node.entry)
    }

    /// Drop LRU entries while over budget. Never evicts the last remaining entry
    /// (so a lone over-`maxSize` entry is retained, matching lru-cache), and
    /// never the just-set key (it carries the highest `last_used`, so the
    /// min-stamp scan can't select it while another entry exists).
    fn evict(&mut self) {
        while self.map.len() > 1
            && (self.map.len() > self.max_entries || self.total_bytes > self.max_bytes)
        {
            let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, n)| n.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(n) = self.map.remove(&victim) {
                self.total_bytes = self.total_bytes.saturating_sub(n.size);
            }
        }
    }

    /// Drain every entry as `(path, entry)` and reset the registry — the Rust
    /// equal of the post-compact `eOt` snapshot + `readFileState.clear()` step
    /// (`orchestrator::restore_post_compact_attachments`). Preserves the prior
    /// `HashMap::drain` call-site (`map.drain().collect()`).
    pub fn drain(&mut self) -> impl Iterator<Item = (PathBuf, ReadFileEntry)> + '_ {
        self.total_bytes = 0;
        self.clock = 0;
        self.map.drain().map(|(k, node)| (k, node.entry))
    }

    /// Every cached path in most-recently-used → least-recently-used order
    /// (1:1 with lru-cache's `keys()` iteration, which yields MRU first). This
    /// includes host-seeded snapshots that participate in staleness/dedup but
    /// are not necessarily model-visible.
    #[must_use]
    pub fn keys(&self) -> Vec<PathBuf> {
        self.ordered_paths(|_| true)
    }

    /// Every MODEL-VISIBLE cached path in most-recently-used →
    /// least-recently-used order. This is the source of truth for `/files`,
    /// relevant-memory dedup against already-loaded
    /// files, and post-compact restore candidate selection.
    #[must_use]
    pub fn model_context_keys(&self) -> Vec<PathBuf> {
        self.ordered_paths(|node| node.in_model_context)
    }

    /// Cached paths with a recorded changed-file source route. Current model,
    /// memory, post-compact restore, and SDK host-seed producers record their
    /// actual source path when writing the entry; route-less synthetic seeds do
    /// not become reminders merely from their cache key.
    #[must_use]
    pub fn changed_file_candidate_keys(&self) -> Vec<PathBuf> {
        self.ordered_paths(|node| !node.requested_path_groups.is_empty())
    }

    fn ordered_paths(&self, include: impl Fn(&Node) -> bool) -> Vec<PathBuf> {
        let mut nodes: Vec<(&PathBuf, u64)> = self
            .map
            .iter()
            .filter_map(|(k, n)| include(n).then_some((k, n.last_used)))
            .collect();
        // MRU first = highest `last_used` first.
        nodes.sort_by(|a, b| b.1.cmp(&a.1));
        nodes.into_iter().map(|(k, _)| k.clone()).collect()
    }

    /// Drain only the MODEL-VISIBLE entries as `(path, entry)` and leave
    /// non-model seed snapshots in place. This preserves host-seeded
    /// staleness/dedup state across compaction while rebuilding model context
    /// solely from files the model had actually seen.
    pub fn drain_model_context(&mut self) -> Vec<(PathBuf, ReadFileEntry)> {
        let model_paths = self.model_context_keys();
        let mut drained = Vec::with_capacity(model_paths.len());
        for path in model_paths {
            if let Some(node) = self.map.remove(&path) {
                self.total_bytes = self.total_bytes.saturating_sub(node.size);
                drained.push((path, node.entry));
            }
        }
        if self.map.is_empty() {
            self.clock = 0;
        }
        drained
    }

    /// Number of cached entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether the registry is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Accumulated byte size across all entries (the lru-cache
    /// `calculatedSize`). Exposed for tests + future `file_state_cache` telemetry.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// The shared, cheaply-clonable read-state registry: absolute path → entry,
/// evicted by a byte-budgeted LRU ([`ReadFileStateLru`]).
///
/// Cloning an `Arc` is cheap; every holder shares the same underlying LRU so a
/// write from one file tool is visible to every other holder.
pub type ReadFileStateMap = Arc<Mutex<ReadFileStateLru>>;

/// Construct a fresh, empty read-state registry with the claude-code defaults
/// (100 entries / 25 MiB).
#[must_use]
pub fn new_read_file_state_map() -> ReadFileStateMap {
    Arc::new(Mutex::new(ReadFileStateLru::with_limits(
        READ_FILE_STATE_MAX_ENTRIES,
        READ_FILE_STATE_MAX_BYTES,
    )))
}

/// Construct a read-state registry with explicit limits (used by eviction
/// tests; also the seam a future config override would use).
#[must_use]
pub fn new_read_file_state_map_with_limits(max_entries: usize, max_bytes: u64) -> ReadFileStateMap {
    Arc::new(Mutex::new(ReadFileStateLru::with_limits(
        max_entries,
        max_bytes,
    )))
}

/// Compute the floor-truncated mtime in milliseconds from a [`SystemTime`],
/// matching TS `Math.floor(mtimeMs)`.
///
/// Returns `0` for the Unix epoch and clamps pre-epoch times to `0` (the file
/// tools always read existing files, so pre-epoch is not expected in practice).
///
/// [`SystemTime`]: std::time::SystemTime
#[must_use]
pub fn mtime_ms_floor(mtime: std::time::SystemTime) -> i64 {
    match mtime.duration_since(std::time::UNIX_EPOCH) {
        // `as_millis()` already floors to whole milliseconds, matching
        // `Math.floor(mtimeMs)`.
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// Insert (or overwrite) the read-state entry for `path`.
///
/// 1:1 with TS `readFileState.set(fullFilePath, …)`. The key is the absolute
/// path the caller resolved; this helper does no normalization of its own. For
/// this generic model-visible producer the supplied path is also recorded as
/// the changed-file route. A producer with a distinct requested spelling must
/// use [`set_with_requested_path`] or [`set_with_requested_aliases`].
pub fn set(map: &ReadFileStateMap, path: PathBuf, entry: ReadFileEntry) {
    if let Ok(mut guard) = map.lock() {
        guard.set(path, entry);
    }
}

/// Insert (or overwrite) the read-state entry for `path` with explicit
/// model-context provenance.
///
/// Generic model-visible callers record `path` as their changed-file route.
/// File tools and seed producers with a distinct observed spelling use
/// [`set_with_requested_path`] or [`set_with_requested_aliases`]. Host snapshots
/// use `in_model_context = false`; their Native changed-file source route is
/// captured separately by the host-seed producer.
pub fn set_with_model_context(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
) {
    if let Ok(mut guard) = map.lock() {
        guard.set_with_model_context(path, entry, in_model_context);
    }
}

/// Insert or refresh read-state while retaining a current source route. When
/// `in_model_context` is true and `requested_path` is `None`, `path` is recorded
/// at this producer boundary as the model path. Non-model seed producers pass
/// `Some(route)` when Native scans their source path. `None` for a non-model
/// entry adds no route and preserves prior routes on the folded key.
pub fn set_with_requested_path(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
    requested_path: Option<PathBuf>,
) {
    if let Ok(mut guard) = map.lock() {
        guard.set_with_requested_path(path, entry, in_model_context, requested_path);
    }
}

/// Insert or refresh read-state with one source route represented by multiple
/// spellings, such as the original guest path plus its translated host path.
/// Empty aliases on a model-visible producer record `path` itself; existing
/// route groups remain in first-observed order.
pub fn set_with_requested_aliases(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
    requested_paths: Vec<PathBuf>,
) {
    if let Ok(mut guard) = map.lock() {
        guard.set_with_requested_aliases(path, entry, in_model_context, requested_paths);
    }
}

/// Fetch a clone of the read-state entry for `path`, if present, promoting it to
/// most-recently-used.
///
/// 1:1 with TS `readFileState.get(fullFilePath)`.
#[must_use]
pub fn get(map: &ReadFileStateMap, path: &Path) -> Option<ReadFileEntry> {
    map.lock().ok().and_then(|mut guard| guard.get(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn entry(content: &str) -> ReadFileEntry {
        ReadFileEntry {
            content: content.to_string(),
            mtime_ms: 1,
            offset: None,
            limit: None,
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        }
    }

    #[test]
    fn set_then_get_roundtrips() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/tmp/a.txt");
        let e = ReadFileEntry {
            content: "hello\nworld\n".to_string(),
            mtime_ms: 1_234,
            offset: Some(2),
            limit: Some(10),
            from_read: true,
            seeded_from_context: false,
            is_partial_view: false,
        };
        set(&map, path.clone(), e.clone());
        assert_eq!(get(&map, &path), Some(e));
    }

    #[test]
    fn get_missing_is_none() {
        let map = new_read_file_state_map();
        assert_eq!(get(&map, Path::new("/nope")), None);
    }

    #[test]
    fn set_overwrites_existing_entry() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/tmp/a.txt");
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                content: "v1".into(),
                mtime_ms: 1,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                content: "v2".into(),
                mtime_ms: 2,
                offset: Some(5),
                limit: Some(7),
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        let got = get(&map, &path).unwrap();
        assert_eq!(got.content, "v2");
        assert_eq!(got.mtime_ms, 2);
        assert_eq!(got.offset, Some(5));
        assert_eq!(got.limit, Some(7));
    }

    #[test]
    fn requested_path_aliases_accumulate_once_in_insertion_order() {
        let map = new_read_file_state_map();
        let canonical = PathBuf::from("/workspace/secrets/a.rs");
        let first = PathBuf::from("/workspace/alias-first/a.rs");
        let first_host = PathBuf::from("/host/workspace/alias-first/a.rs");
        let second = PathBuf::from("/workspace/alias-second/a.rs");
        let second_host = PathBuf::from("/host/workspace/alias-second/a.rs");

        set_with_requested_aliases(
            &map,
            canonical.clone(),
            entry("old"),
            true,
            vec![first.clone(), first_host.clone()],
        );
        set_with_requested_aliases(
            &map,
            canonical.clone(),
            entry("new"),
            true,
            vec![second.clone(), second_host.clone()],
        );
        set_with_requested_aliases(
            &map,
            canonical.clone(),
            entry("newer"),
            true,
            vec![first.clone(), first_host.clone()],
        );
        assert_eq!(
            map.lock().unwrap().requested_path_groups(&canonical),
            vec![
                vec![first.clone(), first_host.clone()],
                vec![second.clone(), second_host.clone()]
            ],
            "repeat observations do not reorder routes"
        );

        set_with_model_context(&map, canonical.clone(), entry("seed"), false);
        let guard = map.lock().unwrap();
        assert_eq!(
            guard.requested_path_groups(&canonical),
            vec![vec![first, first_host], vec![second, second_host]],
            "generic refreshes cannot erase routes that remain separate Native entries"
        );
        assert!(
            guard.model_context_keys().contains(&canonical),
            "a non-model seed cannot hide an existing model-visible route"
        );
    }

    #[test]
    fn arc_clone_shares_underlying_map() {
        let map = new_read_file_state_map();
        let clone = Arc::clone(&map);
        let path = PathBuf::from("/tmp/shared.txt");
        set(
            &clone,
            path.clone(),
            ReadFileEntry {
                content: "shared".into(),
                mtime_ms: 9,
                offset: None,
                limit: None,
                from_read: true,
                seeded_from_context: false,
                is_partial_view: false,
            },
        );
        // A write through `clone` is visible through the original handle.
        assert_eq!(get(&map, &path).unwrap().content, "shared");
    }

    #[test]
    fn mtime_ms_floor_truncates_to_whole_ms() {
        // 1.5009 s -> 1500 ms (sub-millisecond fraction floored away).
        let t = UNIX_EPOCH + Duration::new(1, 500_900_000);
        assert_eq!(mtime_ms_floor(t), 1_500);
    }

    #[test]
    fn mtime_ms_floor_epoch_is_zero() {
        assert_eq!(mtime_ms_floor(UNIX_EPOCH), 0);
    }

    #[test]
    fn evicts_lru_when_entry_count_exceeds_max() {
        // Entry-count cap only (huge byte budget): the 101st insert evicts the
        // first-inserted key; size clamps to `max`.
        let map = new_read_file_state_map_with_limits(100, u64::MAX);
        for i in 0..101 {
            set(&map, PathBuf::from(format!("/f/{i}")), entry("x"));
        }
        assert_eq!(map.lock().unwrap().len(), 100);
        // First-inserted (LRU) evicted; last present.
        assert_eq!(get(&map, Path::new("/f/0")), None);
        assert!(get(&map, Path::new("/f/100")).is_some());
    }

    #[test]
    fn evicts_by_byte_budget() {
        // Byte budget only (huge entry cap): 10-byte entries, 25-byte budget →
        // at most 2 survive; total_bytes stays within budget.
        let map = new_read_file_state_map_with_limits(usize::MAX, 25);
        for i in 0..5 {
            set(&map, PathBuf::from(format!("/b/{i}")), entry("0123456789")); // 10 bytes
        }
        let guard = map.lock().unwrap();
        assert!(
            guard.total_bytes() <= 25,
            "over budget: {}",
            guard.total_bytes()
        );
        assert!(guard.len() <= 2, "too many survived: {}", guard.len());
    }

    #[test]
    fn get_promotes_mru_so_it_survives_eviction() {
        // Cap = 2. Insert A, B. get(A) promotes A. Insert C → B (now LRU) evicted;
        // A survives.
        let map = new_read_file_state_map_with_limits(2, u64::MAX);
        set(&map, PathBuf::from("/A"), entry("a"));
        set(&map, PathBuf::from("/B"), entry("b"));
        assert!(get(&map, Path::new("/A")).is_some()); // promote A to MRU
        set(&map, PathBuf::from("/C"), entry("c")); // evicts LRU = B
        assert!(
            get(&map, Path::new("/A")).is_some(),
            "A promoted, must survive"
        );
        assert_eq!(
            get(&map, Path::new("/B")),
            None,
            "B was LRU, must be evicted"
        );
        assert!(get(&map, Path::new("/C")).is_some());
    }

    #[test]
    fn overwrite_updates_byte_total_not_count() {
        // Overwriting a key subtracts the old size, not double-counts: set P=10B
        // then P=3B → 1 entry, total_bytes = 3.
        let map = new_read_file_state_map_with_limits(100, u64::MAX);
        set(&map, PathBuf::from("/p"), entry("0123456789")); // 10 bytes
        set(&map, PathBuf::from("/p"), entry("abc")); // 3 bytes
        let guard = map.lock().unwrap();
        assert_eq!(guard.len(), 1);
        assert_eq!(guard.total_bytes(), 3);
    }

    #[test]
    fn remove_deletes_entry_and_updates_byte_total() {
        let map = new_read_file_state_map_with_limits(100, u64::MAX);
        let path = PathBuf::from("/tmp/remove-me.txt");
        set(&map, path.clone(), entry("0123456789"));

        let mut guard = map.lock().unwrap();
        assert!(guard.remove(&path).is_some());
        assert_eq!(guard.len(), 0);
        assert_eq!(guard.total_bytes(), 0);
    }

    #[test]
    fn single_oversized_entry_is_retained() {
        // A lone entry larger than the byte budget is kept (lru-cache never
        // evicts the key it just set when it's the only one).
        let map = new_read_file_state_map_with_limits(100, 4);
        set(
            &map,
            PathBuf::from("/big"),
            entry("this content far exceeds four bytes"),
        );
        assert!(
            get(&map, Path::new("/big")).is_some(),
            "lone over-budget entry must be retained"
        );
        assert_eq!(map.lock().unwrap().len(), 1);
    }

    #[test]
    fn keys_are_ordered_mru_first_and_reread_promotes() {
        // Insert a, b, c → MRU→LRU = [c, b, a]. Re-reading (`set`) `a` promotes
        // it → [a, c, b], matching lru-cache's `keys()` (and fixing the old
        // first-insertion `/files` ordering divergence).
        let map = new_read_file_state_map();
        for p in ["/a", "/b", "/c"] {
            set(&map, PathBuf::from(p), entry("x"));
        }
        assert_eq!(
            map.lock().unwrap().keys(),
            vec![
                PathBuf::from("/c"),
                PathBuf::from("/b"),
                PathBuf::from("/a")
            ]
        );
        // Re-read `a` (a fresh `set`, as the Read tool performs) promotes it.
        set(&map, PathBuf::from("/a"), entry("x"));
        assert_eq!(
            map.lock().unwrap().keys(),
            vec![
                PathBuf::from("/a"),
                PathBuf::from("/c"),
                PathBuf::from("/b")
            ]
        );
    }

    #[test]
    fn get_promotes_key_in_keys_order() {
        // A bare `get` (MRU-promote) also reorders `keys()`.
        let map = new_read_file_state_map();
        set(&map, PathBuf::from("/a"), entry("x"));
        set(&map, PathBuf::from("/b"), entry("x"));
        assert!(get(&map, Path::new("/a")).is_some());
        assert_eq!(
            map.lock().unwrap().keys(),
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
    }

    #[test]
    fn changed_file_candidates_include_model_memory_and_host_routes() {
        let map = new_read_file_state_map();
        let model = PathBuf::from("/repo/read.rs");
        let rendered_memory = PathBuf::from("/repo/LINGXI.md");
        let unrendered_memory = PathBuf::from("/repo/hidden.md");
        let host = PathBuf::from("/repo/host-seed.rs");

        set(&map, model.clone(), entry("read"));
        set_with_requested_path(
            &map,
            rendered_memory.clone(),
            ReadFileEntry { seeded_from_context: true, ..entry("memory") },
            false,
            Some(rendered_memory.clone()),
        );
        set_with_model_context(
            &map,
            unrendered_memory.clone(),
            entry("unrendered"),
            false,
        );
        set_with_requested_path(
            &map,
            host.clone(),
            entry("host"),
            false,
            Some(host.clone()),
        );

        let candidates = map.lock().unwrap().changed_file_candidate_keys();
        assert!(candidates.contains(&model));
        assert!(candidates.contains(&rendered_memory));
        assert!(!candidates.contains(&unrendered_memory));
        assert!(candidates.contains(&host));
    }

    #[test]
    fn generic_model_setter_records_a_new_observed_path_after_an_alias() {
        let map = new_read_file_state_map();
        let canonical = PathBuf::from("/repo/target.rs");
        let alias = PathBuf::from("/repo/alias.rs");

        set_with_requested_path(
            &map,
            canonical.clone(),
            entry("through alias"),
            true,
            Some(alias.clone()),
        );
        set(&map, canonical.clone(), entry("direct model read"));

        assert_eq!(
            map.lock().unwrap().requested_path_groups(&canonical),
            vec![vec![alias], vec![canonical.clone()]],
            "a later model source is captured; reminder refresh supplies its chosen alias explicitly"
        );
    }

    #[test]
    fn model_context_keys_exclude_non_model_seed_entries() {
        let map = new_read_file_state_map();
        set(&map, PathBuf::from("/visible"), entry("v"));
        set_with_model_context(&map, PathBuf::from("/seed"), entry("s"), false);
        assert_eq!(
            map.lock().unwrap().keys(),
            vec![PathBuf::from("/seed"), PathBuf::from("/visible")]
        );
        assert_eq!(
            map.lock().unwrap().model_context_keys(),
            vec![PathBuf::from("/visible")]
        );
    }

    #[test]
    fn drain_yields_all_entries_and_resets() {
        // The post-compact snapshot+clear seam: drain returns every (path, entry)
        // and leaves the registry empty with zero accounted bytes.
        let map = new_read_file_state_map();
        set(&map, PathBuf::from("/d/1"), entry("one"));
        set(&map, PathBuf::from("/d/2"), entry("two"));
        let drained: Vec<(PathBuf, ReadFileEntry)> = map.lock().unwrap().drain().collect();
        assert_eq!(drained.len(), 2);
        let guard = map.lock().unwrap();
        assert!(guard.is_empty());
        assert_eq!(guard.total_bytes(), 0);
    }

    #[test]
    fn seeded_entry_roundtrips_its_flags() {
        // The two new oracle fields survive a set/get round-trip on the very
        // entry `get` returns (the consumers are per-ENTRY predicates:
        // @235741459 `_.seededFromContext && !_.isPartialView`).
        let map = new_read_file_state_map();
        let path = PathBuf::from("/tmp/LINGXI.md");
        let e = ReadFileEntry {
            content: "# rules\n".to_string(),
            mtime_ms: 42,
            offset: None,
            limit: None,
            from_read: false,
            seeded_from_context: true,
            is_partial_view: true,
        };
        set(&map, path.clone(), e.clone());
        let got = get(&map, &path).unwrap();
        assert!(got.seeded_from_context);
        assert!(got.is_partial_view);
        assert_eq!(got, e);
    }

    #[test]
    fn contains_does_not_promote_mru() {
        // The seeding guard (`!readFileState.has(path)` @237715046) must use a
        // presence check that does NOT reshuffle LRU order — otherwise seeding
        // at startup would rewrite eviction order behind the model's back.
        let map = new_read_file_state_map_with_limits(2, u64::MAX);
        set(&map, PathBuf::from("/A"), entry("a"));
        set(&map, PathBuf::from("/B"), entry("b"));
        assert!(map.lock().unwrap().contains(Path::new("/A")));
        set(&map, PathBuf::from("/C"), entry("c"));
        assert_eq!(
            get(&map, Path::new("/A")),
            None,
            "contains must NOT promote: /A was still LRU and must be evicted"
        );
        assert!(get(&map, Path::new("/B")).is_some());
        assert!(map.lock().unwrap().contains(Path::new("/C")));
        assert!(!map.lock().unwrap().contains(Path::new("/nope")));
    }

    #[test]
    fn strip_seeded_from_context_clears_every_entry() {
        // `Gxe(e,{stripSeededFromContext})` @232454658 — a per-entry flip of
        // `seededFromContext` to false, touching nothing else.
        let map = new_read_file_state_map();
        let seeded = ReadFileEntry {
            seeded_from_context: true,
            is_partial_view: true,
            ..entry("s1")
        };
        set(&map, PathBuf::from("/s1"), seeded);
        set(
            &map,
            PathBuf::from("/s2"),
            ReadFileEntry {
                seeded_from_context: true,
                ..entry("s2")
            },
        );
        set(&map, PathBuf::from("/plain"), entry("plain"));

        map.lock().unwrap().strip_seeded_from_context();

        for p in ["/s1", "/s2", "/plain"] {
            assert!(
                !get(&map, Path::new(p)).unwrap().seeded_from_context,
                "{p} must be cleared"
            );
        }
        // Nothing else changed.
        let s1 = get(&map, Path::new("/s1")).unwrap();
        assert!(s1.is_partial_view, "is_partial_view must be untouched");
        assert_eq!(s1.content, "s1");
        assert_eq!(map.lock().unwrap().len(), 3);
    }

    #[test]
    fn drain_model_context_preserves_non_model_seed_entries() {
        let map = new_read_file_state_map();
        set(&map, PathBuf::from("/visible"), entry("seen"));
        set_with_model_context(&map, PathBuf::from("/seed"), entry("host"), false);

        let drained = map.lock().unwrap().drain_model_context();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0, PathBuf::from("/visible"));

        let guard = map.lock().unwrap();
        assert_eq!(guard.keys(), vec![PathBuf::from("/seed")]);
        assert_eq!(guard.model_context_keys(), Vec::<PathBuf>::new());
        assert_eq!(guard.total_bytes(), 4);
    }
}
