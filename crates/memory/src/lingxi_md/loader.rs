//! LINGXI.md file reader. Skips any file that is not regular or exceeds
//! [`MEMORY_FILE_BYTE_LIMIT`] (4 MiB on bytes), matching claude-code. The
//! `agents-md` Mod reads its selected `AGENTS.md` instruction files through
//! `fs.ancestors`; those files bypass the memory-file byte cap, while their
//! imported files and ordinary LINGXI.md files keep it.
//!
//! Beyond the raw [`load_file`] reader this module also ports the
//! claude-code `@import` / `@include` expansion and the per-file body
//! sanitisation (frontmatter + block HTML-comment stripping) so the
//! orchestrator can splice referenced files into the memory block exactly
//! the way the TS reference does. See [`expand_memory_file`].
//!
//! The hash-pinned 2.1.286 binary's `Hot` (@184390402) calls `l0`
//! (@177773337) with `T7 = 4194304`. `l0` stats first, skips a non-regular
//! file or `stat.size > T7`, then reads with `encoding: "utf8"`. Invalid UTF-8
//! is replaced with U+FFFD; it does not make the file unreadable. The native
//! backend path, `QDn` (@184389863), applies the same limit to `totalBytes`
//! and decodes the returned byte view with `Buffer.toString("utf8")`.
//!
//! [`load_file`] keeps the same pre-read stat guard and lossy decoding.
//! [`report_skipped_memory_file`] carries `Hot`'s skip line and one-shot
//! `file_skipped_special_or_oversize` report. Other read errors remain soft
//! skips; the native read-error health family is outside this loader's port.
//!
//! Separately from the skip cap, claude-code surfaces a non-blocking warning
//! list for files over
//! [`crate::MAX_MEMORY_CHARACTER_COUNT`] (40k chars) via
//! [`crate::get_large_memory_files`]. The 10 MB drop this loader used to
//! enforce was still not the oracle's rule (the oracle's is 4 MiB on BYTES,
//! before sanitisation) and has been
//! removed.

use crate::lingxi_md::{LingxiMdExcluder, LingxiMdTier};
use regex::Regex;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};
use thiserror::Error;

/// One loaded LINGXI.md (or local override) file.
#[derive(Debug, Clone)]
pub struct LoadedFile {
    /// Absolute path the file was loaded from.
    pub path: PathBuf,
    /// UTF-8 decoded body, replacing malformed byte sequences with U+FFFD.
    /// Secrets are not yet redacted at this layer.
    pub body: String,
    /// Number of raw bytes read, before UTF-8 replacement or redaction.
    pub size_bytes: u64,
}

/// Failure modes the loader can encounter.
#[derive(Debug, Error)]
pub enum LoaderError {
    /// I/O error reading the file.
    #[error("io: {0}")]
    Io(String),
    /// Also carries the NON-REGULAR-file case, because the oracle folds both
    /// into one `null` return (`!o.isFile() || o.size > r`) and every caller
    /// treats them identically: there is no memory file to read here.
    ///
    /// Produced by [`load_file`]'s `T7` guard; the memdir scanner
    /// keeps its own [`crate::MAX_MEMORY_FILE_SIZE`] cap and the `/memory` TUI
    /// dialog's match arm is unchanged.
    #[error("file too large: {bytes} bytes at {path}")]
    FileTooLarge {
        /// Path of the oversized file.
        path: PathBuf,
        /// Observed size in bytes.
        bytes: u64,
        /// Which half of `!o.isFile() || o.size > r` rejected the path.
        ///
        /// Both halves skip the file identically, but the oracle SUPPRESSES its
        /// one-shot report for a directory (`if(!b.skip && !h)` in `Hot`,
        /// with `h = stat.isDirectory()`), so the caller distinguishes them.
        /// Carried here rather than re-`stat`ing at the report site, mirroring
        /// the oracle's `n?.(o)` callback, which hands the same stat straight
        /// back to `Hot`.
        is_directory: bool,
    },
}

/// Telemetry event name for the memdir oversize-skip path.
///
/// ⚠️ NOT AN ORACLE EVENT. `tengu_memory_file_too_large` has ZERO occurrences in
/// the 2.1.220 binary, and [`emit_file_too_large`] below has no production
/// caller — only its own unit test. The doc that claimed this is "the mechanism
/// memdir reports oversize files through" was describing an invention, not a
/// port. The real claude-code report for a skipped memory file is
/// [`report_skipped_memory_file`]; this pair is left in place only because
/// removing it touches memdir's docs, and is flagged so nobody cites it as
/// parity evidence.
pub const TENGU_MEMORY_FILE_TOO_LARGE: &str = "tengu_memory_file_too_large";

/// `tengu_feature_sad` — the generic soft-failure counter the binary's
/// `Ne`/`logFeatureSad` helper emits (@226537274):
/// `Ne(e,t,r) = M("tengu_feature_sad",{...r, feature_name: fe(e), error_code: t})`.
///
/// The same event the MCP tool's `Ue` port already uses; redeclared locally
/// because the `tengu_feature_*` family has no shared home in `telemetry::tengu`.
const TENGU_FEATURE_SAD: &str = "tengu_feature_sad";

/// `feature_name` for the memory-file loading step.
///
/// NOT rebranded: `feature_name`/`error_code` are analytics WIRE identifiers,
/// which this port keeps verbatim (same rule that kept `tengu_*` event names and
/// `mcp_auto_background`). Only human-readable text gets the LingXi name — see
/// [`skip_log_line`].
const CONTEXT_CLAUDE_MD_LOAD_FEATURE: &str = "context_claude_md_load";

/// `error_code` reported when a memory file is skipped as special or oversized.
const FILE_SKIPPED_SPECIAL_OR_OVERSIZE: &str = "file_skipped_special_or_oversize";

/// claude-code 2.1.286 `T7` (@184387135): the byte ceiling above which a
/// memory file is skipped rather than read.
pub const MEMORY_FILE_BYTE_LIMIT: u64 = 4_194_304;

/// Leaf name in the upstream `agents-md` Mod's `AGENTS_NAMES` list. The other
/// listed spelling is `.claude/AGENTS.md`, which has the same leaf name.
const AGENTS_FILE_NAME: &str = "AGENTS.md";

/// Load one LINGXI.md (or local override) file, skipping it when it is not a
/// regular file or exceeds [`MEMORY_FILE_BYTE_LIMIT`].
///
/// Ports claude-code 2.1.286 `l0` (@177773337), called from `Hot`
/// (@184390402) with `r = T7`. Its stat guard uses `size > r`: a file exactly
/// at the limit still loads. Reading with `encoding: "utf8"` replaces
/// malformed UTF-8 rather than rejecting the file, as does `QDn`'s backend
/// `Buffer.toString("utf8")` path.
///
/// The guard uses the stat byte count before reading. The returned local
/// [`LoadedFile::size_bytes`] records the raw bytes actually read; it is not a
/// native memory-info wire field. U+FFFD can occupy more UTF-8 bytes than the
/// malformed input, so the decoded string length cannot supply this value.
///
/// # Errors
///
/// - [`LoaderError::Io`] for filesystem errors (missing, unreadable,
///   permissions).
/// - [`LoaderError::FileTooLarge`] for a non-regular file or one over the
///   limit — the caller treats both as "no memory file here", matching the
///   oracle's `null` return.
pub fn load_file(
    path: &Path,
    _bus: Option<&Arc<telemetry::AnalyticsBus>>,
) -> Result<LoadedFile, LoaderError> {
    load_file_with_byte_limit(path, Some(MEMORY_FILE_BYTE_LIMIT))
}

/// Load a regular UTF-8 file, optionally enforcing a byte ceiling.
///
/// `None` is used only for the root `AGENTS.md` path selected by the upstream
/// Mod's `AGENTS_NAMES` (`AGENTS.md` or `.claude/AGENTS.md`). Its nested Read
/// frame is not subject to the CLAUDE.md/LINGXI.md memory cap. Imported files
/// are expanded through the ordinary capped path.
fn load_file_with_byte_limit(
    path: &Path,
    byte_limit: Option<u64>,
) -> Result<LoadedFile, LoaderError> {
    let meta = std::fs::metadata(path).map_err(|e| LoaderError::Io(e.to_string()))?;
    if !meta.is_file() || byte_limit.is_some_and(|limit| meta.len() > limit) {
        return Err(LoaderError::FileTooLarge {
            path: path.to_path_buf(),
            bytes: meta.len(),
            is_directory: meta.is_dir(),
        });
    }
    let bytes = std::fs::read(path).map_err(|e| LoaderError::Io(e.to_string()))?;
    let size_bytes = bytes.len() as u64;
    let body = String::from_utf8_lossy(&bytes).into_owned();
    Ok(LoadedFile {
        path: path.to_path_buf(),
        body,
        size_bytes,
    })
}

/// Render the skip line claude-code logs for every skipped memory file.
///
/// Byte-verbatim to 2.1.286 `Hot`'s template (@184390402) apart from the name:
/// ``[CLAUDE.md] skipping ${e}: not a regular file or exceeds ${T7} byte limit``.
/// `CLAUDE.md` → `LINGXI.md` is the established rebrand for human-readable text
/// in this crate (`format_large_memory_file_status_row` does the same to "Large
/// CLAUDE.md will impact performance"); the limit is interpolated as the raw
/// number, exactly as `${T7}` renders.
fn skip_log_line(path: &Path) -> String {
    format!(
        "[LINGXI.md] skipping {}: not a regular file or exceeds {MEMORY_FILE_BYTE_LIMIT} byte limit",
        path.display()
    )
}

/// Claim the process-wide one-shot report slot for a skipped memory file.
///
/// Ports 2.1.286 `Hot`'s skip guard: `if(!b.skip && !h) b.skip=!0, p(…)`.
/// Two details that are easy to get wrong and are pinned by tests:
///
/// - A directory returns `false` WITHOUT claiming the slot. The oracle only
///   assigns `b.skip` inside the `!h` arm, so a directory seen first still leaves
///   the report available for a genuine oversize skip later.
/// - Every skip after the first returns `false` — the log line repeats, the
///   report does not.
///
/// Takes the slot by reference so the guard is testable without process-global
/// state; [`report_skipped_memory_file`] supplies the real one.
fn take_skip_report_slot(slot: &std::sync::atomic::AtomicBool, is_directory: bool) -> bool {
    use std::sync::atomic::Ordering;
    if is_directory {
        return false;
    }
    slot.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
}

