//! Memory-section formatter (claude-code `getLingxiMds`) +
//! `MemoryHierarchyProvider` trait.
//!
//! The trait abstracts the M3-02 `lingxi_md::walk` + `load_file` pair
//! so the orchestrator can take a `Arc<dyn MemoryHierarchyProvider>`
//! field and tests can substitute a static fixture without touching
//! the filesystem. Production impl: [`RealMemoryHierarchyProvider`].
//! The formatter ([`format`]) emits the preamble + per-file
//! `Contents of …:` blocks (GAP 3 — no enclosing tag, no trailing newline).
#![forbid(unsafe_code)]

use crate::prompt::MemoryFile;
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

/// Loads the LINGXI.md hierarchy for a given cwd.
///
/// Implementations MUST return the files in claude-code splice order:
/// the Managed tier first (`<managed>/LINGXI.md` + rules), then User
/// (`~/.lingxi/LINGXI.md` + rules), then Project (`<repo>/LINGXI.md`, …),
/// then Local (`<repo>/LINGXI.local.md`) — innermost last so it wins the
/// model's recency attention.
///
/// §F: the returned vec contains BOTH unconditional files (`globs == None`)
/// AND conditional rules (`globs == Some(_)`, carrying `paths:` globs). The
/// caller decides what to do with each: [`format`] filters to `globs.is_none()`
/// for the eager system-prompt block, while the orchestrator routes the
/// conditional rules to per-edited-file lazy activation (claudemd.ts
/// `processConditionedMdRules`).
#[async_trait]
pub trait MemoryHierarchyProvider: Send + Sync {
    /// Load all LINGXI.md files relevant to `cwd`. May be empty.
    ///
    /// Errors are NOT propagated — unreadable files are skipped silently.
    ///
    /// CORRECTED: this used to credit `loader::emit_file_too_large`, which has
    /// no production caller and fires an event absent from the binary. A file
    /// skipped by the loader's 4 MiB / non-regular stat guard is logged and
    /// reported by `loader::report_skipped_memory_file`; a file that fails to
    /// read (ENOENT, perms, non-UTF-8) really is silent here, matching the
    /// oracle, whose `read_eacces` / `read_failed` reports are a separate
    /// unported family.
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile>;

    /// Compiled `claudeMdExcludes` matcher shared with lazy/nested loaders.
    /// Static and custom providers default to no exclusions.
    fn excluder(&self) -> Option<memory::lingxi_md::LingxiMdExcluder> {
        None
    }
}

/// Production implementation — wraps `memory::lingxi_md::walk` +
/// `expand_memory_file`. Reverses the walk order so the returned vec is in
/// claude-code splice order (managed → home → repo → local-override),
/// recursively splices each file's `@import` references directly after it
/// (parity with claude-code `processMemoryFile`), and tags each file with its
/// [`memory::lingxi_md::LingxiMdTier`]. Conditional (`paths:`-gated) rules are
/// returned WITH their globs intact (§F) — the eager-vs-lazy split is the
/// caller's responsibility (see [`format`] / the orchestrator's
/// `conditional_rules_reminder_message`).
pub struct RealMemoryHierarchyProvider;

