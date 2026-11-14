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
//! - ordinary per-entry size = `entry.content.len().max(1)` (UTF-8 byte length,
//!   the Rust equal of `Math.max(1, Buffer.byteLength(content))`). Rare exact
//!   UTF-16 sidecars additionally count their allocated code units.
//! - MRU-promote on [`ReadFileStateLru::get`]; on [`ReadFileStateLru::set`],
//!   least-recently-used entries are evicted while the entry count exceeds
//!   `max` OR the accumulated bytes exceed `maxSize`. A lone entry larger than
//!   `maxSize` is retained (lru-cache never evicts the key it just set).
//!
//! Zero new dependency: a `HashMap` + monotonic recency counter (the cap is 100
//! entries, so the O(n) LRU scan on eviction is trivial and deterministic).
//!
//! Memory forks clone this cache and strip seeded-context markers. Their
//! successful writes invalidate the foreground view without importing private
//! child reads. Ordinary Agent fork persistence remains a separate follow-up:
//! `dump()`/`load()` + `cloneFileStateCache` (`AgentTool/runAgent.ts:377`),
//! `mergeFileStateCaches` (timestamp merge), the `normalize(key)` path
//! normalization (Rust callers already pass canonicalized absolute keys — see
//! `tools/file/src/lib.rs`), and the `file_state_cache:{entries, bytes}`
//! telemetry.
//!

use std::collections::{HashMap, HashSet};
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

/// Scalar read-state fields for callers that do not inspect the cached content.
/// Fetching these fields preserves the same LRU promotion as a full entry read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadFileMetadata {
    /// Recorded floor-truncated modification time in milliseconds.
    pub mtime_ms: i64,
    /// The read's optional line offset.
    pub offset: Option<u64>,
    /// The read's optional line limit.
    pub limit: Option<u64>,
    /// Whether the entry came from a Read invocation.
    pub from_read: bool,
    /// Whether the entry was seeded from model context.
    pub seeded_from_context: bool,
    /// Whether the recorded content differs from a complete disk view.
    pub is_partial_view: bool,
}

impl From<&ReadFileEntry> for ReadFileMetadata {
    fn from(entry: &ReadFileEntry) -> Self {
        Self {
            mtime_ms: entry.mtime_ms,
            offset: entry.offset,
            limit: entry.limit,
            from_read: entry.from_read,
            seeded_from_context: entry.seeded_from_context,
            is_partial_view: entry.is_partial_view,
        }
    }
}

type FileIdentity = lingxi_core::host::rooted_fs::RootIdentity;

/// Physical identity sampled from the descriptor that supplied a Read snapshot.
pub type ReadFileIdentity = lingxi_core::host::rooted_fs::RootIdentity;