/// The process-wide one-shot guard for the skip report (`Rwe().skip`).
static SKIP_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Log, and at most once per process report, a memory file skipped by
/// [`load_file`]'s stat guard.
///
/// Ports the `g===null` branch of 2.1.286 `Hot` (@184390402). The log fires on
/// every skip; the report fires at most once and never for a directory.
pub fn report_skipped_memory_file(path: &Path, is_directory: bool) {
    tracing::debug!("{}", skip_log_line(path));
    if take_skip_report_slot(&SKIP_REPORTED, is_directory) {
        tracing::info!(
            event = TENGU_FEATURE_SAD,
            feature_name = CONTEXT_CLAUDE_MD_LOAD_FEATURE,
            error_code = FILE_SKIPPED_SPECIAL_OR_OVERSIZE,
        );
    }
}

/// Emit `tengu_memory_file_too_large` (memdir oversize-skip path).
///
/// No-op when `bus` is `None`. Payload keys (locked):
/// `_PROTO_path: PiiTagged(<path>)`, `size_bytes: Int(N)`.
pub async fn emit_file_too_large(
    bus: Option<&Arc<telemetry::AnalyticsBus>>,
    path: &Path,
    bytes: u64,
) {
    let Some(bus) = bus else {
        return;
    };
    let mut md = telemetry::sink::LogEventMetadata::new();
    md.insert(
        "_PROTO_path".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::pii::PiiTagged::assert_pii_tagged_column(path.display().to_string())
                .into_inner(),
        ),
    );
    // Files this big are pathological — saturate rather than wrap so the
    // event still carries a sensible value if `bytes > i64::MAX`.
    let size_int = i64::try_from(bytes).unwrap_or(i64::MAX);
    md.insert(
        "size_bytes".into(),
        telemetry::sink::AnalyticsValue::Int(size_int),
    );
    bus.log_event(TENGU_MEMORY_FILE_TOO_LARGE, md).await;
}

// ---------------------------------------------------------------------------
// @import expansion + per-file body sanitisation
//
// Ports claude-code `src/utils/claudemd.ts`:
//   - processMemoryFile        (claudemd.ts:618-685) -> [`expand_memory_file`]
//   - parseMemoryFileContent   (claudemd.ts:343-400) -> [`parse_memory_content`]
//   - parseFrontmatterPaths    (claudemd.ts:254-279) -> [`strip_frontmatter`]
//   - stripHtmlCommentsFromTokens (claudemd.ts:303-334) -> [`strip_html_comments`]
//   - extractIncludePathsFromTokens (claudemd.ts:451-535) -> [`extract_include_paths`]
//   - expandPath               (path.ts:32-85)        -> [`expand_path`]
//
// FIDELITY BOUNDARY: the TS reference drives strip/extract off a full
// `marked` markdown lexer. To avoid a new dependency we use a line-oriented
// scanner that understands fenced code blocks (``` / ~~~) and inline code
// spans (`...`). Consequences vs. the lexer:
//   - 4-space *indented* code blocks are NOT recognised, so an @import or
//     `<!-- -->` inside one is treated as normal text.
//   - Emphasis/link markup wrapping a bare `@path` may leave trailing markup
//     in the captured path.
//   - Symlink identities are resolved for exclusion/dedupe, while emitted paths
//     retain the lexical spelling discovered by the hierarchy walker.
// These edges do not occur in any LINGXI.md we ship and are documented here.
// ---------------------------------------------------------------------------

/// Maximum `@import` recursion depth (claude-code `MAX_INCLUDE_DEPTH`,
/// claudemd.ts:537). A node at `depth >= MAX_INCLUDE_DEPTH` is not processed,
/// so a chain expands at most `MAX_INCLUDE_DEPTH` files deep (depths 0..=4).
pub const MAX_INCLUDE_DEPTH: usize = 5;

/// One expanded memory file: a discovered LINGXI.md or an `@import`'d file,
/// after frontmatter + HTML-comment stripping (NOT yet `.trim()`med).
#[derive(Debug, Clone)]
pub struct MemoryEntry {
    /// The path this entry was loaded from (as passed in — display form).
    pub path: PathBuf,
    /// The file whose `@` directive loaded this entry; absent for a discovered root.
    pub parent: Option<PathBuf>,
    /// Sanitised body (frontmatter + block HTML comments removed).
    pub body: String,
    /// Glob patterns from the `paths:` frontmatter key (claudemd.ts:254-279
    /// `parseFrontmatterPaths`): comma/brace-split, trailing `/**` stripped,
    /// match-all `**` dropped. `None` means the rule applies unconditionally;
    /// `Some(_)` marks a CONDITIONAL rule that must NOT be eagerly injected.
    pub globs: Option<Vec<String>>,
    /// The file's unmodified UTF-8 decoded text (claude-code `rawContent`).
    ///
    /// [`load_file`] replaces malformed byte sequences with U+FFFD, matching
    /// native `readFile(..., {encoding: "utf8"})`. No trim or newline
    /// normalization is applied; frontmatter and HTML comments remain.
    ///
    /// The seeded read-state entry uses this (not [`Self::body`], which the
    /// memory-block renderer later `.trim()`s) so a `Read` of the file can be
    /// compared against a UTF-8 decoded read of the file.
    pub raw_content: String,
    /// claude-code 2.1.286 `contentDiffersFromDisk` — `$ot` (@184388419)
    /// computes `let _e = he !== e`: an exact compare of the stripped body
    /// against the raw disk text `e`, with **no trim**.
    ///
    /// `false` for an ordinary LINGXI.md (no frontmatter, no HTML comments —
    /// stripping is a no-op), which is precisely the case the seeded Read
    /// dedup fires on. `true` once frontmatter or a block HTML comment was
    /// stripped, which seeds the entry with `is_partial_view: true` and so
    /// refuses both the dedup and the staleness content-equality fallback.
    pub content_differs_from_disk: bool,
}

/// Result of parsing one memory file's raw bytes.
#[derive(Debug, Clone)]
pub struct ParsedMemory {
    /// Body with a leading YAML frontmatter block and block-level HTML
    /// comments removed (code-fenced / inline-code content preserved).
    pub body: String,
    /// Resolved absolute paths of every `@import` directive found in leaf
    /// text (deduped, in first-seen order).
    pub include_paths: Vec<PathBuf>,
    /// `paths:` frontmatter globs (see [`MemoryEntry::globs`]).
    pub globs: Option<Vec<String>>,
}

/// Discover the external `@import` targets reachable from one top-level memory
/// file without reading any file outside `cwd`.
///
/// Internal imports are followed up to [`MAX_INCLUDE_DEPTH`] so an external
/// target referenced by a nested project file still participates in the
/// startup approval dialog. External targets are returned in first-seen order
/// and are never opened before approval.
#[must_use]
pub fn discover_external_include_paths(
    path: &Path,
    cwd: &Path,
    home: Option<&Path>,
) -> Vec<PathBuf> {
    discover_external_include_paths_with_excluder(path, cwd, home, LingxiMdTier::Project, None)
}

/// Exclude-aware variant of [`discover_external_include_paths`].
#[must_use]
pub fn discover_external_include_paths_with_excluder(
    path: &Path,
    cwd: &Path,
    home: Option<&Path>,
    tier: LingxiMdTier,
    excluder: Option<&LingxiMdExcluder>,
) -> Vec<PathBuf> {
    #[allow(clippy::too_many_arguments)]
    fn visit(
        path: &Path,
        cwd: &Path,
        home: Option<&Path>,
        depth: usize,
        tier: LingxiMdTier,
        excluder: Option<&LingxiMdExcluder>,
        processed: &mut HashSet<PathBuf>,
        external_seen: &mut HashSet<PathBuf>,
        external: &mut Vec<PathBuf>,
    ) {
        if is_memory_path_excluded(path, tier, excluder) {
            return;
        }
        if depth >= MAX_INCLUDE_DEPTH || !claim_path_identities(path, processed) {
            return;
        }
        let Ok(loaded) = load_file(path, None) else {
            return;
        };
        let parsed = parse_memory_content(&loaded.body, path, home);
        if parsed.body.trim().is_empty() {
            return;
        }
        for include in parsed.include_paths {
            if !is_text_include_extension(&include) {
                continue;
            }
            if !path_in_working_path(&include, cwd) {
                // Apply the same exclusion policy to the external target
                // itself before exposing it in the startup approval list. The
                // target may not exist yet, so `is_memory_path_excluded` checks
                // both lexical and canonical identities when available.
                if is_memory_path_excluded(&include, tier, excluder) {
                    continue;
                }
                let normalized =
                    std::fs::canonicalize(&include).unwrap_or_else(|_| lexical_normalize(&include));
                if is_memory_path_excluded(&normalized, tier, excluder) {
                    continue;
                }
                if external_seen.insert(normalized.clone()) {
                    external.push(normalized);
                }
                continue;
            }
            visit(
                &include,
                cwd,
                home,
                depth + 1,
                tier,
                excluder,
                processed,
                external_seen,
                external,
            );
        }
    }

    let mut processed = HashSet::new();
    let mut external_seen = HashSet::new();
    let mut external = Vec::new();
    visit(
        path,
        cwd,
        home,
        0,
        tier,
        excluder,
        &mut processed,
        &mut external_seen,
        &mut external,
    );
    external
}

/// Recursively load `path` and every file it `@import`s, returning a flat
/// list with the parent FIRST, then each include's expansion appended in
/// directive order — matching claude-code `processMemoryFile`
/// (claudemd.ts:661-684: "Add the main file first (parent before children)").
///
/// - `processed`: shared cycle guard. A file's lexical and resolved identities
///   are recorded BEFORE it is read, so imports through symlink aliases cannot
///   replay an ancestor (claudemd.ts:629-645).
/// - `include_external`: when `false`, includes resolving OUTSIDE `cwd` are
///   skipped (claudemd.ts:667-670). User memory passes `true`.
/// - `cwd`: original working dir used for the external-include gate.
/// - `home`: used to expand `@~/...`; `None` makes `~` imports resolve to
///   nothing and be skipped.
/// - `depth`: 0 for a top-level LINGXI.md.
///
/// Missing / unreadable files are silently ignored — the oracle throws out of
/// `l0` into `Hot`'s catch, which reports through a different (unported) family
/// and prints no skip line.
///
/// CORRECTED: the tail of this doc read "There is no size cap — files are read
/// whole", citing `claudemd.ts:433-436`. There IS a cap. A file rejected by
/// [`load_file`]'s stat guard is skipped AND announced via
/// [`report_skipped_memory_file`]; this function supplies `Hot`'s behavior, so the
/// message and the one-shot report belong here rather than in the reader.
#[must_use]
pub fn expand_memory_file<S: std::hash::BuildHasher>(
    path: &Path,
    processed: &mut HashSet<PathBuf, S>,
    include_external: bool,
    cwd: &Path,
    home: Option<&Path>,
    depth: usize,
) -> Vec<MemoryEntry> {
    expand_memory_file_with_excluder(
        path,
        processed,
        include_external,
        cwd,
        home,
        depth,
        LingxiMdTier::Project,
        None,
    )
}