fn load_memory_files(
    cwd: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
) -> Vec<MemoryFile> {
    // LINGXI_DISABLE_LINGXI_MDS (binary `yOe` @208938221:
    // `je.LINGXI_DISABLE_LINGXI_MDS ? [] : await Mv()`). A plain truthy
    // env check — ANY non-empty value (incl. "0") disables all LINGXI.md
    // loading; safe-mode sets it to "1".
    if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let managed = memory::lingxi_md::hierarchy::managed_path();
    let h = memory::lingxi_md::hierarchy::walk(cwd, &home, Some(&managed));
    let mut entries = h.entries;
    entries.reverse();

    let mut processed: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();
    let external_includes_approved =
        migrations::global_config::global_config_path().is_some_and(|path| {
            migrations::global_config::check_has_lingxi_md_external_includes_approved(&path, cwd)
        });
    let mut out = Vec::new();
    for e in entries {
        let include_external = include_external_for(e.tier, external_includes_approved);
        let expanded = memory::lingxi_md::loader::expand_memory_file_with_excluder(
            &e.path,
            &mut processed,
            include_external,
            cwd,
            Some(&home),
            0,
            e.tier,
            excluder,
        );
        for (idx, entry) in expanded.into_iter().enumerate() {
            let body = entry.body.trim().to_string();
            if body.is_empty() {
                continue;
            }
            out.push(MemoryFile {
                path: entry.path,
                body,
                is_local_override: idx == 0 && e.is_local_override,
                tier: e.tier,
                globs: entry.globs,
                raw_content: entry.raw_content,
                content_differs_from_disk: entry.content_differs_from_disk,
            });
        }
    }
    out
}

/// Discover external `@import` targets that would require project approval.
///
/// User-tier memory is intentionally excluded because Claude Code always
/// allows its imports. Managed, project, and local files are scanned without
/// opening any target outside `cwd`; the returned paths are de-duplicated in
/// hierarchy order for the startup warning.
#[must_use]
pub fn pending_external_include_paths(cwd: &Path) -> Vec<std::path::PathBuf> {
    pending_external_include_paths_with_excluder(cwd, None)
}