/// Capture a Read's identity before asynchronous processing or publication.
pub fn opened_read_file_identity(file: &std::fs::File) -> std::io::Result<ReadFileIdentity> {
    lingxi_core::host::rooted_fs::opened_file_identity(file)
        .map_err(|error| std::io::Error::other(error.to_string()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    entry: Arc<ReadFileEntry>,
    /// Exact JavaScript code units when disk decoding cannot be represented by
    /// Rust text. Ordinary producers leave this absent and use UTF-16 of text.
    content_units: Option<Vec<u16>>,
    /// Original and translated pre-canonicalization spellings for each model
    /// route that reached this canonical key, in first-observed order. Keeping
    /// route forms grouped lets reminders require every coordinate spelling to
    /// pass the live Read-deny check without treating the I/O key as an
    /// alternate authorization route.
    requested_path_groups: Vec<Vec<PathBuf>>,
    /// Route spellings for the current snapshot, without historical targets.
    snapshot_requested_paths: Vec<PathBuf>,
    /// Whether this cache slot represents content the model has actually seen.
    /// Host-provided seed snapshots participate in staleness/dedup but must
    /// stay out of model-context consumers such as `/files` and post-compact
    /// restore.
    in_model_context: bool,
    snapshot_in_model_context: bool,
    model_file_identity: Option<FileIdentity>,
    /// Monotonic recency stamp; higher = more recently used.
    last_used: u64,
    /// Cached content bytes plus any nonredundant exact UTF-16 sidecar.
    size: u64,
}

/// The byte-budgeted LRU behind the shared read-state registry.
///
/// Eviction happens on [`Self::set`]: while the entry count exceeds
/// `max_entries` OR the accumulated bytes exceed `max_bytes`, the
/// least-recently-used entry (lowest `last_used`) is dropped — never the key
/// just inserted (it always carries the highest stamp), so a lone over-budget
/// entry is retained, matching lru-cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFileStateLru {
    map: HashMap<PathBuf, Node>,
    /// Instruction clipping is a session fence, not an evictable read entry.
    /// Native FileStateCache keeps this Set across delete, clear and clones.
    start_only_paths: Vec<PathBuf>,
    /// Monotonic clock; every `get`/`set` bumps it to stamp recency.
    clock: u64,
    /// Running sum of every entry's `size` (kept in sync with `map`).
    total_bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl ReadFileStateLru {
    /// Apply a compact prepared on an isolated cache, without overwriting an
    /// entry read or edited concurrently after the original snapshot.
    pub fn apply_compaction_transition(&mut self, before: &Self, after: &Self) {
        if self == before {
            *self = after.clone();
            return;
        }
        let keys: HashSet<_> = before.map.keys().chain(after.map.keys()).cloned().collect();
        for key in keys {
            if self.map.get(&key) != before.map.get(&key) {
                continue;
            }
            if let Some(node) = after.map.get(&key) {
                self.map.insert(key, node.clone());
            } else {
                self.map.remove(&key);
            }
        }
        self.total_bytes = self.map.values().map(|node| node.size).sum();
        self.clock = self.clock.max(after.clock);
        for path in &after.start_only_paths {
            self.mark_start_only(path.clone());
        }
        self.evict();
    }

    /// Construct with explicit limits.
    #[must_use]
    fn with_limits(max_entries: usize, max_bytes: u64) -> Self {
        Self {
            map: HashMap::new(),
            start_only_paths: Vec::new(),
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
        Some((*node.entry).clone())
    }

    /// Fetch scalar metadata and promote `path` exactly as [`Self::get`] does,
    /// without cloning its cached file content or source routes.
    pub fn get_metadata(&mut self, path: &Path) -> Option<ReadFileMetadata> {
        let next = self.clock + 1;
        let node = self.map.get_mut(path)?;
        self.clock = next;
        node.last_used = next;
        Some(ReadFileMetadata::from(node.entry.as_ref()))
    }

    /// Fetch exact cached text without losing unpaired UTF-16 code units.
    /// This has the same recency effect as a normal entry lookup.
    pub fn get_content_units(&mut self, path: &Path) -> Option<Vec<u16>> {
        let next = self.clock + 1;
        let node = self.map.get_mut(path)?;
        self.clock = next;
        node.last_used = next;
        Some(
            node.content_units
                .clone()
                .unwrap_or_else(|| node.entry.content.encode_utf16().collect()),
        )
    }

    /// Inspect exact cached text without changing its eviction order.
    #[must_use]
    pub fn peek_content_units(&self, path: &Path) -> Option<Vec<u16>> {
        let node = self.map.get(path)?;
        Some(
            node.content_units
                .clone()
                .unwrap_or_else(|| node.entry.content.encode_utf16().collect()),
        )
    }

    /// Record that instruction context exposed only a prefix of this file.
    /// Never infer clipping merely from stripped frontmatter or comments.
    pub fn mark_start_only(&mut self, path: PathBuf) {
        let path = normalized_fence_path(&path);
        let aliases = self
            .map
            .iter()
            .filter(|(key, node)| {
                normalized_fence_path(key) == path
                    || node
                        .snapshot_requested_paths
                        .iter()
                        .any(|alias| normalized_fence_path(alias) == path)
            })
            .flat_map(|(_, node)| node.snapshot_requested_paths.iter().cloned())
            .collect::<Vec<_>>();
        for alias in std::iter::once(path).chain(aliases) {
            let alias = normalized_fence_path(&alias);
            if !self.start_only_paths.contains(&alias) {
                self.start_only_paths.push(alias);
            }
        }
    }

    /// Persistent clipped-path markers, independent of cached read entries.
    #[must_use]
    pub fn start_only_paths(&self) -> Vec<PathBuf> {
        self.start_only_paths.clone()
    }

    /// A different admitted session owns a fresh clipping fence. Ordinary LRU
    /// clear/drain operations deliberately do not call this reset boundary.
    pub fn reset_for_session(&mut self) {
        self.map.clear();
        self.start_only_paths.clear();
        self.total_bytes = 0;
        self.clock = 0;
    }

    fn has_full_model_view(&self, path: &Path) -> bool {
        self.map
            .get(path)
            .or_else(|| self.map.get(&normalized_fence_path(path)))
            .is_some_and(|node| {
                (node.snapshot_in_model_context || node.entry.seeded_from_context)
                    && entry_covers_full_file(&node.entry)
            })
    }

    /// A full model-visible read satisfies the fence only while that observed
    /// entry remains cached. Eviction makes the persistent fence active again.
    #[must_use]
    pub fn holds_only_start_of(&self, path: &Path) -> bool {
        let path = normalized_fence_path(path);
        self.start_only_paths.iter().any(|marked| marked == &path)
            && !self.has_full_model_view(&path)
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
        self.map.get(path).map(|n| (*n.entry).clone())
    }

    /// Retain an immutable content snapshot without copying text or promoting
    /// recency. Later cache writes cannot change this observation.
    #[must_use]
    pub fn peek_snapshot(&self, path: &Path) -> Option<Arc<ReadFileEntry>> {
        self.map.get(path).map(|node| Arc::clone(&node.entry))
    }

    /// Capture metadata and its nonredundant exact text under one observation,
    /// without promoting recency or copying ordinary cached file bodies.
    /// A later Read may replace this slot; callers must keep these units beside
    /// this exact Arc rather than fetching the new slot after asynchronous I/O.
    #[must_use]
    pub fn peek_snapshot_with_units(
        &self,
        path: &Path,
    ) -> Option<(Arc<ReadFileEntry>, Option<Arc<[u16]>>)> {
        let node = self.map.get(path)?;
        Some((
            Arc::clone(&node.entry),
            node.content_units.as_deref().map(Arc::from),
        ))
    }

    /// Mark a snapshot stale only if no concurrent Read/Edit/Write replaced
    /// it. Preserve content, source routes, visibility, byte accounting and LRU
    /// order; the sentinel timestamp forces the file tools' content check.
    pub fn mark_stale_if_unchanged(&mut self, path: &Path, expected: &ReadFileEntry) -> bool {
        let Some(node) = self.map.get_mut(path) else {
            return false;
        };
        if node.entry.as_ref() != expected {
            return false;
        }
        Arc::make_mut(&mut node.entry).mtime_ms = i64::MIN;
        true
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
    /// Memory workers use this on their private snapshot before dispatching
    /// tools; their inherited history must not rely on a seeded Read stub.
    pub fn strip_seeded_from_context(&mut self) {
        for node in self.map.values_mut() {
            if node.entry.seeded_from_context {
                Arc::make_mut(&mut node.entry).seeded_from_context = false;
            }
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
        self.set_with_requested_aliases_and_units(
            path,
            entry,
            in_model_context,
            new_requested_paths,
            None,
        );
    }

    /// Source-preserving variant for text producers with exact UTF-16 units.
    pub fn set_with_requested_aliases_and_units(
        &mut self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        new_requested_paths: Vec<PathBuf>,
        content_units: Option<Vec<u16>>,
    ) {
        self.set_with_requested_aliases_units_and_identity(
            path,
            entry,
            in_model_context,
            new_requested_paths,
            content_units,
            None,
        );
    }

    fn set_with_requested_aliases_units_and_identity(
        &mut self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        new_requested_paths: Vec<PathBuf>,
        content_units: Option<Vec<u16>>,
        opened_identity: Option<FileIdentity>,
    ) {
        let content_units =
            content_units.filter(|units| entry.content.encode_utf16().ne(units.iter().copied()));
        // The canonical cache key may be reached through more than one model
        // spelling. Keep prior spellings across every refresh: unlike Native's
        // per-spelling Map entries, this cache folds aliases into one key, so a
        // generic update cannot prove an earlier route was removed. Append a
        // newly observed spelling once, preserving first-observed order.
        let new_requested_paths = if new_requested_paths.is_empty() && in_model_context {
            vec![path.clone()]
        } else {
            new_requested_paths
        };
        let mut route = Vec::new();
        for requested_path in new_requested_paths {
            if !route.contains(&requested_path) {
                route.push(requested_path);
            }
        }
        let size = (entry.content.len() as u64)
            .saturating_add(
                content_units
                    .as_ref()
                    .map_or(0, |units| (units.len() as u64).saturating_mul(2)),
            )
            .max(1);
        self.clock += 1;
        let last_used = self.clock;
        // Only the I/O producer can bind snapshot bytes to a physical file.
        // Generic seeds and replayed metadata must not authorize a target that
        // replaced the original file before this publication boundary.
        let model_file_identity = (in_model_context || entry.seeded_from_context)
            .then_some(opened_identity)
            .flatten();
        if self.start_only_paths.iter().any(|marked| {
            *marked == normalized_fence_path(&path)
                || route
                    .iter()
                    .any(|alias| normalized_fence_path(alias) == *marked)
        }) {
            for alias in &route {
                let alias = normalized_fence_path(alias);
                if !self.start_only_paths.contains(&alias) {
                    self.start_only_paths.push(alias);
                }
            }
        }
        let entry = Arc::new(entry);
        match self.map.entry(path) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                let node = slot.get_mut();
                node.snapshot_requested_paths = route.clone();
                if !route.is_empty() && !node.requested_path_groups.contains(&route) {
                    node.requested_path_groups.push(route);
                }
                // Visibility authorizes this snapshot, not all future content
                // inserted under its path. Route history is retained separately.
                node.in_model_context |= in_model_context;
                node.snapshot_in_model_context = in_model_context;
                node.model_file_identity = model_file_identity;
                self.total_bytes = self.total_bytes.saturating_sub(node.size);
                node.entry = entry;
                node.content_units = content_units;
                node.last_used = last_used;
                node.size = size;
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(Node {
                    entry,
                    content_units,
                    snapshot_requested_paths: route.clone(),
                    requested_path_groups: if route.is_empty() {
                        Vec::new()
                    } else {
                        vec![route]
                    },
                    in_model_context,
                    snapshot_in_model_context: in_model_context,
                    model_file_identity,
                    last_used,
                    size,
                });
            }
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
        Some(Arc::unwrap_or_clone(node.entry))
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
        self.map
            .drain()
            .map(|(k, node)| (k, Arc::unwrap_or_clone(node.entry)))
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
                drained.push((path, Arc::unwrap_or_clone(node.entry)));
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

/// One memory worker's private cache. The registry and builtin tools remain
/// shared; only their file-state operations are rebound while its call runs.
pub(crate) struct MemoryForkReadState {
    parent: ReadFileStateMap,
    private: ReadFileStateMap,
}

tokio::task_local! {
    static MEMORY_FORK_READ_STATE: Arc<MemoryForkReadState>;
    static REMINDER_READ_TARGET: (PathBuf, PathBuf);
}

/// Bind an internal changed-file Read to the route whose policy was checked.
/// The binding is request-only and is restored on cancellation or nesting.
pub async fn with_reminder_read_target<T>(
    requested: PathBuf,
    approved: PathBuf,
    future: impl std::future::Future<Output = T>,
) -> T {
    REMINDER_READ_TARGET
        .scope((requested, approved), future)
        .await
}

/// An internal Read tried to use a request other than the one policy approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReminderReadTargetMismatch;

/// Current physical target for the exact, unmodified `file_path` input.
///
/// Read checks this before path normalization. A scoped request mismatch must
/// fail closed rather than look like an ordinary, unbound explicit Read.
pub fn reminder_read_target(
    requested: &Path,
) -> Result<Option<PathBuf>, ReminderReadTargetMismatch> {
    REMINDER_READ_TARGET
        .try_with(|(route, target)| {
            if route.as_os_str() == requested.as_os_str() {
                Ok(Some(target.clone()))
            } else {
                Err(ReminderReadTargetMismatch)
            }
        })
        .unwrap_or(Ok(None))
}

impl MemoryForkReadState {
    pub(crate) fn fork(&self) -> Self {
        let snapshot = self
            .private
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Self {
            parent: self.parent.clone(),
            private: Arc::new(Mutex::new(snapshot)),
        }
    }

    pub(crate) fn new(parent: ReadFileStateMap, inherit_reads: bool) -> Self {
        let mut snapshot = {
            let parent = parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inherit_reads {
                parent.clone()
            } else {
                let mut snapshot =
                    ReadFileStateLru::with_limits(parent.max_entries, parent.max_bytes);
                snapshot
                    .start_only_paths
                    .clone_from(&parent.start_only_paths);
                snapshot
            }
        };
        snapshot.strip_seeded_from_context();
        Self {
            parent,
            private: Arc::new(Mutex::new(snapshot)),
        }
    }

    pub(crate) async fn scope<F: std::future::Future>(self: &Arc<Self>, future: F) -> F::Output {
        MEMORY_FORK_READ_STATE.scope(self.clone(), future).await
    }

    fn invalidate_parent_write(
        &self,
        path: &Path,
        written: &ReadFileEntry,
        written_units: Option<&[u16]>,
    ) {
        let canonical = std::fs::canonicalize(path).ok();
        let candidates = {
            let parent = self
                .parent
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            parent
                .keys()
                .into_iter()
                .map(|key| {
                    let aliases = parent
                        .requested_path_groups(&key)
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    (key, aliases)
                })
                .collect::<Vec<_>>()
        };
        // Foreground reads may use a symlinked or translated model spelling.
        // Resolve aliases outside the cache lock and preserve their old view.
        let affected = candidates
            .into_iter()
            .filter_map(|(key, aliases)| {
                std::iter::once(&key)
                    .chain(aliases.iter())
                    .any(|route| {
                        route == path
                            || canonical.as_ref().is_some_and(|target| {
                                std::fs::canonicalize(route).ok().as_ref() == Some(target)
                            })
                    })
                    .then_some(key)
            })
            .collect::<Vec<_>>();
        let mut parent = self
            .parent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for key in affected {
            let Some(view) = parent.peek(&key) else {
                continue;
            };
            // A foreground Read that already saw these bytes may race this
            // callback. Preserve that observation.
            let exact_content_changed = written_units.is_some_and(|written_units| {
                let node = parent.map.get(&key).expect("peek found this entry");
                match &node.content_units {
                    Some(prior_units) => prior_units.as_slice() != written_units,
                    None => node
                        .entry
                        .content
                        .encode_utf16()
                        .ne(written_units.iter().copied()),
                }
            });
            if view.content != written.content || view.is_partial_view || exact_content_changed {
                parent.mark_stale_if_unchanged(&key, &view);
            }
        }
    }
}

fn active_map(map: &ReadFileStateMap) -> ReadFileStateMap {
    MEMORY_FORK_READ_STATE
        .try_with(|state| {
            if Arc::ptr_eq(map, &state.parent) {
                state.private.clone()
            } else {
                map.clone()
            }
        })
        .unwrap_or_else(|_| map.clone())
}

/// Owned routing authority for a mutation sent to a blocking worker. Tokio
/// task locals do not propagate to spawn_blocking, so capture both the private
/// read registry and the parent-invalidation callback before leaving the task.
#[derive(Clone)]
pub struct ReadFileStateWriteContext {
    map: ReadFileStateMap,
    memory_fork: Option<Arc<MemoryForkReadState>>,
}

/// Capture the executing model registry and any foreground invalidation owner.
#[must_use]
pub fn capture_read_file_state_write(map: &ReadFileStateMap) -> ReadFileStateWriteContext {
    let memory_fork = MEMORY_FORK_READ_STATE
        .try_with(|state| Arc::ptr_eq(map, &state.parent).then(|| state.clone()))
        .ok()
        .flatten();
    ReadFileStateWriteContext {
        map: active_map(map),
        memory_fork,
    }
}

impl ReadFileStateWriteContext {
    /// This registry has already been resolved to the executing model's view.
    #[must_use]
    pub fn map(&self) -> ReadFileStateMap {
        self.map.clone()
    }

    /// Check clipping fences using the captured execution view.
    #[must_use]
    pub fn start_only_write_guard(
        &self,
        observed_path: &Path,
        io_path: &Path,
    ) -> Option<StartOnlyWriteBlock> {
        start_only_write_guard(&self.map, observed_path, io_path)
    }

    /// Publish a completed write and invalidate its captured foreground view.
    pub fn set_after_write_with_requested_aliases_and_units(
        &self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        requested_paths: Vec<PathBuf>,
        content_units: Option<Vec<u16>>,
    ) {
        self.set_after_write_with_identity(
            path,
            entry,
            in_model_context,
            requested_paths,
            content_units,
            None,
        );
    }

    /// Publish mutation bytes with the identity from the successful write handle.
    pub fn set_after_write_with_identity(
        &self,
        path: PathBuf,
        entry: ReadFileEntry,
        in_model_context: bool,
        requested_paths: Vec<PathBuf>,
        content_units: Option<Vec<u16>>,
        identity: Option<ReadFileIdentity>,
    ) {
        if let Some(fork) = &self.memory_fork {
            self.map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_with_requested_aliases_units_and_identity(
                    path.clone(),
                    entry.clone(),
                    in_model_context,
                    requested_paths,
                    content_units.clone(),
                    identity,
                );
            fork.invalidate_parent_write(&path, &entry, content_units.as_deref());
        } else {
            self.map
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .set_with_requested_aliases_units_and_identity(
                    path,
                    entry,
                    in_model_context,
                    requested_paths,
                    content_units,
                    identity,
                );
        }
    }
}

/// Construct a fresh, empty read-state registry with the claude-code defaults
/// (100 entries / 16 MiB).
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
    if let Ok(mut guard) = active_map(map).lock() {
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
    if let Ok(mut guard) = active_map(map).lock() {
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
    if let Ok(mut guard) = active_map(map).lock() {
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
    if let Ok(mut guard) = active_map(map).lock() {
        guard.set_with_requested_aliases(path, entry, in_model_context, requested_paths);
    }
}

/// Insert exact decoded text while retaining the same path provenance and LRU
/// semantics as [`set_with_requested_aliases`].
pub fn set_with_requested_aliases_and_units(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
    requested_paths: Vec<PathBuf>,
    content_units: Option<Vec<u16>>,
) {
    if let Ok(mut guard) = active_map(map).lock() {
        guard.set_with_requested_aliases_and_units(
            path,
            entry,
            in_model_context,
            requested_paths,
            content_units,
        );
    }
}

/// Publish a model-visible Read with the identity of its actual opened source.
/// An unavailable identity stays unavailable; the current pathname must never
/// grant an exemption to bytes read from an earlier target.
pub fn set_from_opened_file(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    requested_paths: Vec<PathBuf>,
    content_units: Option<Vec<u16>>,
    identity: Option<ReadFileIdentity>,
) {
    if let Ok(mut guard) = active_map(map).lock() {
        guard.set_with_requested_aliases_units_and_identity(
            path,
            entry,
            true,
            requested_paths,
            content_units,
            identity,
        );
    }
}

/// Record successful file mutation in the executing model's cache and fence
/// an older foreground observation when this is a private memory-fork call.
pub fn set_after_write_with_requested_aliases(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
    requested_paths: Vec<PathBuf>,
) {
    set_after_write_with_requested_aliases_and_units(
        map,
        path,
        entry,
        in_model_context,
        requested_paths,
        None,
    );
}

/// Source-preserving post-mutation publication, including memory-fork fences.
pub fn set_after_write_with_requested_aliases_and_units(
    map: &ReadFileStateMap,
    path: PathBuf,
    entry: ReadFileEntry,
    in_model_context: bool,
    requested_paths: Vec<PathBuf>,
    content_units: Option<Vec<u16>>,
) {
    capture_read_file_state_write(map).set_after_write_with_requested_aliases_and_units(
        path,
        entry,
        in_model_context,
        requested_paths,
        content_units,
    );
}

/// Fetch a clone of the read-state entry for `path`, if present, promoting it to
/// most-recently-used.
///
/// 1:1 with TS `readFileState.get(fullFilePath)`.
#[must_use]
pub fn get(map: &ReadFileStateMap, path: &Path) -> Option<ReadFileEntry> {
    active_map(map)
        .lock()
        .ok()
        .and_then(|mut guard| guard.get(path))
}

/// Fetch scalar metadata, promoting the entry with the same missing-path and
/// poisoned-mutex behavior as [`get`], without cloning its content.
#[must_use]
pub fn get_metadata(map: &ReadFileStateMap, path: &Path) -> Option<ReadFileMetadata> {
    active_map(map)
        .lock()
        .ok()
        .and_then(|mut guard| guard.get_metadata(path))
}

/// Exact code units for freshness comparisons; ordinary Rust text is encoded
/// on demand when that producer did not supply a sidecar.
#[must_use]
pub fn get_content_units(map: &ReadFileStateMap, path: &Path) -> Option<Vec<u16>> {
    active_map(map)
        .lock()
        .ok()
        .and_then(|mut guard| guard.get_content_units(path))
}

/// Mark a clipped instruction prefix in the executing model registry.
pub fn mark_start_only(map: &ReadFileStateMap, path: PathBuf) {
    active_map(map)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mark_start_only(path);
}

#[must_use]
/// Whether this execution still lacks a complete view of a clipped file.
pub fn holds_only_start_of(map: &ReadFileStateMap, path: &Path) -> bool {
    active_map(map)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .holds_only_start_of(path)
}

fn entry_covers_full_file(entry: &ReadFileEntry) -> bool {
    if entry.offset.unwrap_or(1) > 1 || entry.is_partial_view {
        return false;
    }
    entry.limit.is_none_or(|limit| {
        !entry.content.is_empty() && (entry.content.matches('\n').count() as u64 + 1) < limit
    })
}

fn normalized_fence_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir if result.file_name().is_some_and(|name| name != "..") => {
                result.pop();
            }
            Component::ParentDir if result.has_root() => {}
            _ => result.push(component.as_os_str()),
        }
    }
    if result.as_os_str().is_empty() {
        result.push(".");
    }
    result
}

/// Why a complete replacement would lose an unseen instruction-file suffix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartOnlyWriteBlock {
    /// The requested spelling is itself marked as clipped.
    Direct,
    /// Another clipped spelling names this same physical file.
    Alias,
    /// Physical identity could not exclude an alias of a clipped file.
    UnknownIdentity,
}

impl StartOnlyWriteBlock {
    /// Native model-facing refusal corresponding to this fence.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Direct => "You were given only the start of this file, and nothing shows that you have since read all of it in one call, so a Write could lose the rest. To change part of it, use Edit. To replace it whole, first Read it in one call with no offset or limit. If Read returns only a page or says the file is too large, Read it again with allow_large: true. If even that cannot return all of it, use Edit.",
            Self::Alias => "Only the start of this file was given in this session, so a Write could lose the rest. Nothing was written. To change it, use Edit. If Edit says that the path is a link, use it on the link's target.",
            Self::UnknownIdentity => "Nothing was written. Only the start of a file was given in this session, and it could not be checked that this path is a different file. To change this file, use Edit.",
        }
    }
}

/// Check the persistent clipping fence, including hard links and symlink
/// spellings. Call after permission/regular-file admission and again at the
/// owned mutation boundary. No filesystem operation occurs while holding the
/// registry lock. A missing target is a new file and cannot lose a suffix.
#[must_use]
pub fn start_only_write_guard(
    map: &ReadFileStateMap,
    observed_path: &Path,
    io_path: &Path,
) -> Option<StartOnlyWriteBlock> {
    let (marked, direct, observed_identity) = {
        let map = active_map(map);
        let state = map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let full_identity = |path: &Path| {
            state
                .map
                .get(path)
                .or_else(|| state.map.get(&normalized_fence_path(path)))
                .filter(|_| state.has_full_model_view(path))
                .and_then(|node| node.model_file_identity.clone())
        };
        let observed = normalized_fence_path(observed_path);
        let io = normalized_fence_path(io_path);
        let direct = state
            .start_only_paths
            .iter()
            .any(|path| *path == observed || *path == io);
        let marked = state
            .start_only_paths
            .iter()
            .map(|path| (path.clone(), full_identity(path)))
            .collect::<Vec<_>>();
        (marked, direct, full_identity(observed_path))
    };
    if marked.is_empty() {
        return None;
    }
    let candidate = match file_object_identity(io_path) {
        Ok(Some(identity)) => identity,
        Ok(None) => return None,
        Err(_) => {
            return Some(if direct {
                StartOnlyWriteBlock::Direct
            } else {
                StartOnlyWriteBlock::UnknownIdentity
            })
        }
    };
    if observed_identity.as_ref() == Some(&candidate) {
        return None;
    }
    let mut unknown = false;
    for (path, full_identity) in marked {
        match file_object_identity(&path) {
            Ok(Some(identity)) if full_identity.as_ref() == Some(&identity) => {}
            Ok(Some(identity)) if identity == candidate => {
                return Some(if direct {
                    StartOnlyWriteBlock::Direct
                } else {
                    StartOnlyWriteBlock::Alias
                })
            }
            Ok(_) => {}
            Err(_) => unknown = true,
        }
    }
    if direct {
        return Some(StartOnlyWriteBlock::Direct);
    }
    unknown.then_some(StartOnlyWriteBlock::UnknownIdentity)
}

#[cfg(unix)]
fn file_object_identity(path: &Path) -> std::io::Result<Option<FileIdentity>> {
    use std::os::unix::fs::MetadataExt;
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.ino() != 0 => Ok(Some(
            lingxi_core::host::rooted_fs::metadata_file_identity(&metadata),
        )),
        Ok(_) => Err(std::io::Error::other("file identity is unavailable")),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn file_object_identity(
    path: &Path,
) -> std::io::Result<Option<lingxi_core::host::rooted_fs::RootIdentity>> {
    use std::os::windows::fs::OpenOptionsExt;
    // Access mode zero requests metadata only; unlike a content open it cannot
    // wait for a peer on a named pipe. Allow concurrent replacement/deletion.
    let file = std::fs::OpenOptions::new()
        .access_mode(0)
        .share_mode(7)
        .custom_flags(0x0200_0000)
        .open(path);
    match file {
        Ok(file) => lingxi_core::host::rooted_fs::opened_file_identity(&file)
            .map(Some)
            .map_err(|error| std::io::Error::other(error.to_string())),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(any(unix, windows)))]
fn file_object_identity(_path: &Path) -> std::io::Result<Option<FileIdentity>> {
    Err(std::io::Error::other("file identity is unavailable"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[tokio::test]
    async fn reminder_binding_keeps_raw_request_identity_and_restores_nested_scopes() {
        let raw = PathBuf::from("/workspace/source.txt ");
        let normalized = PathBuf::from("/workspace/source.txt");
        let approved = PathBuf::from("/workspace/approved.txt");
        assert_eq!(reminder_read_target(&raw), Ok(None));
        with_reminder_read_target(raw.clone(), approved.clone(), async {
            assert_eq!(reminder_read_target(&raw), Ok(Some(approved.clone())));
            assert_eq!(
                reminder_read_target(&normalized),
                Err(ReminderReadTargetMismatch)
            );
            with_reminder_read_target(normalized.clone(), normalized.clone(), async {
                assert_eq!(
                    reminder_read_target(&normalized),
                    Ok(Some(normalized.clone()))
                );
                assert_eq!(reminder_read_target(&raw), Err(ReminderReadTargetMismatch));
            })
            .await;
            assert_eq!(reminder_read_target(&raw), Ok(Some(approved.clone())));
        })
        .await;
        assert_eq!(reminder_read_target(&raw), Ok(None));

        // Dropping a suspended internal call must not bind the next explicit Read.
        let mut pending = Box::pin(with_reminder_read_target(raw.clone(), approved, async {
            assert!(reminder_read_target(&raw).unwrap().is_some());
            std::future::pending::<()>().await;
        }));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(pending);
        assert_eq!(reminder_read_target(&raw), Ok(None));
    }

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

    fn record_full_read(
        map: &ReadFileStateMap,
        path: PathBuf,
        content: &str,
        aliases: Vec<PathBuf>,
    ) {
        let mut file = std::fs::File::open(aliases.last().unwrap_or(&path)).unwrap();
        let mut actual = String::new();
        std::io::Read::read_to_string(&mut file, &mut actual).unwrap();
        assert_eq!(actual, content);
        let identity = opened_read_file_identity(&file).unwrap();
        set_from_opened_file(map, path, entry(content), aliases, None, Some(identity));
    }

    #[test]
    fn generic_snapshots_do_not_grant_current_path_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "unseen suffix").unwrap();
        let map = new_read_file_state_map();
        mark_start_only(&map, path.clone());
        set(&map, path.clone(), entry("old full snapshot"));
        assert_eq!(
            start_only_write_guard(&map, &path, &path),
            Some(StartOnlyWriteBlock::Direct)
        );
        set_from_opened_file(
            &map,
            path.clone(),
            entry("unseen suffix"),
            vec![path.clone()],
            None,
            None,
        );
        assert_eq!(
            start_only_write_guard(&map, &path, &path),
            Some(StartOnlyWriteBlock::Direct)
        );
    }

    #[test]
    fn translated_partial_read_keeps_host_alias_fenced_after_a_host_seed() {
        let dir = tempfile::tempdir().unwrap();
        let host = dir.path().join("file");
        let guest = PathBuf::from("/virtual/file");
        std::fs::write(&host, "prefix and unseen suffix").unwrap();
        let map = new_read_file_state_map();
        mark_start_only(&map, guest.clone());
        let identity = opened_read_file_identity(&std::fs::File::open(&host).unwrap()).unwrap();
        set_from_opened_file(
            &map,
            guest.clone(),
            ReadFileEntry {
                is_partial_view: true,
                ..entry("prefix")
            },
            vec![guest.clone(), host.clone()],
            None,
            Some(identity),
        );
        set_with_model_context(&map, host.clone(), entry("prefix and unseen suffix"), false);
        assert_eq!(
            start_only_write_guard(&map, &host, &host),
            Some(StartOnlyWriteBlock::Direct)
        );
        map.lock().unwrap().drain().for_each(drop);
        assert_eq!(
            start_only_write_guard(&map, &host, &host),
            Some(StartOnlyWriteBlock::Direct)
        );
        record_full_read(
            &map,
            guest.clone(),
            "prefix and unseen suffix",
            vec![guest.clone(), host.clone()],
        );
        assert_eq!(start_only_write_guard(&map, &guest, &host), None);
    }

    #[test]
    fn full_guest_read_uses_its_translated_io_identity_to_satisfy_the_fence() {
        let root = tempfile::tempdir().unwrap();
        let host = root.path().join("file");
        std::fs::write(&host, "whole").unwrap();
        let guest = PathBuf::from(format!(
            "/virtual-guest-{}/file",
            root.path().file_name().unwrap().to_string_lossy()
        ));
        let map = new_read_file_state_map();
        mark_start_only(&map, guest.clone());
        assert_eq!(
            start_only_write_guard(&map, &guest, &host),
            Some(StartOnlyWriteBlock::Direct)
        );
        record_full_read(
            &map,
            guest.clone(),
            "whole",
            vec![guest.clone(), host.clone()],
        );
        assert_eq!(start_only_write_guard(&map, &guest, &host), None);
    }

    #[test]
    fn host_refresh_cannot_promote_a_partial_model_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        std::fs::write(&path, "prefix and unseen suffix").unwrap();
        let map = new_read_file_state_map();
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                limit: Some(1),
                ..entry("prefix")
            },
        );
        mark_start_only(&map, path.clone());
        set_with_model_context(&map, path.clone(), entry("prefix and unseen suffix"), false);
        assert_eq!(
            start_only_write_guard(&map, &path, &path),
            Some(StartOnlyWriteBlock::Direct)
        );
        assert_eq!(
            map.lock().unwrap().requested_path_groups(&path),
            vec![vec![path.clone()]]
        );
        record_full_read(
            &map,
            path.clone(),
            "prefix and unseen suffix",
            vec![path.clone()],
        );
        assert_eq!(start_only_write_guard(&map, &path, &path), None);
    }

    #[cfg(unix)]
    #[test]
    fn full_view_does_not_authorize_a_retargeted_hardlink_or_directory_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full");
        let clipped = dir.path().join("clipped");
        let route = dir.path().join("route");
        std::fs::write(&full, "full").unwrap();
        std::fs::write(&clipped, "prefix suffix").unwrap();
        std::fs::hard_link(&full, &route).unwrap();
        let map = new_read_file_state_map();
        record_full_read(&map, route.clone(), "full", vec![route.clone()]);
        mark_start_only(&map, clipped.clone());
        std::fs::remove_file(&route).unwrap();
        std::fs::hard_link(&clipped, &route).unwrap();
        assert_eq!(
            start_only_write_guard(&map, &route, &route),
            Some(StartOnlyWriteBlock::Alias)
        );
        assert_eq!(std::fs::read_to_string(&clipped).unwrap(), "prefix suffix");
        record_full_read(&map, route.clone(), "prefix suffix", vec![route.clone()]);
        assert_eq!(start_only_write_guard(&map, &route, &route), None);
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::fs::write(a.join("x"), "A").unwrap();
        std::fs::write(b.join("x"), "B suffix").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let route = link.join("x");
        record_full_read(&map, route.clone(), "A", vec![route.clone()]);
        mark_start_only(&map, b.join("x"));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_eq!(
            start_only_write_guard(&map, &route, &route),
            Some(StartOnlyWriteBlock::Alias)
        );
    }

    #[test]
    fn shared_snapshots_preserve_content_recency_and_copy_on_write() {
        let path = PathBuf::from("/project/a");
        let mut cache = ReadFileStateLru::with_limits(2, 1024);
        cache.set(
            path.clone(),
            ReadFileEntry {
                seeded_from_context: true,
                ..entry("original")
            },
        );
        cache.set(PathBuf::from("/project/b"), entry("other"));
        let order = cache.keys();
        let snapshot = cache.peek_snapshot(&path).unwrap();
        assert!(Arc::ptr_eq(&snapshot, &cache.peek_snapshot(&path).unwrap()));
        assert_eq!(cache.keys(), order, "snapshot lookup must not promote LRU");
        let mut fork = cache.clone();
        fork.strip_seeded_from_context();
        assert!(snapshot.seeded_from_context);
        assert!(cache.peek_snapshot(&path).unwrap().seeded_from_context);
        assert!(!fork.peek_snapshot(&path).unwrap().seeded_from_context);
        assert!(cache.mark_stale_if_unchanged(&path, &snapshot));
        assert_eq!(
            snapshot.mtime_ms, 1,
            "metadata changes cannot mutate snapshots"
        );
        assert_eq!(cache.peek_snapshot(&path).unwrap().mtime_ms, i64::MIN);
        cache.set(path.clone(), entry("updated"));
        assert_eq!(snapshot.content, "original");
        assert_eq!(cache.remove(&path).unwrap().content, "updated");
        assert_eq!(
            snapshot.content, "original",
            "removal cannot invalidate a snapshot"
        );
    }

    #[test]
    fn clipping_fence_survives_lru_removal_and_compaction_drains() {
        let path = PathBuf::from("/project/LINGXI.md");
        let map = new_read_file_state_map_with_limits(1, 1024);
        mark_start_only(&map, path.clone());
        assert!(holds_only_start_of(&map, &path));
        set(&map, path.clone(), entry("whole"));
        assert!(!holds_only_start_of(&map, &path));
        set(&map, PathBuf::from("/other"), entry("other"));
        assert!(
            holds_only_start_of(&map, &path),
            "eviction cannot forget clipping"
        );
        set(&map, path.clone(), entry("whole"));
        map.lock().unwrap().drain_model_context();
        assert!(holds_only_start_of(&map, &path));
        set(&map, path.clone(), entry("whole"));
        map.lock().unwrap().drain().for_each(drop);
        assert!(holds_only_start_of(&map, &path));
        assert!(holds_only_start_of(
            &Arc::new(Mutex::new(map.lock().unwrap().clone())),
            &path
        ));
    }

    #[test]
    fn host_only_seed_and_partial_reads_do_not_satisfy_clipping_fence() {
        let path = PathBuf::from("/project/LINGXI.md");
        let map = new_read_file_state_map();
        mark_start_only(&map, path.clone());
        set_with_model_context(&map, path.clone(), entry("host snapshot"), false);
        assert!(holds_only_start_of(&map, &path));
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                offset: Some(2),
                ..entry("tail")
            },
        );
        assert!(holds_only_start_of(&map, &path));
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                is_partial_view: true,
                ..entry("prefix")
            },
        );
        assert!(holds_only_start_of(&map, &path));
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                limit: Some(1),
                ..entry("one line")
            },
        );
        assert!(holds_only_start_of(&map, &path));
        set(
            &map,
            path.clone(),
            ReadFileEntry {
                limit: Some(2),
                ..entry("one line")
            },
        );
        assert!(
            !holds_only_start_of(&map, &path),
            "bounded read below its limit reached EOF"
        );
    }

    #[test]
    fn clipping_alias_guard_checks_file_identity_and_allows_other_files() {
        let dir = tempfile::tempdir().unwrap();
        let marked = dir.path().join("LINGXI.md");
        let alias = dir.path().join("alias.md");
        let other = dir.path().join("other.md");
        std::fs::write(&marked, "prefix\nhidden tail").unwrap();
        std::fs::hard_link(&marked, &alias).unwrap();
        std::fs::write(&other, "unrelated").unwrap();
        let map = new_read_file_state_map();
        mark_start_only(&map, marked.clone());
        assert_eq!(
            start_only_write_guard(&map, &marked, &marked),
            Some(StartOnlyWriteBlock::Direct)
        );
        assert_eq!(
            start_only_write_guard(&map, &alias, &alias),
            Some(StartOnlyWriteBlock::Alias)
        );
        assert_eq!(start_only_write_guard(&map, &other, &other), None);
        assert_eq!(
            start_only_write_guard(&map, &dir.path().join("new.md"), &dir.path().join("new.md")),
            None
        );
        let deleted = dir.path().join("deleted.md");
        mark_start_only(&map, deleted.clone());
        assert_eq!(
            start_only_write_guard(&map, &deleted, &deleted),
            None,
            "new-file creation cannot overwrite a hidden suffix"
        );
        record_full_read(
            &map,
            alias.clone(),
            "prefix\nhidden tail",
            vec![alias.clone()],
        );
        assert_eq!(
            start_only_write_guard(&map, &alias, &alias),
            None,
            "a full Read through the alias authorizes that view"
        );
        #[cfg(unix)]
        {
            let symlink = dir.path().join("link.md");
            std::os::unix::fs::symlink(&marked, &symlink).unwrap();
            assert_eq!(
                start_only_write_guard(&map, &symlink, &marked),
                Some(StartOnlyWriteBlock::Direct)
            );
        }
    }

    #[test]
    fn exact_utf16_cache_does_not_conflate_distinct_surrogates() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/surrogates.txt");
        let units = vec![0xd800, 0x61, 0xdc00];
        let text = String::from_utf16_lossy(&units);
        set_with_requested_aliases_and_units(
            &map,
            path.clone(),
            entry(&text),
            true,
            vec![],
            Some(units.clone()),
        );
        assert_eq!(get_content_units(&map, &path), Some(units));
        set(&map, path.clone(), entry(&text));
        assert_eq!(
            get_content_units(&map, &path),
            Some(text.encode_utf16().collect())
        );
    }

    #[test]
    fn exact_utf16_sidecars_are_nonredundant_and_count_toward_the_byte_budget() {
        let mut state = ReadFileStateLru::with_limits(10, 6);
        let path = PathBuf::from("/invalid.txt");
        state.mark_start_only(path.clone());
        state.set_with_requested_aliases_and_units(
            path.clone(),
            entry("\u{fffd}"),
            true,
            vec![],
            Some(vec![0xd800]),
        );
        assert_eq!(
            state.total_bytes(),
            5,
            "three display bytes plus two exact-unit bytes"
        );
        state.set(PathBuf::from("/other.txt"), entry("xy"));
        assert!(
            state.peek(&path).is_none(),
            "sidecar memory participates in eviction"
        );
        assert!(
            state.holds_only_start_of(&path),
            "eviction keeps the clipping fence"
        );
        state.set_with_requested_aliases_and_units(
            path.clone(),
            entry("abc"),
            true,
            vec![],
            Some(vec![97, 98, 99]),
        );
        assert!(
            state.map.get(&path).unwrap().content_units.is_none(),
            "valid text has no duplicate UTF-16 allocation"
        );
        state.reset_for_session();
        assert!(state.is_empty());
        assert!(
            !state.holds_only_start_of(&path),
            "a different session cannot inherit a previous clipping fence"
        );
    }

    #[tokio::test]
    async fn memory_fork_metadata_and_units_use_the_private_cache() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/memory/topic.md");
        mark_start_only(&map, path.clone());
        set(&map, path.clone(), entry("foreground"));
        let fork = Arc::new(MemoryForkReadState::new(map.clone(), false));
        fork.scope(async {
            assert!(
                get_metadata(&map, &path).is_none(),
                "metadata cannot leak foreground state into a private worker"
            );
            assert!(
                holds_only_start_of(&map, &path),
                "private workers inherit clipping fences even without reads"
            );
            let units = vec![0xd800];
            set_with_requested_aliases_and_units(
                &map,
                path.clone(),
                ReadFileEntry {
                    mtime_ms: 42,
                    ..entry("\u{fffd}")
                },
                true,
                vec![],
                Some(units.clone()),
            );
            assert_eq!(get_metadata(&map, &path).unwrap().mtime_ms, 42);
            assert_eq!(get_content_units(&map, &path), Some(units));
        })
        .await;
        assert_eq!(get_metadata(&map, &path).unwrap().mtime_ms, 1);
        assert_eq!(get(&map, &path).unwrap().content, "foreground");
    }

    #[tokio::test]
    async fn captured_write_context_preserves_private_state_on_blocking_worker() {
        let map = new_read_file_state_map();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("topic.md");
        std::fs::write(&path, "\u{fffd}").unwrap();
        set_with_requested_aliases_and_units(
            &map,
            path.clone(),
            entry("\u{fffd}"),
            true,
            vec![],
            Some(vec![0xd800]),
        );
        let fork = Arc::new(MemoryForkReadState::new(map.clone(), true));
        fork.scope(async {
            let owned = capture_read_file_state_write(&map);
            let worker_path = path.clone();
            tokio::task::spawn_blocking(move || {
                owned.set_after_write_with_requested_aliases_and_units(
                    worker_path.clone(),
                    ReadFileEntry {
                        mtime_ms: 42,
                        ..entry("\u{fffd}")
                    },
                    true,
                    vec![],
                    Some(vec![0xdc00]),
                );
                assert_eq!(
                    get_content_units(&owned.map(), &worker_path),
                    Some(vec![0xdc00])
                );
            })
            .await
            .unwrap();
            assert_eq!(get_metadata(&map, &path).unwrap().mtime_ms, 42);
        })
        .await;
        assert_eq!(
            get_metadata(&map, &path).unwrap().mtime_ms,
            i64::MIN,
            "equal lossy display must not suppress exact-unit invalidation"
        );
        assert_eq!(
            get_content_units(&map, &path),
            Some(vec![0xd800]),
            "private writes do not invent a new foreground observation"
        );
    }

    #[test]
    fn native_322_clipping_state_matches_pinned_helper_transitions() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/file_memory_fence_322.json"
        ))
        .unwrap();
        for case in fixture["fenceCases"].as_array().unwrap() {
            let mut state = ReadFileStateLru::with_limits(100, 16 * 1024 * 1024);
            for (operation, expected) in case["input"]
                .as_array()
                .unwrap()
                .iter()
                .zip(case["expected"].as_array().unwrap())
            {
                let path = PathBuf::from(operation["path"].as_str().unwrap());
                match operation["op"].as_str().unwrap() {
                    "mark" => state.mark_start_only(path.clone()),
                    "set" => {
                        let source = &operation["entry"];
                        state.set_with_model_context(
                            path.clone(),
                            ReadFileEntry {
                                offset: source["offset"].as_u64(),
                                limit: source["limit"].as_u64(),
                                is_partial_view: source["isPartialView"].as_bool().unwrap_or(false),
                                ..entry(source["content"].as_str().unwrap())
                            },
                            source["contentNotInModelContext"].as_bool() != Some(true),
                        );
                    }
                    "delete" => {
                        state.remove(&path);
                    }
                    "clear" => state.drain().for_each(drop),
                    "clone" => {
                        let paths = state.start_only_paths();
                        state = ReadFileStateLru::with_limits(100, 16 * 1024 * 1024);
                        for path in paths {
                            state.mark_start_only(path);
                        }
                    }
                    other => panic!("unknown pinned operation {other}"),
                }
                assert_eq!(
                    state.holds_only_start_of(&path),
                    expected.as_bool().unwrap(),
                    "{operation:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn memory_fork_reads_are_private_and_only_writes_fence_the_foreground_view() {
        let parent = new_read_file_state_map();
        let path = PathBuf::from("/memory/topic.md");
        let private_path = PathBuf::from("/memory/private-read.md");
        let mut seeded = entry("foreground bytes");
        seeded.seeded_from_context = true;
        set(&parent, path.clone(), seeded.clone());
        let fork = Arc::new(MemoryForkReadState::new(parent.clone(), true));
        fork.scope(async {
            assert!(!get(&parent, &path).unwrap().seeded_from_context);
            set(&parent, private_path.clone(), entry("private observation"));
            tokio::spawn({
                let parent = parent.clone();
                let private_path = private_path.clone();
                async move {
                    assert!(get(&parent, &private_path).is_none());
                }
            })
            .await
            .unwrap();
            let mut written = entry("background bytes");
            written.mtime_ms = 2;
            written.from_read = false;
            set_after_write_with_requested_aliases(
                &parent,
                path.clone(),
                written,
                true,
                vec![path.clone()],
            );
            assert_eq!(get(&parent, &path).unwrap().content, "background bytes");
        })
        .await;
        assert!(get(&parent, &private_path).is_none());
        let view = get(&parent, &path).unwrap();
        assert_eq!(view.content, "foreground bytes");
        assert_eq!(view.mtime_ms, i64::MIN);
        assert!(view.seeded_from_context);
        let next = Arc::new(MemoryForkReadState::new(parent.clone(), true));
        next.scope(async {
            assert!(get(&parent, &private_path).is_none());
        })
        .await;
    }

    #[test]
    fn isolated_compaction_commit_preserves_concurrent_file_updates() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let c = PathBuf::from("/c");
        let mut before = ReadFileStateLru::with_limits(100, u64::MAX);
        before.set(a.clone(), entry("a before"));
        before.set(b.clone(), entry("b before"));
        let mut after = before.clone();
        after.remove(&a);
        after.set(b.clone(), entry("b compact"));
        let mut current = before.clone();
        current.set(b.clone(), entry("b concurrent"));
        current.set(c.clone(), entry("c concurrent"));
        current.apply_compaction_transition(&before, &after);
        assert_eq!(current.peek(&a), None);
        assert_eq!(current.peek(&b), Some(entry("b concurrent")));
        assert_eq!(current.peek(&c), Some(entry("c concurrent")));
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
    fn metadata_get_matches_full_get_promotion_without_changing_the_entry() {
        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let c = PathBuf::from("/c");
        let mut value = entry("recorded content");
        value.mtime_ms = 42;
        value.offset = Some(3);
        value.limit = Some(9);
        value.from_read = false;
        value.seeded_from_context = true;
        value.is_partial_view = true;
        let expected_metadata = ReadFileMetadata::from(&value);
        let mut state = ReadFileStateLru::with_limits(2, u64::MAX);
        state.set_with_requested_aliases(a.clone(), value, false, vec![PathBuf::from("/guest/a")]);
        state.set(b.clone(), entry("b"));
        let mut full_get_state = state.clone();

        let missing_state = state.clone();
        assert_eq!(state.get_metadata(Path::new("/missing")), None);
        assert_eq!(
            state, missing_state,
            "a miss must not advance the LRU clock"
        );
        assert_eq!(state.get_metadata(&a), Some(expected_metadata));
        assert_eq!(
            full_get_state.get(&a).as_ref().map(ReadFileMetadata::from),
            Some(expected_metadata)
        );
        assert_eq!(
            state, full_get_state,
            "metadata touches only get's recency fields"
        );
        state.set(c.clone(), entry("c"));
        assert!(!state.contains(&b));
        assert_eq!(state.keys(), vec![c, a]);
    }

    #[test]
    fn metadata_map_get_keeps_full_get_poison_behavior() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/a");
        set(&map, path.clone(), entry("a"));
        let poison_map = Arc::clone(&map);
        assert!(std::thread::spawn(move || {
            let _guard = poison_map.lock().unwrap();
            panic!("poison the read-state mutex");
        })
        .join()
        .is_err());
        assert_eq!(get_metadata(&map, &path), None);
        assert_eq!(get(&map, &path), None);
    }

    #[test]
    fn alias_refresh_reuses_existing_routes_and_preserves_eviction_accounting() {
        let path = PathBuf::from("/host/a");
        let model = PathBuf::from("/guest/a");
        let other = PathBuf::from("/host/b");
        let mut state = ReadFileStateLru::with_limits(2, 10);
        state.set_with_requested_aliases(
            path.clone(),
            entry("aaaa"),
            true,
            vec![model.clone(), path.clone(), model.clone()],
        );
        state.set(other.clone(), entry("bbbbb"));
        let (groups_storage, route_storage, model_storage) = {
            let node = state.map.get(&path).unwrap();
            (
                node.requested_path_groups.as_ptr(),
                node.requested_path_groups[0].as_ptr(),
                node.requested_path_groups[0][0]
                    .as_os_str()
                    .as_encoded_bytes()
                    .as_ptr(),
            )
        };

        state.set_with_requested_aliases(path.clone(), entry("aaaaaaaa"), false, Vec::new());
        assert!(
            !state.contains(&other),
            "updated content must still drive byte eviction"
        );
        assert_eq!(state.total_bytes(), 8);
        state.set_with_requested_aliases(
            path.clone(),
            entry("x"),
            true,
            vec![model.clone(), path.clone(), model.clone()],
        );
        let node = state.map.get(&path).unwrap();
        assert_eq!(node.requested_path_groups, vec![vec![model, path.clone()]]);
        assert_eq!(node.requested_path_groups.as_ptr(), groups_storage);
        assert_eq!(node.requested_path_groups[0].as_ptr(), route_storage);
        assert_eq!(
            node.requested_path_groups[0][0]
                .as_os_str()
                .as_encoded_bytes()
                .as_ptr(),
            model_storage
        );
        assert!(state.model_context_keys().contains(&path));
        assert_eq!(state.total_bytes(), 1);
        assert_eq!(state.keys(), vec![path]);
    }

    #[test]
    #[ignore = "manual metadata/full-content lookup comparison; no timing threshold"]
    fn metadata_lookup_copy_probe() {
        const LOOKUPS: usize = 512;
        const CONTENT_BYTES: usize = 4 * 1024 * 1024;
        let path = PathBuf::from("/large");
        let mut state = ReadFileStateLru::with_limits(1, u64::MAX);
        state.set(path.clone(), entry(&"x".repeat(CONTENT_BYTES)));
        let started = std::time::Instant::now();
        for _ in 0..LOOKUPS {
            std::hint::black_box(state.get_metadata(std::hint::black_box(&path)));
        }
        let metadata_ns = started.elapsed().as_nanos();
        let started = std::time::Instant::now();
        for _ in 0..LOOKUPS {
            std::hint::black_box(state.get(std::hint::black_box(&path)));
        }
        let full_content_ns = started.elapsed().as_nanos();
        eprintln!("metadata_lookup_copy_probe lookups={LOOKUPS} content_bytes={CONTENT_BYTES} metadata_ns={metadata_ns} full_content_ns={full_content_ns} content_copy_bytes_avoided={}", LOOKUPS * CONTENT_BYTES);
        assert_eq!(state.total_bytes(), CONTENT_BYTES as u64);
    }

    #[test]
    fn marking_stale_preserves_routes_visibility_and_lru_without_overwriting_refreshes() {
        let mut state = ReadFileStateLru::with_limits(100, u64::MAX);
        let path = PathBuf::from("/tmp/a.txt");
        let alias = PathBuf::from("/guest/a.txt");
        let original = entry("old");
        state.set_with_requested_aliases(path.clone(), original.clone(), true, vec![alias.clone()]);
        state.set(PathBuf::from("/tmp/b.txt"), entry("other"));
        let order = state.keys();
        let bytes = state.total_bytes;

        assert!(state.mark_stale_if_unchanged(&path, &original));
        let mut stale = original.clone();
        stale.mtime_ms = i64::MIN;
        assert_eq!(state.peek(&path), Some(stale));
        assert_eq!(state.keys(), order);
        assert_eq!(state.total_bytes, bytes);
        assert_eq!(state.requested_path_groups(&path), vec![vec![alias]]);
        assert!(state.model_context_keys().contains(&path));

        let refreshed = entry("fresh");
        state.set(path.clone(), refreshed.clone());
        assert!(!state.mark_stale_if_unchanged(&path, &original));
        assert_eq!(state.peek(&path), Some(refreshed));
        assert!(!state.mark_stale_if_unchanged(Path::new("/missing"), &original));
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
    fn exact_snapshot_peeks_preserve_recency_and_the_next_eviction() {
        let map = new_read_file_state_map_with_limits(3, u64::MAX);
        for name in ["/A", "/B", "/C"] {
            set(&map, PathBuf::from(name), entry(name));
        }
        {
            let cache = map.lock().unwrap();
            let before = cache.keys();
            for path in &before {
                assert_eq!(
                    cache.peek_content_units(path).unwrap(),
                    cache
                        .peek(path)
                        .unwrap()
                        .content
                        .encode_utf16()
                        .collect::<Vec<_>>()
                );
            }
            assert_eq!(cache.keys(), before);
        }
        set(&map, PathBuf::from("/D"), entry("d"));
        let cache = map.lock().unwrap();
        assert!(cache.peek(Path::new("/A")).is_none());
        assert!(cache.peek(Path::new("/C")).is_some());
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
            ReadFileEntry {
                seeded_from_context: true,
                ..entry("memory")
            },
            false,
            Some(rendered_memory.clone()),
        );
        set_with_model_context(&map, unrendered_memory.clone(), entry("unrendered"), false);
        set_with_requested_path(&map, host.clone(), entry("host"), false, Some(host.clone()));

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

    #[test]
    fn exact_snapshot_keeps_its_original_units_and_eviction_order() {
        let map = new_read_file_state_map();
        let path = PathBuf::from("/exact");
        set_with_requested_aliases_and_units(
            &map,
            path.clone(),
            entry("\u{fffd}"),
            true,
            vec![],
            Some(vec![0xd800]),
        );
        set(&map, PathBuf::from("/ordinary"), entry("ordinary"));
        let (snapshot, units) = {
            let state = map.lock().unwrap();
            let order = state.keys();
            let capture = state.peek_snapshot_with_units(&path).unwrap();
            assert!(Arc::ptr_eq(
                &capture.0,
                &state.peek_snapshot(&path).unwrap()
            ));
            assert!(state
                .peek_snapshot_with_units(Path::new("/ordinary"))
                .unwrap()
                .1
                .is_none());
            assert_eq!(
                state.keys(),
                order,
                "a scan must not promote either observation"
            );
            capture
        };
        set_with_requested_aliases_and_units(
            &map,
            path.clone(),
            entry("\u{fffd}"),
            true,
            vec![],
            Some(vec![0xdc00]),
        );
        assert_eq!(units.as_deref(), Some(&[0xd800][..]));
        let state = map.lock().unwrap();
        let (current, current_units) = state.peek_snapshot_with_units(&path).unwrap();
        assert!(!Arc::ptr_eq(&snapshot, &current));
        assert_eq!(current_units.as_deref(), Some(&[0xdc00][..]));
    }
}