/// Exclude-aware variant of [`expand_memory_file`].
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn expand_memory_file_with_excluder<S: std::hash::BuildHasher>(
    path: &Path,
    processed: &mut HashSet<PathBuf, S>,
    include_external: bool,
    cwd: &Path,
    home: Option<&Path>,
    depth: usize,
    tier: LingxiMdTier,
    excluder: Option<&LingxiMdExcluder>,
) -> Vec<MemoryEntry> {
    expand_memory_file_inner(
        path,
        processed,
        include_external,
        cwd,
        home,
        depth,
        tier,
        excluder,
        is_agents_instruction_file(path),
    )
}

#[allow(clippy::too_many_arguments)]
fn expand_memory_file_inner<S: std::hash::BuildHasher>(
    path: &Path,
    processed: &mut HashSet<PathBuf, S>,
    include_external: bool,
    cwd: &Path,
    home: Option<&Path>,
    depth: usize,
    tier: LingxiMdTier,
    excluder: Option<&LingxiMdExcluder>,
    allow_large_agents_root: bool,
) -> Vec<MemoryEntry> {
    if is_memory_path_excluded(path, tier, excluder) {
        return Vec::new();
    }
    // Skip if already processed or max depth exceeded (claudemd.ts:630).
    if depth >= MAX_INCLUDE_DEPTH || !claim_path_identities(path, processed) {
        return Vec::new();
    }

    // Read; a stat-guard skip is logged + reported here rather than inside
    // `load_file`, matching the oracle's split: `l0` guards the stat and returns
    // `null`, and its caller `Hot` owns the message and the one-shot report.
    //
    // Only THIS site reports. `discover_external_include_paths` re-walks the
    // same files to build the startup approval dialog's target list; it is a
    // port-side pre-scan with no `Hot` counterpart, so logging there too would
    // double every skip line for one user-visible event.
    let loaded = match if allow_large_agents_root {
        load_file_with_byte_limit(path, None)
    } else {
        load_file(path, None)
    } {
        Ok(loaded) => loaded,
        Err(LoaderError::FileTooLarge { is_directory, .. }) => {
            report_skipped_memory_file(path, is_directory);
            return Vec::new();
        }
        // ENOENT / perms: the oracle throws out of `l0` into
        // `Hot`'s catch, which routes to a different report family:
        // `read_eacces` / `read_failed`) and never prints the skip line.
        Err(_) => return Vec::new(),
    };

    let parsed = parse_memory_content(&loaded.body, path, home);
    // claudemd.ts:652 — drop whitespace-only files entirely.
    if lingxi_core::host::instruction_announcements::js_trim(&parsed.body).is_empty() {
        return Vec::new();
    }

    // Parent before children (claudemd.ts:663-664).
    //
    // `content_differs_from_disk` is `$ot`'s `_e = he !== e` (@184388419): the
    // stripped body compared EXACTLY against the raw disk text, no trim. It
    // must be computed HERE, before the memory-block renderer's later
    // `.trim()`, or every file would falsely report "differs". `loaded.body` is
    // unmodified lossy UTF-8 decoded output (see `load_file`), so the comparison
    // uses the same decoded disk text as the native loader.
    let content_differs_from_disk = parsed.body != loaded.body;
    let mut result = vec![MemoryEntry {
        path: path.to_path_buf(),
        parent: None,
        body: parsed.body,
        globs: parsed.globs,
        raw_content: loaded.body,
        content_differs_from_disk,
    }];

    for inc in parsed.include_paths {
        // claude-code `pwd` parse gate (binary @197189020): skip an `@import`
        // whose extension is non-empty and NOT in the text-file allowlist
        // ("Skipping non-text file in @include"). Files with no extension pass.
        if !is_text_include_extension(&inc) {
            continue;
        }
        let is_external = !path_in_working_path(&inc, cwd);
        if is_external && !include_external {
            continue;
        }
        let mut included = expand_memory_file_inner(
            &inc,
            processed,
            include_external,
            cwd,
            home,
            depth + 1,
            tier,
            excluder,
            false,
        );
        if let Some(first) = included.first_mut() {
            first.parent = Some(path.to_path_buf());
        }
        result.extend(included);
    }

    result
}

fn is_agents_instruction_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name == AGENTS_FILE_NAME)
}

fn is_memory_path_excluded(
    path: &Path,
    tier: LingxiMdTier,
    excluder: Option<&LingxiMdExcluder>,
) -> bool {
    excluder.is_some_and(|excluder| {
        excluder.is_excluded(path, tier)
            || std::fs::canonicalize(path)
                .ok()
                .is_some_and(|resolved| excluder.is_excluded(&resolved, tier))
    })
}

/// Claim both the lexical path and its resolved filesystem identity. This keeps
/// symlink aliases from replaying one memory file or bypassing a cycle guard.
fn claim_path_identities<S: std::hash::BuildHasher>(
    path: &Path,
    processed: &mut HashSet<PathBuf, S>,
) -> bool {
    let lexical = lexical_normalize(path);
    let resolved = std::fs::canonicalize(path)
        .ok()
        .map(|path| lexical_normalize(&path));
    if processed.contains(&lexical)
        || resolved
            .as_ref()
            .is_some_and(|resolved| processed.contains(resolved))
    {
        return false;
    }
    processed.insert(lexical);
    if let Some(resolved) = resolved {
        processed.insert(resolved);
    }
    true
}

/// claude-code text-file extension allowlist (binary `cwd` Set @197189020). An
/// `@import` whose extension is non-empty and NOT in this set is skipped. Lookup
/// is case-insensitive (the binary lowercases `path.extname()` before the
/// `.has` check), so entries are stored lowercased without the leading dot.
/// A file with no extension (incl. dotfiles like `.env`, whose Node `extname`
/// is `""`) is allowed.
const TEXT_INCLUDE_EXTENSIONS: &[&str] = &[
    "md",
    "txt",
    "text",
    "json",
    "yaml",
    "yml",
    "toml",
    "xml",
    "csv",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "js",
    "ts",
    "tsx",
    "jsx",
    "mjs",
    "cjs",
    "mts",
    "cts",
    "py",
    "pyi",
    "pyw",
    "rb",
    "erb",
    "rake",
    "go",
    "rs",
    "java",
    "kt",
    "kts",
    "scala",
    "c",
    "cpp",
    "cc",
    "cxx",
    "h",
    "hpp",
    "hxx",
    "cs",
    "swift",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "env",
    "ini",
    "cfg",
    "conf",
    "config",
    "properties",
    "sql",
    "graphql",
    "gql",
    "proto",
    "vue",
    "svelte",
    "astro",
    "ejs",
    "hbs",
    "pug",
    "jade",
    "php",
    "pl",
    "pm",
    "lua",
    "r",
    "dart",
    "ex",
    "exs",
    "erl",
    "hrl",
    "clj",
    "cljs",
    "cljc",
    "edn",
    "hs",
    "lhs",
    "elm",
    "ml",
    "mli",
    "f",
    "f90",
    "f95",
    "for",
    "cmake",
    "make",
    "makefile",
    "gradle",
    "sbt",
    "rst",
    "adoc",
    "asciidoc",
    "org",
    "tex",
    "latex",
    "lock",
    "log",
    "diff",
    "patch",
];

/// `true` if `path` may be expanded as an `@import` target per the binary's
/// text-file gate. Matches Node `path.extname(t).toLowerCase()` semantics: a
/// missing/empty extension passes; otherwise membership in
/// [`TEXT_INCLUDE_EXTENSIONS`].
#[must_use]
fn is_text_include_extension(path: &Path) -> bool {
    let Some(ext) = path.extension() else {
        return true;
    };
    let ext = ext.to_string_lossy().to_ascii_lowercase();
    ext.is_empty() || TEXT_INCLUDE_EXTENSIONS.contains(&ext.as_str())
}

/// Parse one memory file's raw content into its sanitised body and the set
/// of `@import` paths it references. Pure (no I/O). Mirrors
/// `parseMemoryFileContent` (claudemd.ts:343-400): frontmatter is stripped
/// first, then includes are extracted and HTML comments stripped from the
/// remainder.
#[must_use]
pub fn parse_memory_content(raw: &str, file_path: &Path, home: Option<&Path>) -> ParsedMemory {
    let without_fm = strip_frontmatter(raw);
    let globs = parse_frontmatter_paths(raw);
    let base_dir = file_path.parent().unwrap_or_else(|| Path::new("."));
    let include_paths = extract_include_paths(without_fm, base_dir, home);
    let body = strip_html_comments(without_fm);
    ParsedMemory {
        body,
        include_paths,
        globs,
    }
}

fn frontmatter_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // Current native GC = /^---\s*\n([\s\S]*?)---\s*\n?/.
    // ECMAScript \s includes FEFF but excludes NEL; Rust's Unicode \s
    // differs. Preserve the native capture even for an ambiguous closing ---.
    RE.get_or_init(|| {
        let whitespace = r"[\t\n\x0b\x0c\r \u{00a0}\u{1680}\u{2000}-\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}]";
        Regex::new(&format!("(?s)^---{whitespace}*\\n(.*?)---{whitespace}*\\n?")).unwrap()
    })
}

/// Strip a leading `---`…`---` YAML frontmatter block, returning the content
/// past it (claude-code `parseFrontmatter` / `FRONTMATTER_REGEX`,
/// frontmatterParser.ts:123-145). When no frontmatter is present the input is
/// returned unchanged.
#[must_use]
pub fn strip_frontmatter(raw: &str) -> &str {
    // Native Hs applies Ww only for frontmatter detection. If the block is
    // absent it returns the original input, including a leading BOM.
    let for_detection = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    if let Some(m) = frontmatter_re().find(for_detection) {
        if m.start() == 0 {
            return &for_detection[m.end()..];
        }
    }
    raw
}