/// Exclude-aware variant of [`pending_external_include_paths`].
#[must_use]
pub fn pending_external_include_paths_with_excluder(
    cwd: &Path,
    excluder: Option<&memory::lingxi_md::LingxiMdExcluder>,
) -> Vec<std::path::PathBuf> {
    if std::env::var_os("LINGXI_DISABLE_LINGXI_MDS").is_some_and(|v| !v.is_empty()) {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let managed = memory::lingxi_md::hierarchy::managed_path();
    let mut entries = memory::lingxi_md::hierarchy::walk(cwd, &home, Some(&managed)).entries;
    entries.reverse();

    let mut seen = std::collections::HashSet::new();
    let mut paths = Vec::new();
    for entry in entries {
        if matches!(entry.tier, memory::lingxi_md::LingxiMdTier::User) {
            continue;
        }
        for path in memory::lingxi_md::loader::discover_external_include_paths_with_excluder(
            &entry.path,
            cwd,
            Some(&home),
            entry.tier,
            excluder,
        ) {
            if seen.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    paths
}

#[async_trait]
impl MemoryHierarchyProvider for RealMemoryHierarchyProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        load_memory_files(cwd, None)
    }
}

/// Convenience constructor: returns an `Arc<dyn MemoryHierarchyProvider>`
/// wrapping a fresh [`RealMemoryHierarchyProvider`]. Used by the
/// production constructor of `ConversationOrchestrator`.
#[must_use]
pub fn real_provider() -> Arc<dyn MemoryHierarchyProvider> {
    Arc::new(RealMemoryHierarchyProvider)
}

/// Like [`real_provider`] but DROPS the `LINGXI.md` files whose path matches the
/// `claudeMdExcludes` settings patterns (claude-code `isLingxiMdExcluded` runs
/// inside `processMemoryFile`, so excluded User/Project/Local files never reach
/// the system prompt; Managed is never excludable). Returns the unfiltered
/// [`real_provider`] when no patterns are configured (byte-identical to before).
#[must_use]
pub fn real_provider_with_excludes(excludes: Vec<String>) -> Arc<dyn MemoryHierarchyProvider> {
    let matcher = memory::lingxi_md::LingxiMdExcluder::new(&excludes);
    if matcher.is_empty() {
        return real_provider();
    }
    Arc::new(ExcludeFilterProvider { excluder: matcher })
}

/// Wraps a [`MemoryHierarchyProvider`] and filters its result through the
/// `claudeMdExcludes` gate (see [`real_provider_with_excludes`]).
struct ExcludeFilterProvider {
    excluder: memory::lingxi_md::LingxiMdExcluder,
}

#[async_trait]
impl MemoryHierarchyProvider for ExcludeFilterProvider {
    async fn load(&self, cwd: &Path) -> Vec<MemoryFile> {
        load_memory_files(cwd, Some(&self.excluder))
    }

    fn excluder(&self) -> Option<memory::lingxi_md::LingxiMdExcluder> {
        Some(self.excluder.clone())
    }
}

/// claude-code external-`@import` gate (claudemd.ts:826-846): the User tier
/// always resolves external includes; every other tier (Managed/Project/Local)
/// does so ONLY when `hasLingxiMdExternalIncludesApproved` is set for the
/// project. (claude-code also has an internal `forceIncludeExternal`; LingXi has
/// no caller that sets it, so it is omitted.)
#[must_use]
fn include_external_for(tier: memory::lingxi_md::LingxiMdTier, approved: bool) -> bool {
    matches!(tier, memory::lingxi_md::LingxiMdTier::User) || approved
}

#[cfg(test)]
mod external_include_tests {
    use super::{
        include_external_for, pending_external_include_paths,
        pending_external_include_paths_with_excluder,
    };
    use memory::lingxi_md::LingxiMdExcluder;
    use memory::lingxi_md::LingxiMdTier::{Local, Managed, Project, User};

    #[test]
    fn user_tier_always_allows_external_others_only_when_approved() {
        // User tier: external `@import`s always allowed (unconditional).
        assert!(include_external_for(User, false));
        assert!(include_external_for(User, true));
        // Managed/Project/Local: gated on the per-project approval flag.
        for tier in [Managed, Project, Local] {
            assert!(
                !include_external_for(tier, false),
                "{tier:?} gated when unapproved"
            );
            assert!(
                include_external_for(tier, true),
                "{tier:?} allowed when approved"
            );
        }
    }

    #[test]
    fn pending_scan_reports_project_external_imports_once() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().join("project");
        std::fs::create_dir_all(&cwd).expect("mkdir project");
        let outside = root.path().join("shared.md");
        std::fs::write(
            cwd.join(branding::MEMORY_FILE),
            format!("@{}\n@{}\n", outside.display(), outside.display()),
        )
        .expect("write memory file");

        assert_eq!(pending_external_include_paths(&cwd), vec![outside]);
    }

    #[test]
    fn pending_scan_with_excluder_skips_excluded_subtree() {
        let root = tempfile::tempdir().expect("tempdir");
        let cwd = root.path().join("project");
        let nested = cwd.join("secret");
        std::fs::create_dir_all(&nested).expect("mkdir nested");
        let outside = root.path().join("shared.md");
        std::fs::write(cwd.join(branding::MEMORY_FILE), "@./secret/LINGXI.md\n")
            .expect("write root memory");
        std::fs::write(
            nested.join("LINGXI.md"),
            format!("@{}\n", outside.display()),
        )
        .expect("write nested memory");

        let excluder = LingxiMdExcluder::new(&["**/secret/LINGXI.md".to_string()]);
        assert_eq!(
            pending_external_include_paths_with_excluder(&cwd, Some(&excluder)),
            Vec::<std::path::PathBuf>::new()
        );
    }
}

#[cfg(test)]
mod exclude_filter_tests {
    use super::*;
    use memory::lingxi_md::LingxiMdTier;

    #[test]
    fn filter_drops_matching_user_project_local_keeps_managed() {
        let excluder = memory::lingxi_md::LingxiMdExcluder::new(&["**/LINGXI.md".to_string()]);
        assert!(!excluder.is_excluded(Path::new("/mgr/LINGXI.md"), LingxiMdTier::Managed));
        assert!(excluder.is_excluded(Path::new("/a/secret/LINGXI.md"), LingxiMdTier::Project));
        assert!(excluder.is_excluded(Path::new("/a/public/LINGXI.md"), LingxiMdTier::User));
    }

    #[test]
    fn empty_excludes_returns_unfiltered_provider() {
        // No patterns ⇒ the plain real_provider (no wrapper), byte-identical path.
        let _ = real_provider_with_excludes(vec![]);
    }
}

/// Build a memdir-backed memory prefetcher for the composition root — the P0.1
/// activation of the `relevant_memories` surfacing channel.
///
/// Wires the LLM memory selector (`side_query_client`, Haiku-class) over the
/// user memdir (`<home>/.lingxi/memdir`) so that, each turn,
/// [`MemoryPrefetch::start`](memory::prefetch::MemoryPrefetch::start) scans the
/// memdir, asks the selector which entries are relevant to the turn query, and
/// surfaces them through
/// [`ConversationOrchestrator::relevant_memory_reminder_messages`](crate::ConversationOrchestrator).
/// Hand the returned handle to
/// [`ConversationOrchestrator::with_memory_prefetch`](crate::ConversationOrchestrator).
///
/// Centralised here (not inlined at each composition root) so desktop / bridge /
/// mobile build the prefetch identically and the engine apps need no direct
/// dependency on the `memory` crate's internals.
///
/// GATING: the composition root decides whether to call this — claude-code keeps
/// the feature behind `tengu_moth_copse` (default OFF); the LingXi equivalent is
/// "is a prefetch wired at all". `team_memory.enabled` is left `false` here (the
/// user memdir only), matching the inert default until team memory is configured.
#[must_use]
pub fn build_memdir_prefetch(
    side_query_client: Arc<dyn sidequery::SideQueryClient>,
    runtime: Arc<dyn platform_api::RuntimeSpawner>,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Arc<memory::prefetch::MemoryPrefetch> {
    // MEM-2: the User tier is scoped to `cwd`'s project, so memories written
    // while working on one repository are not recalled in another.
    let roots = memory::memdir::memdir_path(home, cwd, false);
    let selector = Arc::new(memory::selector::MemorySelector::new(side_query_client));
    Arc::new(memory::prefetch::MemoryPrefetch::new(
        selector, runtime, roots,
    ))
}

/// Build a [`SessionMemoryHandle`](crate::SessionMemoryHandle) for the
/// composition root — the §6.5 standalone session-memory extractor + its forked
/// runner. Hand to [`ConversationOrchestrator::with_session_memory`](crate::ConversationOrchestrator);
/// the composition root gates the call (default OFF). `config_home` is the
/// resolved `$LINGXI_CONFIG_DIR ?? ~/.claude` dir (the write base — pass
/// [`user_config_dir`](memory::lingxi_md::user_config_dir)`(dirs::home_dir())`).
#[must_use]
pub fn build_session_memory_handle(
    side_query_client: Arc<dyn sidequery::SideQueryClient>,
    extraction_model: String,
    initialization_threshold: u32,
    update_threshold: u32,
    home: &std::path::Path,
    runtime: Arc<dyn platform_api::RuntimeSpawner>,
) -> Arc<crate::SessionMemoryHandle> {
    // Called only when the composition root is enabling the feature, so
    // `enabled = true`. The `$LINGXI_CONFIG_DIR`-aware config-home is the SAME
    // base the Session-tier memdir scan reads, so writes re-load next session.
    let config = memory::session_memory::SessionMemoryConfig {
        enabled: true,
        initialization_threshold,
        update_threshold,
        // The legacy threshold parameters are retained for compatibility,
        // while new composition roots use Claude's token/activity gates.
        minimum_message_tokens_to_init: 10_000,
        minimum_tokens_between_update: 5_000,
        tool_calls_between_updates: 3,
        extraction_model,
    };
    let config_home = memory::lingxi_md::user_config_dir(home);
    let runner = Arc::new(
        sidequery::ForkedAgentRunner::new()
            .with_side_query_client(side_query_client, config.extraction_model.clone()),
    );
    Arc::new(crate::SessionMemoryHandle {
        extractor: tokio::sync::Mutex::new(memory::session_memory::SessionMemoryExtractor::new(
            config,
        )),
        runner,
        config_home,
        runtime,
        in_flight: std::sync::atomic::AtomicBool::new(false),
        generation: std::sync::atomic::AtomicU64::new(0),
    })
}

/// [`build_memdir_prefetch`] for a composition root that has raw Anthropic
/// credentials + an HTTP transport but no pre-built [`sidequery::SideQueryClient`]
/// (e.g. the mobile host, which assembles a multi-provider `llm_runtime` rather
/// than the desktop's side-query client). Constructs a
/// [`sidequery::ProviderSideQueryClient`] over `(api_key, api_base, http)` —
/// the same Anthropic-first-party side-query path the desktop build uses — so
/// the engine app needs no direct `sidequery` dependency.
///
/// NOTE: the side query routes through the Anthropic first-party path with the
/// supplied `api_key`, independent of any multi-provider routing the host's main
/// turn client uses. With no usable `api_key` the side query fails and the
/// prefetch resolves to empty (inert) — never breaking a turn. Gating is the
/// caller's — see `memory::auto_memory_enabled` (default ON).
#[must_use]
pub fn build_memdir_prefetch_from_anthropic(
    api_key: impl Into<String>,
    api_base: Option<String>,
    http: Arc<dyn llm_runtime::Transport>,
    runtime: Arc<dyn platform_api::RuntimeSpawner>,
    home: &std::path::Path,
    cwd: &std::path::Path,
) -> Arc<memory::prefetch::MemoryPrefetch> {
    let client: Arc<dyn sidequery::SideQueryClient> = Arc::new(
        sidequery::ProviderSideQueryClient::new(api_key, api_base, http),
    );
    build_memdir_prefetch(client, runtime, home, cwd)
}

/// Verbatim preamble that precedes the memory blocks.
///
/// 1:1 with claude-code `MEMORY_INSTRUCTION_PROMPT` (claudemd.ts:89-90).
const MEMORY_INSTRUCTION_PROMPT: &str = "Codebase and user instructions are shown below. Be sure to adhere to these instructions. IMPORTANT: These instructions OVERRIDE any default behavior and you MUST follow them exactly as written.";

/// The per-file injection description for a given tier (claudemd.ts:1168-1186).
/// Includes the leading space, exactly as TS concatenates `${file.path}${description}`.
fn tier_description(tier: memory::lingxi_md::LingxiMdTier) -> &'static str {
    use memory::lingxi_md::LingxiMdTier;
    match tier {
        LingxiMdTier::Project => " (project instructions, checked into the codebase)",
        LingxiMdTier::Local => " (user's private project instructions, not checked in)",
        // Binary `getLingxiMds` (`nUt`) 5-way switch on `o.type`: Managed has its
        // OWN description; only the default (User) gets the global-instructions
        // wording. (Previously Managed was folded into the User arm — a
        // divergence whenever an org-managed LINGXI.md is loaded.)
        LingxiMdTier::Managed => " (organization-managed policy instructions)",
        LingxiMdTier::User => " (user's private global instructions for all projects)",
    }
}