/// Parse the `paths:` frontmatter key into glob patterns, or `None` when the
/// rule is unconditional. Ports `parseFrontmatterPaths` (claudemd.ts:254-279):
///
/// 1. Read the `paths` value from the leading frontmatter block.
/// 2. Split it with `split_path_in_frontmatter` (comma-split respecting
///    braces, then brace-expansion — `splitPathInFrontmatter`).
/// 3. Drop a trailing `/**` from each pattern (the `ignore` crate treats
///    `path` as matching the dir and everything under it).
/// 4. Filter out empty patterns; if nothing remains, or every pattern is the
///    match-all `**`, return `None` (applies to all paths).
///
/// The frontmatter is parsed with the same YAML semantics as other markdown
/// frontmatter in this crate. This matters for inline comments, quoted `#` and
/// commas, flow/block sequences, and YAML escaping.
#[must_use]
pub fn parse_frontmatter_paths(raw: &str) -> Option<Vec<String>> {
    let values = frontmatter_paths_values(raw)?;
    // ONE budget shared across the whole frontmatter block (oracle threads a
    // single `t` through every `uCg` call), so multiple parts / list items can't
    // each claim a fresh allowance.
    let mut budget = BraceBudget::default();
    let mut patterns = Vec::new();
    for value in values {
        match value {
            FrontmatterPathValue::Scalar(value) => {
                patterns.extend(split_path_in_frontmatter(&value, &mut budget));
            }
            FrontmatterPathValue::ListItem(value) => {
                patterns.extend(expand_braces_budgeted(&value, &mut budget));
            }
        }
    }
    let patterns: Vec<String> = patterns
        .into_iter()
        .map(|pattern| {
            // Remove a trailing `/**` (claudemd.ts:266-269).
            pattern
                .strip_suffix("/**")
                .map(str::to_string)
                .unwrap_or(pattern)
        })
        .filter(|p| !p.is_empty())
        .collect();
    // All `**` (or empty) ⇒ unconditional (claudemd.ts:272-276).
    if patterns.is_empty() || patterns.iter().all(|p| p == "**") {
        return None;
    }
    Some(patterns)
}

enum FrontmatterPathValue {
    /// A scalar follows Claude's comma/braces path-list grammar.
    Scalar(String),
    /// A YAML sequence item is already one logical path; a quoted comma stays
    /// literal while brace expansion still applies.
    ListItem(String),
}

/// Parse the leading frontmatter as YAML and project its `paths` value.
fn frontmatter_paths_values(raw: &str) -> Option<Vec<FrontmatterPathValue>> {
    let for_detection = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let captures = frontmatter_re().captures(for_detection)?;
    let yaml = captures.get(1)?.as_str();
    let document: serde_yaml::Value = serde_yaml::from_str(yaml).ok()?;
    let paths = document
        .as_mapping()?
        .get(serde_yaml::Value::String("paths".into()))?;
    match paths {
        serde_yaml::Value::String(value) => Some(vec![FrontmatterPathValue::Scalar(value.clone())]),
        serde_yaml::Value::Sequence(values) => {
            let values = values
                .iter()
                .filter_map(serde_yaml::Value::as_str)
                .map(|value| FrontmatterPathValue::ListItem(value.to_string()))
                .collect::<Vec<_>>();
            (!values.is_empty()).then_some(values)
        }
        _ => None,
    }
}

/// Comma-split a frontmatter path value while respecting `{...}` braces, then
/// brace-expand each part. 1:1 with `splitPathInFrontmatter` +
/// `expandBraces` (frontmatterParser.ts:189-266), with the bounded expansion
/// added in Claude Code 2.1.217.
fn split_path_in_frontmatter(input: &str, budget: &mut BraceBudget) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut brace_depth: i32 = 0;
    for ch in input.chars() {
        match ch {
            '{' => {
                brace_depth += 1;
                current.push(ch);
            }
            '}' => {
                brace_depth -= 1;
                current.push(ch);
            }
            ',' if brace_depth == 0 => {
                let trimmed = current.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    let trimmed = current.trim();
    if !trimmed.is_empty() {
        parts.push(trimmed.to_string());
    }

    let mut expanded = Vec::new();
    for part in parts.into_iter().filter(|part| !part.is_empty()) {
        expanded.extend(expand_braces_budgeted(&part, budget));
    }
    expanded
}

/// Shared brace-expansion budget — Claude Code 2.1.218's `{results, bytes}`
/// (`lCg=1000`, `cCg=4_194_304`). It is MUTATED across every path in one
/// frontmatter block (not reset per part / list item), and when a single pattern
/// would blow it the expander returns that pattern UNEXPANDED with a warn rather
/// than truncating. This is the DoS fix: the previous 100_000-result cap could
/// still allocate ~100k × ~30KB ≈ 3GB just by opening a project whose `paths:`
/// held a wide multi-group brace product; the byte budget trips first.
struct BraceBudget {
    results: usize,
    bytes: i64,
}

impl Default for BraceBudget {
    fn default() -> Self {
        Self {
            results: 1_000,
            bytes: 4_194_304,
        }
    }
}

/// Claude Code 2.1.218 `uCg`: expand `{a,b}` groups under the shared
/// [`BraceBudget`].
///
/// Uses a first-CLOSE-brace match (the oracle regex `^([^{]*)\{([^}]+)\}(.*)$`),
/// so the alternatives run up to the FIRST `}` and may themselves contain `{`.
/// A nested `{a,{b,c}}` therefore expands to `["a}","b","c}"]` — matching
/// upstream's (quirky) behaviour is required for byte parity. On budget exceed
/// the whole pattern is returned unexpanded (one element) with a warn.
fn expand_braces_budgeted(pattern: &str, budget: &mut BraceBudget) -> Vec<String> {
    if !pattern.contains('{') {
        return vec![pattern.to_string()];
    }
    let pattern_len = pattern.chars().count() as i64;
    let mut results: Vec<String> = Vec::new();
    let mut pending: Vec<String> = vec![pattern.to_string()];
    while let Some(current) = pending.pop() {
        let Some((prefix, alternatives, suffix)) = match_first_brace(&current) else {
            results.push(current);
            continue;
        };
        let branches: Vec<&str> = alternatives.split(',').map(str::trim).collect();
        // Charge the current string, then bail (unexpanded) if the projected
        // item count or its byte cost would exceed what remains — 1:1 with
        // `t.bytes<0 || u>t.results || u*e.length>t.bytes`.
        budget.bytes -= current.chars().count() as i64;
        let projected = results.len() + pending.len() + branches.len();
        if budget.bytes < 0
            || projected > budget.results
            || (projected as i64) * pattern_len > budget.bytes
        {
            tracing::warn!(
                "Brace pattern expansion exceeds the budget; using it unexpanded: {}",
                truncate_for_log(pattern, 256)
            );
            return vec![pattern.to_string()];
        }
        for branch in branches.iter().rev() {
            pending.push(format!("{prefix}{branch}{suffix}"));
        }
    }
    budget.results = budget.results.saturating_sub(results.len());
    budget.bytes -= (results.len() as i64) * pattern_len;
    results
}

/// The oracle regex `^([^{]*)\{([^}]+)\}(.*)$` — the FIRST `{` and the FIRST
/// `}` after it. Alternatives (`[^}]+`) must be non-empty and carry no `}` but
/// MAY carry `{` (the un-closed nested case), so `{}` does not match and
/// `{a,{b,c}}` splits as `a` / `{b` / `c` against a `}` suffix.
fn match_first_brace(s: &str) -> Option<(&str, &str, &str)> {
    let open = s.find('{')?;
    let after = &s[open + 1..];
    let close_rel = after.find('}')?;
    if close_rel == 0 {
        return None;
    }
    Some((&s[..open], &after[..close_rel], &after[close_rel + 1..]))
}

/// Truncate a pattern to `max` chars for the budget-exceeded warn (oracle
/// `Pl(e,256)`).
fn truncate_for_log(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

fn comment_span_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // claude-code commentSpan = /<!--[\s\S]*?-->/g
    RE.get_or_init(|| Regex::new(r"(?s)<!--.*?-->").unwrap())
}

fn strip_comment_spans(s: &str) -> String {
    comment_span_re().replace_all(s, "").into_owned()
}

/// Strip block-level `<!-- ... -->` HTML comments, preserving comments inside
/// fenced code blocks and inline code spans. Mirrors
/// `stripHtmlCommentsFromTokens` (claudemd.ts:303-334): a comment that begins
/// a line (≤3 leading spaces) is removed up to and including the line that
/// contains `-->`; any residue on that closing line is kept. Unclosed
/// comments and mid-line (inline) comments are left in place.
#[must_use]
pub fn strip_html_comments(content: &str) -> String {
    if !content.contains("<!--") {
        return content.to_string();
    }
    // Native uat invokes the marked lexer only when this marker is present;
    // Rq.lex normalizes CRLF and bare CR before token sanitization. A body
    // without an HTML-comment marker retains its disk line endings.
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let content = normalized.as_str();
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut out = String::with_capacity(content.len());
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;
    while i < lines.len() {
        let raw = lines[i];
        let probe = raw.trim_end_matches(['\r', '\n']);

        if let Some((fc, fl)) = fence {
            if is_closing_fence(probe, fc, fl) {
                fence = None;
            }
            out.push_str(raw);
            i += 1;
            continue;
        }
        if let Some((c, n)) = opening_fence(probe) {
            fence = Some((c, n));
            out.push_str(raw);
            i += 1;
            continue;
        }

        if strip_leading_spaces_max3(probe).starts_with("<!--") {
            // Find the line that closes the comment block.
            let mut j = i;
            let mut found = false;
            while j < lines.len() {
                if lines[j].trim_end_matches(['\r', '\n']).contains("-->") {
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                // Unclosed comment — leave the remainder verbatim
                // (claudemd.ts:317 requires a matching `-->`).
                for line in &lines[i..] {
                    out.push_str(line);
                }
                break;
            }
            let mut block = String::new();
            for line in &lines[i..=j] {
                block.push_str(line);
            }
            let residue = strip_comment_spans(&block);
            if !residue.trim().is_empty() {
                out.push_str(&residue);
            }
            i = j + 1;
            continue;
        }

        out.push_str(raw);
        i += 1;
    }
    out
}

fn include_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // claude-code includeRegex = /(?:^|\s)@((?:[^\s\\]|\\ )+)/g
    RE.get_or_init(|| Regex::new(r"(?:^|\s)@((?:[^\s\\]|\\ )+)").unwrap())
}

/// Extract `@import` paths from leaf text and resolve them to absolute paths
/// relative to `base_dir`. Ports `extractIncludePathsFromTokens`
/// (claudemd.ts:451-535): code spans / fenced blocks are skipped, block HTML
/// comments contribute only their residue, `#fragment` suffixes are dropped,
/// `\ ` escapes are unescaped, and the accept rules at claudemd.ts:476-489 are
/// applied before resolution. Results are deduped in first-seen order.
#[must_use]
pub fn extract_include_paths(content: &str, base_dir: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let scannable = build_scannable_text(content);
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut out = Vec::new();
    for caps in include_re().captures_iter(&scannable) {
        let raw = caps.get(1).map_or("", |m| m.as_str());
        // Strip fragment identifiers (#heading) — claudemd.ts:466-469.
        let mut path = match raw.find('#') {
            Some(idx) => raw[..idx].to_string(),
            None => raw.to_string(),
        };
        if path.is_empty() {
            continue;
        }
        // Unescape spaces — claudemd.ts:473.
        path = path.replace("\\ ", " ");
        if path.is_empty() || !is_valid_include_path(&path) {
            continue;
        }
        if let Some(resolved) = expand_path(&path, base_dir, home) {
            if seen.insert(resolved.clone()) {
                out.push(resolved);
            }
        }
    }
    out
}

/// Accept rules for a candidate `@import` path (claudemd.ts:476-489).
fn is_valid_include_path(path: &str) -> bool {
    if path.starts_with("./") || path.starts_with("~/") {
        return true;
    }
    if path.starts_with('/') && path != "/" {
        return true;
    }
    // Bareword: must start with [a-zA-Z0-9._-] (which also excludes a leading
    // '@' and the punctuation class /^[#%^&*()]+/).
    matches!(
        path.chars().next(),
        Some(c) if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'
    )
}

/// Expand a path that may use `~`, `~/`, an absolute prefix, or be relative to
/// `base_dir`. Ports `expandPath` (path.ts:32-85), minus the Windows POSIX
/// conversion. Returns `None` only when a `~`/`~/` path is given but `home`
/// is `None`.
#[must_use]
pub fn expand_path(path: &str, base_dir: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Some(lexical_normalize(base_dir));
    }
    if trimmed == "~" {
        return home.map(lexical_normalize);
    }
    if let Some(rest) = trimmed.strip_prefix("~/") {
        return home.map(|h| lexical_normalize(&h.join(rest)));
    }
    let p = Path::new(trimmed);
    if p.is_absolute() {
        return Some(lexical_normalize(p));
    }
    Some(lexical_normalize(&base_dir.join(p)))
}

/// Build the text over which `@import` extraction runs: fenced code blocks are
/// dropped, inline code spans are removed, and block HTML comments contribute
/// only their residue (claudemd.ts:494-531). Line boundaries are preserved so
/// the leading `(?:^|\s)` of the include regex still matches at line starts.
fn build_scannable_text(content: &str) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let mut buf = String::with_capacity(content.len());
    let mut fence: Option<(char, usize)> = None;
    let mut i = 0;
    while i < lines.len() {
        let probe = lines[i].trim_end_matches(['\r', '\n']);

        if is_indented_code_line(probe) {
            buf.push('\n');
            i += 1;
            continue;
        }

        if let Some((fc, fl)) = fence {
            if is_closing_fence(probe, fc, fl) {
                fence = None;
            }
            buf.push('\n');
            i += 1;
            continue;
        }
        if let Some((c, n)) = opening_fence(probe) {
            fence = Some((c, n));
            buf.push('\n');
            i += 1;
            continue;
        }

        if strip_leading_spaces_max3(probe).starts_with("<!--") {
            let mut j = i;
            let mut found = false;
            while j < lines.len() {
                if lines[j].trim_end_matches(['\r', '\n']).contains("-->") {
                    found = true;
                    break;
                }
                j += 1;
            }
            if !found {
                // Unclosed comment block — skipped entirely for extraction.
                for _ in i..lines.len() {
                    buf.push('\n');
                }
                break;
            }
            let mut block = String::new();
            for line in &lines[i..=j] {
                block.push_str(line);
            }
            // Only the residue outside the comment spans can carry @paths.
            let residue = strip_comment_spans(&block);
            buf.push_str(&residue);
            if !residue.ends_with('\n') {
                buf.push('\n');
            }
            i = j + 1;
            continue;
        }

        buf.push_str(&strip_inline_code(probe));
        buf.push('\n');
        i += 1;
    }
    buf
}