/// Whether `f`'s content is actually RENDERED into the model's context by
/// [`format`] — i.e. the model has already seen this file's body.
///
/// This is LingXi's analog of claude-code `MLu` (2.1.220 @230809370):
///
/// ```text
/// function MLu(e){
///   if(e.type==="AutoMem"&&ARe())return!1;
///   if((e.type==="Project"||e.type==="Local")&&Ke("tengu_paper_halyard",!1))return!1;
///   if(!e.content)return!1;
///   return!0}
/// ```
///
/// which the seeding loop `xCt` (@245883373) uses as `seededFromContext: jn`.
///
/// # Deliberate structural divergence
/// The oracle HAND-WRITES `MLu` as a duplicate of the renderer's own drops
/// (`dfo`/`pfo`, @230812024) — two copies that can drift. LingXi instead makes
/// [`format`] call THIS function, so the predicate and the renderer are one
/// source of truth (pinned by
/// `orchestrator/tests/prompt_memory_block_test.rs::rendered_into_context_matches_format_output`).
///
/// Of the oracle's three drops, only the last has a LingXi analog:
/// - `ARe()` (memory-stores mode / `CLAUDE_MEMORY_STORES`) — Divergence(reason):
///   unported, there is no `AutoMem` tier in LingXi at all.
/// - `tengu_paper_halyard` — Divergence(reason): a GrowthBook gate; LingXi has
///   no GrowthBook.
/// - `!e.content` → `f.body.trim().is_empty()`.
///
/// The surviving LingXi-only drop is the CONDITIONAL-rule filter
/// (`globs.is_some()`).
///
/// CORRECTED: an earlier draft of this doc added "which lives in the renderer in
/// the oracle too". It does not. `pfo` (@230812024) drops on exactly three
/// things — a caller-supplied type filter, the `tengu_paper_halyard`
/// Project/Local gate, and empty content — and carries no conditional-rule arm.
/// `conditionalRule:!0` appears in `lfo`, the LAZY conditional loader, not in
/// the eager path. So this filter is a LingXi structural choice, and citing the
/// oracle for it would have made a divergence look like parity.
#[must_use]
pub fn is_rendered_into_context(f: &MemoryFile) -> bool {
    f.globs.is_none() && !f.body.trim().is_empty()
}

/// Format the memory section from a slice of loaded files, 1:1 with claude-code
/// `getLingxiMds` (claudemd.ts:1153-1195).
///
/// Shape (NO enclosing tag, NO trailing newline; DOUBLE-newline separators):
/// ```text
/// {MEMORY_INSTRUCTION_PROMPT}
///
/// Contents of {p1}{desc1}:
///
/// {body1}
///
/// Contents of {p2}{desc2}:
///
/// {body2}
/// ```
/// where `{descN}` is the tier description (project / local / global) and each
/// body is `.trim()`med. Blocks are joined by `"\n\n"` (binary `_9t` tail
/// `${$ip}\n\n${n.join(`\n\n`)}`, and each block is `…:\n\n${i}` — all DOUBLE
/// newlines, verified via `od -c` on the 2.1.195 binary; the `strings` dump
/// misled an earlier pass into single `\n`). When `files` is empty, returns the
/// EMPTY STRING and the caller MUST elide the section.
///
/// §F: CONDITIONAL rules (`globs.is_some()`) are filtered OUT here — only
/// unconditional files (`LINGXI.md` + non-`paths:` rules) are eagerly injected,
/// mirroring claude-code's `conditionalRule:false` eager filter (claudemd.ts:773).
/// Conditional rules are activated lazily per edited/opened file by the
/// orchestrator. If `files` contains ONLY conditional rules, this returns the
/// empty string (caller elides the section).
#[must_use]
pub fn format(files: &[MemoryFile]) -> String {
    let blocks: Vec<String> = files
        .iter()
        // §F: eager block = unconditional files only. Derived from
        // [`is_rendered_into_context`] so the seeding predicate cannot drift
        // from what the model actually receives.
        .filter(|f| is_rendered_into_context(f))
        .map(|f| {
            // Binary `_9t` (getLingxiMds): `Contents of ${o.path}${s}:\n\n${i}`
            // — DOUBLE `\n` between the header and the trimmed body (od -c
            // verified on 2.1.195).
            format!(
                "Contents of {}{}:\n\n{}",
                f.path.display(),
                tier_description(f.tier),
                f.body.trim()
            )
        })
        .collect();
    if blocks.is_empty() {
        return String::new();
    }
    // Binary `_9t` tail: `${$ip}\n\n${n.join(`\n\n`)}` — DOUBLE `\n` both for the
    // preamble→blocks separator and the block join (od -c verified on 2.1.195).
    format!("{MEMORY_INSTRUCTION_PROMPT}\n\n{}", blocks.join("\n\n"))
}