/// Remove inline code spans (`` `...` ``) from a single line so `@paths`
/// inside them are ignored (claudemd.ts:496-498 skips `codespan`).
fn strip_inline_code(line: &str) -> String {
    if !line.contains('`') {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            let start = i;
            let mut open = 0;
            while i < chars.len() && chars[i] == '`' {
                open += 1;
                i += 1;
            }
            // Find a closing run of exactly `open` backticks.
            let mut k = i;
            let mut close_end = None;
            while k < chars.len() {
                if chars[k] == '`' {
                    let run_start = k;
                    let mut run = 0;
                    while k < chars.len() && chars[k] == '`' {
                        run += 1;
                        k += 1;
                    }
                    if run == open {
                        close_end = Some(k);
                        break;
                    }
                    let _ = run_start;
                } else {
                    k += 1;
                }
            }
            match close_end {
                Some(end) => i = end, // drop the whole span
                None => {
                    // No closing run — keep the backticks literally.
                    out.extend(&chars[start..i]);
                }
            }
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// Detect an opening fence line: ≤3 leading spaces then a run of ≥3 `` ` `` or
/// `~`. Returns the fence char and run length.
fn opening_fence(probe: &str) -> Option<(char, usize)> {
    if count_leading_spaces(probe) > 3 {
        return None;
    }
    let rest = strip_leading_spaces_max3(probe);
    let c = rest.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let run = rest.chars().take_while(|&x| x == c).count();
    if run >= 3 {
        Some((c, run))
    } else {
        None
    }
}

/// A closing fence: same char, run length ≥ the opening run, nothing but
/// whitespace after the run.
fn is_closing_fence(probe: &str, fence_char: char, fence_len: usize) -> bool {
    if count_leading_spaces(probe) > 3 {
        return false;
    }
    let rest = strip_leading_spaces_max3(probe);
    let run = rest.chars().take_while(|&x| x == fence_char).count();
    if run < fence_len {
        return false;
    }
    rest.chars().skip(run).collect::<String>().trim().is_empty()
}

fn count_leading_spaces(s: &str) -> usize {
    s.bytes().take_while(|&b| b == b' ').count()
}

fn strip_leading_spaces_max3(s: &str) -> &str {
    let n = count_leading_spaces(s).min(3);
    &s[n..]
}

fn is_indented_code_line(s: &str) -> bool {
    s.starts_with('\t') || count_leading_spaces(s) >= 4
}

/// Lexically normalise a path (resolve `.` / `..` components) WITHOUT touching
/// the filesystem — used for the cycle guard and the external-include gate.
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// True when `path` is `working` or lives under it.
///
/// Existing paths are canonicalized before comparison so a project-local
/// symlink cannot make an external import look confined. For a missing target
/// we fall back to lexical containment: it cannot be opened by the loader, and
/// this preserves the upstream behavior of silently ignoring missing imports.
fn path_in_working_path(path: &Path, working: &Path) -> bool {
    let p = std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path));
    let w = std::fs::canonicalize(working).unwrap_or_else(|_| lexical_normalize(working));
    p == w || p.starts_with(&w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    #[test]
    fn frontmatter_ecmascript_whitespace_matches_actual_native_287_parser() {
        let oracle: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/instruction_marked_2_1_287.json"
        ))
        .unwrap();
        let cases: Vec<_> = oracle["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| case["name"].as_str().unwrap().starts_with("frontmatter-"))
            .collect();
        assert_eq!(cases.len(), 3);
        for case in cases {
            let raw = case["input"]["raw"].as_str().unwrap();
            let path = Path::new(case["input"]["path"].as_str().unwrap());
            let parsed = parse_memory_content(raw, path, None);
            let native = &case["expected"]["parsed"];
            assert_eq!(
                parsed.body,
                native["info"]["content"].as_str().unwrap(),
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::json!(parsed.globs),
                native["info"]["globs"],
                "{}",
                case["name"]
            );
            assert_eq!(
                serde_json::json!(parsed.include_paths),
                native["includePaths"],
                "{}",
                case["name"]
            );
            assert_ne!(parsed.body, raw, "native frontmatter was removed");
        }
    }

    #[test]
    fn expand_memory_file_reports_disk_fidelity() {
        // Pins the oracle rule `bn_` @230803364: `let p = d !== e` — an EXACT
        // string compare of the frontmatter/HTML-comment-stripped body against
        // the RAW disk text, with NO trim. This is what keeps the seeded Read
        // dedup from being inert: an ordinary LINGXI.md with a trailing newline
        // and no frontmatter has `differs === false`, so its seeded entry holds
        // the byte-exact disk text and CAN dedup.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();

        let plain = cwd.join("LINGXI.md");
        fs::write(&plain, "# rules\nbe good\n").unwrap();
        let mut seen = HashSet::new();
        let out = expand_memory_file(&plain, &mut seen, false, cwd, None, 0);
        assert_eq!(out.len(), 1);
        assert!(
            !out[0].content_differs_from_disk,
            "plain file with a trailing newline must NOT differ from disk"
        );
        assert_eq!(
            out[0].raw_content, "# rules\nbe good\n",
            "raw_content must be the byte-exact disk text, trailing newline included"
        );

        let fm = cwd.join("cond.md");
        fs::write(&fm, "---\npaths: src/**\n---\nbody\n").unwrap();
        let mut seen = HashSet::new();
        let out = expand_memory_file(&fm, &mut seen, false, cwd, None, 0);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].content_differs_from_disk,
            "stripped frontmatter must mark the entry as differing from disk"
        );
        assert_eq!(out[0].raw_content, "---\npaths: src/**\n---\nbody\n");
        assert_ne!(out[0].body, out[0].raw_content);

        let com = cwd.join("comment.md");
        fs::write(&com, "start\n<!-- hidden -->\nend\n").unwrap();
        let mut seen = HashSet::new();
        let out = expand_memory_file(&com, &mut seen, false, cwd, None, 0);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].content_differs_from_disk,
            "a stripped HTML comment must mark the entry as differing from disk"
        );
        assert_eq!(out[0].raw_content, "start\n<!-- hidden -->\nend\n");
    }

    #[test]
    fn text_include_extension_gate_matches_binary() {
        // In the allowlist (case-insensitive).
        for p in ["a.md", "b.RS", "c.Json", "d.toml", "e.py", "notes.txt"] {
            assert!(
                is_text_include_extension(Path::new(p)),
                "{p} should be a text include"
            );
        }
        // No extension (incl. dotfiles whose Node extname is "") → allowed.
        for p in ["Makefile", ".env", "README"] {
            assert!(
                is_text_include_extension(Path::new(p)),
                "{p} (no extname) should pass"
            );
        }
        // Non-text extensions → skipped.
        for p in [
            "img.png", "blob.gz", "a.tar.gz", "doc.pdf", "lib.so", "x.bin",
        ] {
            assert!(
                !is_text_include_extension(Path::new(p)),
                "{p} should be skipped as non-text"
            );
        }
    }

    #[test]
    fn loads_small_file_into_loadedfile() {
        let tmp = TempDir::new().unwrap();
        let p = tmp.path().join("LINGXI.md");
        fs::write(&p, b"# notes\nhello\n").unwrap();
        let out = load_file(&p, None).unwrap();
        assert_eq!(out.path, p);
        assert!(out.body.contains("hello"));
        assert_eq!(out.size_bytes, 14);
    }

    fn native_utf8_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../tests/fixtures/memory_utf8_2_1_286.json"
        ))
        .expect("hash-pinned native UTF-8 fixture")
    }

    #[test]
    fn native_utf8_byte_cases_load_with_raw_size() {
        let fixture = native_utf8_fixture();
        assert_eq!(fixture["native_version"], "2.1.286");
        assert_eq!(fixture["byte_limit"], MEMORY_FILE_BYTE_LIMIT);
        let cases = fixture["byte_cases"].as_array().unwrap();
        assert_eq!(cases.len(), 20);
        let tmp = TempDir::new().unwrap();
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let bytes: Vec<u8> = case["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
                .collect();
            let path = tmp.path().join(format!("{name}.md"));
            fs::write(&path, &bytes).unwrap();
            let loaded = load_file(&path, None).unwrap();
            let expected = case["l0_body"].as_str().unwrap();
            assert_eq!(loaded.body, expected, "native l0: {name}");
            assert_eq!(case["backend_body"], expected, "native QDn: {name}");
            assert_eq!(case["hot_body"], expected, "native Hot file: {name}");
            assert_eq!(
                case["hot_backend_body"], expected,
                "native Hot backend: {name}"
            );
            assert_eq!(loaded.size_bytes, bytes.len() as u64, "raw size: {name}");
            assert_eq!(case["raw_size_bytes"], loaded.size_bytes);
            assert_eq!(case["decoded_utf8_bytes"], loaded.body.len() as u64);
        }
    }

    #[test]
    fn malformed_utf8_parent_and_child_still_expand_imports() {
        let fixture = native_utf8_fixture();
        let cases = fixture["byte_cases"].as_array().unwrap();
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path();
        let mut expected = Vec::new();
        for (case_name, file_name) in [
            ("invalid-parent-with-import", "LINGXI.md"),
            ("invalid-child-with-import", "child.md"),
        ] {
            let case = cases.iter().find(|case| case["name"] == case_name).unwrap();
            let bytes: Vec<u8> = case["bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
                .collect();
            fs::write(cwd.join(file_name), bytes).unwrap();
            expected.push(case["l0_body"].as_str().unwrap());
        }
        fs::write(cwd.join("grandchild.md"), "grandchild\n").unwrap();
        expected.push("grandchild\n");
        let mut processed = HashSet::new();
        let entries = expand_memory_file(
            &cwd.join("LINGXI.md"),
            &mut processed,
            false,
            cwd,
            Some(cwd),
            0,
        );
        assert_eq!(
            entries.len(),
            3,
            "malformed bytes must not drop either file"
        );
        for ((entry, file_name), body) in entries
            .iter()
            .zip(["LINGXI.md", "child.md", "grandchild.md"])
            .zip(expected)
        {
            assert_eq!(entry.path, cwd.join(file_name));
            assert_eq!(entry.body, body);
            assert_eq!(entry.raw_content, body);
            assert!(!entry.content_differs_from_disk);
        }
    }

    #[test]
    fn byte_limit_applies_before_utf8_replacement() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("LINGXI.md");
        let mut bytes = vec![b'x'; MEMORY_FILE_BYTE_LIMIT as usize];
        *bytes.last_mut().unwrap() = 0xff;
        fs::write(&path, &bytes).unwrap();
        let loaded = load_file(&path, None).expect("native l0 allows stat.size == T7");
        assert_eq!(loaded.size_bytes, MEMORY_FILE_BYTE_LIMIT);
        assert_eq!(loaded.body.len() as u64, MEMORY_FILE_BYTE_LIMIT + 2);
        assert!(loaded.body.ends_with('\u{fffd}'));
    }
    /// SUPERSEDED by `file_over_the_byte_limit_is_skipped`.
    ///
    /// This asserted an 11 MiB file loads, which followed from a module doc
    /// claiming claude-code applies no size check. That doc cited leaked TS;
    /// the binary skips anything over `ELu = 4194304`. The assertion is
    /// inverted rather than deleted so the correction stays visible.
    #[test]
    fn oversized_file_is_skipped_not_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("LINGXI.md");
        std::fs::write(&p, "x".repeat(11 * 1024 * 1024)).unwrap();
        assert!(
            load_file(&p, None).is_err(),
            "an 11 MiB memory file is over ELu and must be skipped"
        );
    }
    /// A memory file over the oracle's byte limit is SKIPPED, not read.
    ///
    /// `Eds` (@230805636) routes every memory file through `EG` (@229022173):
    /// `let o=await e.stat(t); if(!o.isFile()||o.size>r) return n?.(o),null;`
    /// with `r = ELu = 4194304` (@230811638). The old expectation here — that
    /// an 11 MiB file loads — came from a module doc that cited leaked TS and
    /// claimed claude applies no size check. The binary says otherwise.
    #[test]
    fn file_over_the_byte_limit_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("LINGXI.md");
        std::fs::write(&p, "x".repeat(MEMORY_FILE_BYTE_LIMIT as usize + 1)).unwrap();
        match load_file(&p, None) {
            Err(LoaderError::FileTooLarge { bytes, .. }) => {
                assert_eq!(bytes, MEMORY_FILE_BYTE_LIMIT + 1);
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }

    /// EXACTLY at the limit still loads — the oracle's test is `size > r`.
    #[test]
    fn file_at_the_byte_limit_still_loads() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("LINGXI.md");
        std::fs::write(&p, "x".repeat(MEMORY_FILE_BYTE_LIMIT as usize)).unwrap();
        let out = load_file(&p, None).expect("a file at the limit is not over it");
        assert_eq!(out.size_bytes, MEMORY_FILE_BYTE_LIMIT);
    }

    /// The bundled `agents-md` Mod's nested Read frame expands the files named
    /// by `AGENTS_NAMES` without applying the LINGXI/CLAUDE memory-file cap.
    /// Keep the exception at the selected root: normal memory files and
    /// `@include` children still pass through `load_file`'s 4 MiB guard.
    #[test]
    fn agents_md_ancestor_expansion_bypasses_only_the_selected_file_size_cap() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path();
        let oversized = "x".repeat(MEMORY_FILE_BYTE_LIMIT as usize + 1);
        let agents_paths = [
            cwd.join("AGENTS.md"),
            cwd.join(branding::DOT_DIR).join("AGENTS.md"),
        ];

        for path in &agents_paths {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, &oversized).unwrap();

            // This is the same expansion entry point used by
            // `$.fs.ancestors`, not a test of the private byte-limit helper.
            let mut processed = HashSet::new();
            let expanded = expand_memory_file_with_excluder(
                path,
                &mut processed,
                false,
                cwd,
                None,
                0,
                LingxiMdTier::Project,
                None,
            );
            assert_eq!(
                expanded.len(),
                1,
                "selected AGENTS file should load: {path:?}"
            );
            assert_eq!(
                expanded[0].raw_content.len() as u64,
                MEMORY_FILE_BYTE_LIMIT + 1
            );
        }

        let lingxi = cwd.join("LINGXI.md");
        std::fs::write(&lingxi, &oversized).unwrap();
        let mut processed = HashSet::new();
        let expanded = expand_memory_file_with_excluder(
            &lingxi,
            &mut processed,
            false,
            cwd,
            None,
            0,
            LingxiMdTier::Project,
            None,
        );
        assert!(expanded.is_empty(), "ordinary LINGXI.md remains capped");

        let agents_root = cwd.join("AGENTS.md");
        let oversized_import = cwd.join("large-import.md");
        std::fs::write(&agents_root, format!("@./large-import.md\n{}", oversized)).unwrap();
        std::fs::write(&oversized_import, &oversized).unwrap();
        let mut processed = HashSet::new();
        let expanded = expand_memory_file_with_excluder(
            &agents_root,
            &mut processed,
            false,
            cwd,
            None,
            0,
            LingxiMdTier::Project,
            None,
        );
        assert_eq!(expanded.len(), 1, "oversized import must be skipped");
        assert_eq!(expanded[0].path, agents_root);
    }

    /// A directory is skipped by the `!o.isFile()` half of the same guard.
    #[test]
    fn non_regular_file_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sub = dir.path().join("LINGXI.md");
        std::fs::create_dir(&sub).unwrap();
        assert!(
            load_file(&sub, None).is_err(),
            "a directory must not be read as a memory file"
        );
    }

    /// The skip line is byte-verbatim to `Eds`'s template
    /// (`[CLAUDE.md] skipping ${e}: not a regular file or exceeds ${ELu} byte
    /// limit`) with the file NAME rebranded, the same split
    /// `format_large_memory_file_status_row` already applies to "Large
    /// CLAUDE.md will impact performance".
    #[test]
    fn skip_log_line_matches_the_oracle_template() {
        assert_eq!(
            skip_log_line(Path::new("/w/LINGXI.md")),
            "[LINGXI.md] skipping /w/LINGXI.md: not a regular file or exceeds 4194304 byte limit"
        );
    }

    /// `!CLu` — the report fires at most once per process, however many files
    /// are skipped.
    #[test]
    fn the_skip_report_fires_only_once() {
        let slot = AtomicBool::new(false);
        assert!(
            take_skip_report_slot(&slot, false),
            "the first skip reports"
        );
        assert!(
            !take_skip_report_slot(&slot, false),
            "a later skip is silent"
        );
    }

    /// `!o` — a DIRECTORY is logged but never reported, AND must not burn the
    /// one-shot slot: the oracle only assigns `CLu` inside `if(!CLu && !o)`, so
    /// a directory seen first still leaves the report available for a genuine
    /// oversize skip later.
    #[test]
    fn a_directory_skip_is_never_reported_and_keeps_the_slot() {
        let slot = AtomicBool::new(false);
        assert!(
            !take_skip_report_slot(&slot, true),
            "a directory is not reported"
        );
        assert!(
            take_skip_report_slot(&slot, false),
            "the slot survived for a real oversize skip"
        );
    }

    /// The two halves of `!o.isFile() || o.size > r` are distinguishable,
    /// because only the non-directory half is reported.
    #[test]
    fn the_skip_error_distinguishes_a_directory_from_an_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let as_dir = dir.path().join("LINGXI.md");
        std::fs::create_dir(&as_dir).unwrap();
        match load_file(&as_dir, None) {
            Err(LoaderError::FileTooLarge { is_directory, .. }) => {
                assert!(is_directory, "a directory must be flagged as one");
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }

        let big = dir.path().join("big.md");
        std::fs::write(&big, "x".repeat(MEMORY_FILE_BYTE_LIMIT as usize + 1)).unwrap();
        match load_file(&big, None) {
            Err(LoaderError::FileTooLarge { is_directory, .. }) => {
                assert!(
                    !is_directory,
                    "an oversized regular file is not a directory"
                );
            }
            other => panic!("expected FileTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn get_large_memory_files_flags_but_does_not_drop_40k_body() {
        use crate::{get_large_memory_files, MemoryFile, MemoryFrontmatter};
        use std::time::SystemTime;
        // A >40k-char body is flagged by the warning helper but still fully
        // present (mirror claudemd.ts:1132-1134 getLargeMemoryFiles).
        let mk = |path: &str, content: String| MemoryFile {
            path: PathBuf::from(path),
            mtime: SystemTime::UNIX_EPOCH,
            frontmatter: MemoryFrontmatter::default(),
            content,
        };
        let big = mk(
            "/x/LINGXI.md",
            "x".repeat(crate::MAX_MEMORY_CHARACTER_COUNT + 1),
        );
        let small = mk("/y/LINGXI.md", "small".to_string());
        let files = vec![big.clone(), small];
        let large = get_large_memory_files(&files, crate::MAX_MEMORY_CHARACTER_COUNT);
        assert_eq!(large.len(), 1, "only the >40k file is flagged");
        assert_eq!(large[0].path, big.path);
        // Body is NOT truncated — still fully loaded.
        assert_eq!(
            large[0].content.chars().count(),
            crate::MAX_MEMORY_CHARACTER_COUNT + 1
        );
    }

    #[tokio::test]
    async fn emit_file_too_large_writes_event_with_size() {
        use std::sync::{Arc, Mutex};
        use telemetry::{sink::LogEventMetadata, AnalyticsBus, AnalyticsSink, AnalyticsValue};

        struct Cap {
            events: Mutex<Vec<(String, LogEventMetadata)>>,
        }
        #[async_trait::async_trait]
        impl AnalyticsSink for Cap {
            async fn log_event(&self, n: &str, m: LogEventMetadata) {
                self.events.lock().unwrap().push((n.into(), m));
            }
            async fn log_event_async(&self, n: &str, m: LogEventMetadata) {
                self.log_event(n, m).await;
            }
            fn name(&self) -> &str {
                "cap"
            }
        }

        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(Cap {
            events: Mutex::new(Vec::new()),
        });
        bus.attach_sink(sink.clone()).await;
        emit_file_too_large(
            Some(&bus),
            std::path::Path::new("/x/LINGXI.md"),
            11 * 1024 * 1024,
        )
        .await;
        let ev = sink.events.lock().unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].0, "tengu_memory_file_too_large");
        matches!(ev[0].1.get("size_bytes"), Some(AnalyticsValue::Int(_)));
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    fn bodies(entries: &[MemoryEntry]) -> Vec<String> {
        entries.iter().map(|e| e.body.trim().to_string()).collect()
    }

    #[test]
    fn imports_relative_tilde_and_abs_spliced_as_entries() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        let home = tmp.path().join("home");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();

        // `@./rel.md` lives under cwd; `@~/tilde.md` under home; the abs
        // import lives at the tmp root (outside cwd).
        fs::write(cwd.join("rel.md"), "REL").unwrap();
        fs::write(home.join("tilde.md"), "TILDE").unwrap();
        let abs = tmp.path().join("abs.md");
        fs::write(&abs, "ABS").unwrap();

        let main = cwd.join("LINGXI.md");
        fs::write(
            &main,
            format!("main notes\n@./rel.md\n@~/tilde.md\n@{}\n", abs.display()),
        )
        .unwrap();

        let mut processed = HashSet::new();
        // include_external=true so the home/abs imports are not gated out.
        let entries = expand_memory_file(&main, &mut processed, true, &cwd, Some(&home), 0);

        // Parent FIRST, then children in directive order.
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].path, main, "parent must come before children");
        assert_eq!(entries[0].parent, None);
        assert!(entries[1..]
            .iter()
            .all(|entry| entry.parent.as_deref() == Some(main.as_path())));
        assert!(entries[0].body.contains("main notes"));
        let child_bodies: Vec<String> = bodies(&entries[1..]);
        assert_eq!(child_bodies, vec!["REL", "TILDE", "ABS"]);
    }

    #[test]
    fn nested_imports_keep_their_direct_parent() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("LINGXI.md");
        let child = temp.path().join("child.md");
        let grandchild = temp.path().join("grandchild.md");
        fs::write(&root, "root\n@./child.md\n").unwrap();
        fs::write(&child, "child\n@./grandchild.md\n").unwrap();
        fs::write(&grandchild, "grandchild\n").unwrap();
        let mut processed = HashSet::new();
        let entries = expand_memory_file(
            &root,
            &mut processed,
            true,
            temp.path(),
            Some(temp.path()),
            0,
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].parent, None);
        assert_eq!(entries[1].parent.as_deref(), Some(root.as_path()));
        assert_eq!(entries[2].parent.as_deref(), Some(child.as_path()));
    }

    #[test]
    fn cycle_guard_terminates_mutual_imports() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("a.md"), "A\n@./b.md\n").unwrap();
        fs::write(dir.join("b.md"), "B\n@./a.md\n").unwrap();

        let mut processed = HashSet::new();
        let entries =
            expand_memory_file(&dir.join("a.md"), &mut processed, true, dir, Some(dir), 0);

        // a -> b -> (a already processed, skipped). Exactly two entries.
        assert_eq!(entries.len(), 2);
        assert!(entries[0].body.contains('A'));
        assert!(entries[1].body.contains('B'));
    }

    #[test]
    fn depth_cap_stops_at_five() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        // f0 -> f1 -> ... -> f6 (each imports the next).
        for n in 0..=6 {
            let body = if n < 6 {
                format!("F{n}\n@./f{}.md\n", n + 1)
            } else {
                format!("F{n}\n")
            };
            fs::write(dir.join(format!("f{n}.md")), body).unwrap();
        }

        let mut processed = HashSet::new();
        let entries =
            expand_memory_file(&dir.join("f0.md"), &mut processed, true, dir, Some(dir), 0);

        // Depths 0..=4 are processed (5 files); f5 would be depth 5 -> rejected.
        // The body keeps the `@import` directive text (parity: TS only strips
        // frontmatter + comments), so check the leading marker per entry.
        assert_eq!(entries.len(), MAX_INCLUDE_DEPTH);
        for (n, e) in entries.iter().enumerate() {
            assert!(e.body.trim_start().starts_with(&format!("F{n}")));
        }
        assert!(
            !bodies(&entries)
                .iter()
                .any(|b| b.contains("F5") || b.contains("F6")),
            "files past the depth cap must not be spliced"
        );
    }

    #[test]
    fn missing_import_is_silently_ignored() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        let main = dir.join("LINGXI.md");
        fs::write(&main, "hello\n@./does-not-exist.md\n").unwrap();

        let mut processed = HashSet::new();
        let entries = expand_memory_file(&main, &mut processed, true, dir, Some(dir), 0);

        // Only the parent — the missing include is dropped without error.
        assert_eq!(entries.len(), 1);
        assert!(entries[0].body.contains("hello"));
    }

    #[test]
    fn external_import_gated_unless_allowed() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let outside = tmp.path().join("outside.md");
        fs::write(&outside, "OUTSIDE").unwrap();
        let main = cwd.join("LINGXI.md");
        fs::write(&main, format!("root\n@{}\n", outside.display())).unwrap();

        // Gated out when external includes are disallowed.
        let mut p1 = HashSet::new();
        let gated = expand_memory_file(&main, &mut p1, false, &cwd, Some(&cwd), 0);
        assert_eq!(gated.len(), 1, "external include must be skipped");

        // Allowed when include_external=true.
        let mut p2 = HashSet::new();
        let allowed = expand_memory_file(&main, &mut p2, true, &cwd, Some(&cwd), 0);
        assert_eq!(allowed.len(), 2);
        assert!(allowed[1].body.contains("OUTSIDE"));
    }

    #[test]
    fn external_import_discovery_follows_internal_files_without_opening_external_targets() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(cwd.join("rules")).unwrap();
        let outside = tmp.path().join("outside.md");
        let nested = cwd.join("rules/nested.md");
        fs::write(
            cwd.join("LINGXI.md"),
            "root\n@./rules/nested.md\n@../outside.md\n",
        )
        .unwrap();
        fs::write(&nested, "nested\n@../../outside.md\n").unwrap();
        // Deliberately do not create `outside.md`: discovery reports the target
        // from syntax and must not require opening it before approval.

        let found = discover_external_include_paths(&cwd.join("LINGXI.md"), &cwd, Some(tmp.path()));
        assert_eq!(found, vec![outside]);
    }

    #[cfg(unix)]
    #[test]
    fn project_symlink_to_external_import_still_requires_approval() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        let outside_dir = tmp.path().join("outside");
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        let outside = outside_dir.join("policy.md");
        fs::write(&outside, "EXTERNAL").unwrap();
        symlink(&outside_dir, cwd.join("linked")).unwrap();
        fs::write(cwd.join("LINGXI.md"), "root\n@./linked/policy.md\n").unwrap();

        let found = discover_external_include_paths(&cwd.join("LINGXI.md"), &cwd, Some(tmp.path()));
        assert_eq!(found, vec![outside.canonicalize().unwrap()]);

        let mut processed = HashSet::new();
        let gated = expand_memory_file(
            &cwd.join("LINGXI.md"),
            &mut processed,
            false,
            &cwd,
            Some(tmp.path()),
            0,
        );
        assert_eq!(gated.len(), 1, "symlink escape must stay gated");
    }

    #[test]
    fn exclude_aware_discovery_skips_excluded_internal_parent_subtree() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        let nested_dir = cwd.join("secret");
        fs::create_dir_all(&nested_dir).unwrap();
        let outside = tmp.path().join("outside.md");
        fs::write(cwd.join("LINGXI.md"), "root\n@./secret/LINGXI.md\n").unwrap();
        fs::write(nested_dir.join("LINGXI.md"), "@../../outside.md\n").unwrap();

        let excluder = LingxiMdExcluder::new(&["**/secret/LINGXI.md".to_string()]);
        let found = discover_external_include_paths_with_excluder(
            &cwd.join("LINGXI.md"),
            &cwd,
            Some(tmp.path()),
            LingxiMdTier::Project,
            Some(&excluder),
        );

        assert!(
            found.is_empty(),
            "excluded parents must not leak external imports"
        );
        assert!(
            !outside.exists(),
            "preflight must not require the external target to exist"
        );
    }

    #[test]
    fn exclude_aware_discovery_skips_directly_excluded_external_target() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let outside = tmp.path().join("private.md");
        fs::write(&outside, "PRIVATE").unwrap();
        fs::write(
            cwd.join("LINGXI.md"),
            format!("root\n@{}\n", outside.display()),
        )
        .unwrap();

        let excluder = LingxiMdExcluder::new(&[outside.to_string_lossy().into_owned()]);
        let found = discover_external_include_paths_with_excluder(
            &cwd.join("LINGXI.md"),
            &cwd,
            Some(tmp.path()),
            LingxiMdTier::Project,
            Some(&excluder),
        );

        assert!(
            found.is_empty(),
            "excluded external targets must not be exposed"
        );
    }

    #[test]
    fn exclude_aware_expand_skips_excluded_import_subtree() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        let nested_dir = cwd.join("secret");
        fs::create_dir_all(&nested_dir).unwrap();
        fs::write(
            cwd.join("LINGXI.md"),
            "root\n@./secret/LINGXI.md\n@./allowed.md\n",
        )
        .unwrap();
        fs::write(nested_dir.join("LINGXI.md"), "secret\n@./child.md\n").unwrap();
        fs::write(nested_dir.join("child.md"), "child").unwrap();
        fs::write(cwd.join("allowed.md"), "allowed").unwrap();

        let excluder = LingxiMdExcluder::new(&["**/secret/LINGXI.md".to_string()]);
        let mut processed = HashSet::new();
        let entries = expand_memory_file_with_excluder(
            &cwd.join("LINGXI.md"),
            &mut processed,
            true,
            &cwd,
            Some(tmp.path()),
            0,
            LingxiMdTier::Project,
            Some(&excluder),
        );

        assert_eq!(
            bodies(&entries),
            vec!["root\n@./secret/LINGXI.md\n@./allowed.md", "allowed"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_to_an_ancestor_is_not_loaded_twice() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        let root = cwd.join("LINGXI.md");
        let alias = cwd.join("alias.md");
        fs::write(&root, "root\n@./alias.md\n").unwrap();
        symlink(&root, &alias).unwrap();

        let mut processed = HashSet::new();
        let entries = expand_memory_file(&root, &mut processed, true, &cwd, Some(tmp.path()), 0);

        assert_eq!(entries.len(), 1, "symlink alias must share one identity");
        assert_eq!(entries[0].path, root);
    }

    #[test]
    fn frontmatter_block_is_stripped_from_body() {
        let raw = "---\ntitle: secret\npaths: src/**\n---\nVISIBLE BODY\n";
        let parsed = parse_memory_content(raw, Path::new("/x/LINGXI.md"), None);
        assert_eq!(parsed.body, "VISIBLE BODY\n");
        assert!(!parsed.body.contains("title"));
        // GAP 2 part-1: the `paths:` glob is captured (trailing `/**` stripped),
        // marking this a CONDITIONAL rule.
        assert_eq!(parsed.globs, Some(vec!["src".to_string()]));
    }

    #[test]
    fn frontmatter_without_paths_yields_no_globs() {
        // An unconditional file (no `paths:`) has `globs == None`.
        let raw = "---\ntitle: t\n---\nBODY\n";
        let parsed = parse_memory_content(raw, Path::new("/x/LINGXI.md"), None);
        assert_eq!(parsed.globs, None);
        // And a match-all `paths: **` is also treated as unconditional.
        let all = "---\npaths: '**'\n---\nBODY\n";
        assert_eq!(
            parse_memory_content(all, Path::new("/x/LINGXI.md"), None).globs,
            None
        );
    }

    #[test]
    fn frontmatter_paths_expand_nested_brace_groups_in_order() {
        let raw = "---\npaths: '{src,test}/{api,ui}/**'\n---\nBODY\n";
        assert_eq!(
            parse_frontmatter_paths(raw),
            Some(vec![
                "src/api".to_string(),
                "src/ui".to_string(),
                "test/api".to_string(),
                "test/ui".to_string(),
            ])
        );
    }

    #[test]
    fn brace_expansion_returns_unexpanded_when_over_budget() {
        // A small budget → the pattern is returned UNEXPANDED (a warn is logged),
        // NOT truncated to the first N (oracle `uCg` returns `[e]` on exceed).
        let mut budget = BraceBudget {
            results: 3,
            bytes: 4_194_304,
        };
        assert_eq!(
            expand_braces_budgeted("{a,b,c,d}/{x,y}", &mut budget),
            vec!["{a,b,c,d}/{x,y}"]
        );
        // Under a generous budget the same pattern fully expands.
        let mut ok = BraceBudget::default();
        assert_eq!(
            expand_braces_budgeted("{a,b,c,d}/{x,y}", &mut ok),
            vec!["a/x", "a/y", "b/x", "b/y", "c/x", "c/y", "d/x", "d/y"]
        );
    }

    #[test]
    fn brace_expansion_deep_nesting_trips_the_byte_budget() {
        // A ~12KB deeply-nested pattern trips the 4MiB byte budget before it can
        // collapse, so it is returned UNEXPANDED — the DoS is bounded.
        let depth = 4_096;
        let nested = format!("root/{}", "{a/".repeat(depth) + "leaf" + &"}".repeat(depth));
        let mut budget = BraceBudget::default();
        assert_eq!(
            expand_braces_budgeted(&nested, &mut budget),
            vec![nested.clone()]
        );
    }

    #[test]
    fn brace_expansion_matches_upstream_first_close_nesting() {
        // First-CLOSE-brace semantics (oracle `\{([^}]+)\}`): a nested group's
        // first `}` closes the match, leaving the trailing `}` in the suffix.
        let mut b1 = BraceBudget::default();
        assert_eq!(
            expand_braces_budgeted("{a,{b,c}}", &mut b1),
            vec!["a}", "b", "c}"]
        );
        let mut b2 = BraceBudget::default();
        assert_eq!(
            expand_braces_budgeted("root/{a,{b,c}}/{x,y}", &mut b2),
            vec![
                "root/a}/x",
                "root/a}/y",
                "root/b/x",
                "root/b/y",
                "root/c}/x",
                "root/c}/y"
            ]
        );
    }

    #[test]
    fn block_html_comment_stripped_but_code_fence_preserved() {
        let content = "\
before
<!-- authorial note, should vanish -->
after
```
<!-- fenced comment must survive -->
```
done
";
        let stripped = strip_html_comments(content);
        assert!(
            !stripped.contains("authorial note"),
            "block comment must be stripped: {stripped:?}"
        );
        assert!(stripped.contains("before"));
        assert!(stripped.contains("after"));
        assert!(
            stripped.contains("fenced comment must survive"),
            "code-fenced comment must be preserved: {stripped:?}"
        );
    }

    #[test]
    fn comment_residue_after_close_is_kept() {
        // claudemd.ts:320-326 — text after `-->` on the same line is kept.
        let content = "<!-- note --> @./keep.md\n";
        let stripped = strip_html_comments(content);
        assert!(stripped.contains("@./keep.md"));
        assert!(!stripped.contains("note"));
    }

    #[test]
    fn import_inside_fence_or_inline_code_is_not_extracted() {
        let content = "\
@./real.md
```
@./fenced.md
```
inline `@./inline.md` here
";
        let paths = extract_include_paths(content, Path::new("/base"), None);
        assert_eq!(paths, vec![PathBuf::from("/base/real.md")]);
    }

    #[test]
    fn import_inside_indented_code_block_is_not_extracted() {
        let content = "    @./four-space.md\n\t@./tab.md\n@./real.md\n";
        let paths = extract_include_paths(content, Path::new("/base"), None);
        assert_eq!(paths, vec![PathBuf::from("/base/real.md")]);
    }

    #[test]
    fn expand_path_handles_tilde_abs_and_relative() {
        let home = Path::new("/home/u");
        let base = Path::new("/proj/sub");
        assert_eq!(
            expand_path("~/notes.md", base, Some(home)),
            Some(PathBuf::from("/home/u/notes.md"))
        );
        assert_eq!(
            expand_path("/abs/x.md", base, Some(home)),
            Some(PathBuf::from("/abs/x.md"))
        );
        assert_eq!(
            expand_path("./rel.md", base, Some(home)),
            Some(PathBuf::from("/proj/sub/rel.md"))
        );
        assert_eq!(
            expand_path("bare.md", base, Some(home)),
            Some(PathBuf::from("/proj/sub/bare.md"))
        );
        // `~` with no home resolves to nothing.
        assert_eq!(expand_path("~/x.md", base, None), None);
    }
}
