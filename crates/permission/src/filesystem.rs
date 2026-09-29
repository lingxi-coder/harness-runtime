//! File-path glob matching for permission rules (enforcement phase 3a).
//!
//! Ports the path-matching core of claude-code
//! `utils/permissions/filesystem.ts` so that CONTENT rules targeting file
//! tools — `Edit(src/**)`, `Read(./secrets/**)`, `Write(/tmp/out/**)` — match
//! against the tool input's path instead of the whole tool (the phase-2
//! stand-in matched such rules tool-wide). The two functions reproduced here
//! are `patternWithRoot` (resolve a rule pattern to a `(relativePattern, root)`
//! pair keyed off the rule's [`PermissionRuleSource`]) and the per-rule slice
//! of `matchingRuleForInput` (gitignore-test one pattern against one path).
//!
//! ## Tool grouping (claude-code `getPatternsByRoot` / `getRuleByContentsForToolName`)
//! claude-code routes file checks by TOOL TYPE, not the running tool's literal
//! name: every editing tool consults `Edit`-named rules (`FILE_EDIT_TOOL_NAME`)
//! and every reading tool consults `Read`-named rules (`FILE_READ_TOOL_NAME`).
//! [`file_tool_kind`] reproduces that split; the integration in
//! [`crate::policy::PermissionPolicy::authorize`] additionally honors
//! "edit-allow implies read-allow" (`checkReadPermissionForTool` step 5).
//!
//! ## Root resolution needs no per-rule plumbing
//! claude-code's `rootPathForSource` + `getSettingsRootPathForSource` switch
//! ONLY on the `source` enum (not the concrete settings-file path), so a single
//! [`FsRoots`] (`cwd` / `home` / `lingxi_home`) threaded in at policy
//! construction reproduces them exactly: `UserSettings`→`lingxi_home`,
//! `Project`/`Local`/`Policy`/`Flag`→`cwd`, `CliArg`/`Command`/`Session`→`cwd`.
//!
//! ## Documented divergences from `filesystem.ts` (forced / bounded, not gaps)
//! - **Per-rule single-pattern test** rather than claude-code's per-root
//!   batched `ignore().add(patterns)` + map-back. `authorize` evaluates rules
//!   one at a time, so each rule is tested in isolation. This is observably
//!   identical for "does THIS rule match THIS path?" except for cross-rule
//!   gitignore NEGATION (`!pattern`) interplay within one root — permission
//!   rule strings never carry `!`, so the case does not arise.
//! - [`expand_path`] itself is lexical, matching the orchestrator's `expandPath`
//!   mirror. Working-directory containment separately adds the natively
//!   symlink-resolved forms, like `getPathsForPermissionCheck`.
//! - The wider `checkRead/checkWritePermissionForTool` flow (internal-path
//!   allowances, `.git`/`.claude` safety asks, suggestions) is NOT reproduced
//!   here — `authorize` keeps its deny→allow→mode shape; see its docs for what
//!   that elides.
//! - **No Unicode NFC** on `~`-expansion (`homedir().normalize('NFC')`): a no-op
//!   for ASCII paths and `OsStr` has no portable NFC primitive.
//!
//! ## Working-directory containment (`AcceptEdits` auto-allow, Batch 1)
//! [`path_in_working_path`] / [`path_in_allowed_working_path`] port claude-code's
//! `pathInWorkingPath` / `pathInAllowedWorkingPath` (`filesystem.ts:683-744`),
//! which gate the `mode === 'acceptEdits' && isInWorkingDir` write auto-allow
//! (`:1360-1375`). They expand both the target and each working dir, apply the
//! macOS `/private/var`→`/var` & `/private/tmp`→`/tmp` symlink rewrites, lowercase
//! case-fold (so `.cLaUdE/…` cannot bypass a case-insensitive filesystem), and
//! accept iff the working-dir-relative path neither escapes upward (`..`-segment)
//! nor is absolute.
//!
//! Both the lexical path and all discoverable symlink-resolved forms are checked
//! against the corresponding forms of every working directory. Resolution is
//! best-effort: unreadable, malformed, cyclic, and over-depth links retain the
//! lexical form, matching claude-code's exception-swallowing resolver.

use crate::rule::PermissionRuleSource;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

const PERMISSION_PATH_RESOLUTION_MAX_HOPS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
enum PermissionPathForms {
    Paths(Vec<PathBuf>),
    FailClosed,
}

/// The filesystem roots a [`PermissionRuleSource`] resolves against, supplied
/// once at policy construction. Mirrors the process-global `getOriginalCwd()` /
/// `homedir()` / `getClaudeConfigHomeDir()` that claude-code's
/// `rootPathForSource` consults.
#[derive(Debug, Clone)]
pub struct FsRoots {
    /// Original working directory — root for `CliArg`/`Command`/`Session` rules
    /// and for project/local/policy settings (`getOriginalCwd()`).
    pub cwd: PathBuf,
    /// Home directory, for `~/`-prefixed patterns. `None` disables `~/`
    /// expansion (the pattern then fails to resolve a root → never matches).
    pub home: Option<PathBuf>,
    /// Claude config home (`~/.claude`), the root for `UserSettings` rules
    /// (`getSettingsRootPathForSource('userSettings')` = `resolve(configHome)`).
    pub lingxi_home: PathBuf,
}

/// Which file-permission group a running tool belongs to (claude-code routes
/// file checks by tool TYPE — `FILE_EDIT_TOOL_NAME='Edit'` /
/// `FILE_READ_TOOL_NAME='Read'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileToolKind {
    /// Editing tools (`checkWritePermissionForTool`, type `'edit'`): consult
    /// `Edit`-named content rules.
    Editor,
    /// Reading tools (`checkReadPermissionForTool`, type `'read'`): consult
    /// `Read`-named content rules, plus `Edit`-ALLOW rules (edit⇒read).
    Reader,
    /// Not a file tool — content matching does not apply (phase-2 tool-wide
    /// behavior is preserved by the caller; Bash/WebFetch content matching is
    /// the separate 3a-bash deferral).
    NonFile,
}

/// Classify a running tool. Editor set = `Edit`/`Write`/`MultiEdit`/
/// `NotebookEdit` (the tools wired to `checkWritePermissionForTool`); reader
/// set = `Read`/`Glob`/`Grep`/`LSP` (wired to `checkReadPermissionForTool`).
/// `MultiEdit` is listed for forward-compatibility (no such tool exists in the
/// port yet; harmless).
#[must_use]
pub fn file_tool_kind(tool_name: &str) -> FileToolKind {
    match tool_name {
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => FileToolKind::Editor,
        "Read" | "Glob" | "Grep" | "LSP" => FileToolKind::Reader,
        _ => FileToolKind::NonFile,
    }
}

/// The input field a file tool declares its path in.
///
/// SINGLE SOURCE OF TRUTH. [`crate::model_path::rewrite_tool_input`] rewrites
/// the same field for the guest→host translation, so a second hand-copied
/// table would let the permission check read one field while the rewrite
/// writes another — the gate would then silently evaluate an untranslated
/// path with no error anywhere.
///
/// `LSP` is the one file tool whose public schema uses camel-case `filePath`,
/// matching Claude Code's `LSPTool.getPath`.
#[must_use]
pub(crate) fn input_path_field_for_tool(tool_name: &str) -> &'static str {
    match tool_name {
        "NotebookEdit" => "notebook_path",
        "Glob" | "Grep" => "path",
        "LSP" => "filePath",
        // Read / Edit / Write / MultiEdit
        _ => "file_path",
    }
}

/// Extract the path a file tool operates on from its input (claude-code
/// `tool.getPath(input)`). The field comes from
/// [`input_path_field_for_tool`]; `Glob`/`Grep` default their search root to
/// `cwd` when `path` is absent, as `GlobTool.getPath`/`GrepTool.getPath` do.
/// Returns `None` only when a `file_path`/`notebook_path` field is absent or
/// non-string (the tool itself would already have failed input validation).
#[must_use]
pub fn input_path_for_tool<'a>(
    tool_name: &str,
    input: &'a serde_json::Value,
    roots: &FsRoots,
) -> Option<std::borrow::Cow<'a, str>> {
    let field = input_path_field_for_tool(tool_name);
    match input.get(field).and_then(serde_json::Value::as_str) {
        Some(p) => Some(std::borrow::Cow::Borrowed(p)),
        // Glob/Grep default their search root to cwd when `path` is omitted.
        None if matches!(tool_name, "Glob" | "Grep") => Some(std::borrow::Cow::Owned(
            roots.cwd.to_string_lossy().into_owned(),
        )),
        None => None,
    }
}

/// Settings root for a rule source — 1:1 with claude-code `rootPathForSource`
/// (`filesystem.ts`) composed with `getSettingsRootPathForSource`
/// (`settings/settings.ts`): user settings resolve against the Claude config
/// home; every project-scoped / runtime source resolves against the original
/// cwd. `FlagSettings` approximates to `cwd` (the flag-settings file path is
/// not plumbed into the port — documented).
/// NOT a gap: the canonical-git-root refinement is deliberately absent here.
/// Verified in 2.1.235 — the pattern root comes from `Kwu`:
/// `Kwu(e,t){ return e==="localSettings" ? resolve(t.cwd) : CEn(e,t) }`
/// which SHORT-CIRCUITS `localSettings` to a plain `resolve(cwd)` before `CEn`
/// is reached. `CEn`'s `localSettings` branch (`Jct(cwd, canonicalGitRoot)`) is
/// only consulted by `EKe`, which locates the settings FILE on disk — a
/// different question from "what root does this rule's pattern resolve
/// against". `CEn` sends `projectSettings`/`policySettings` to `resolve(cwd)`
/// too, so every non-user source landing on `roots.cwd` is byte-correct.
fn root_path_for_source(source: PermissionRuleSource, roots: &FsRoots) -> PathBuf {
    match source {
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User) => roots.lingxi_home.clone(),
        PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        | PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Local)
        | PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Managed)
        | PermissionRuleSource::FlagSettings
        | PermissionRuleSource::CliArg
        | PermissionRuleSource::Command
        | PermissionRuleSource::Session
        // Runtime sources (2.1.215 tail) → original cwd like every non-user source.
        | PermissionRuleSource::ToolsNarrowing
        | PermissionRuleSource::McpServerPolicy => roots.cwd.clone(),
    }
}

/// Resolve a rule pattern to `(relativePattern, root)` — 1:1 with claude-code
/// `patternWithRoot`. `root == None` means "anywhere" (matched relative to cwd
/// by the caller, mirroring `root ?? getCwd()`).
///
/// - `//abs…` → root `/`, pattern keeps a single leading slash (`pattern[1..]`).
/// - `~/x` → root = home, pattern = `/x` (`pattern[1..]`); `None` home → no root.
/// - `/x` (single slash) → root = [`root_path_for_source`], pattern unchanged.
/// - otherwise → root `None`; a leading `./` is stripped (`./.env` → `.env`).
pub(crate) fn pattern_with_root(
    pattern: &str,
    source: PermissionRuleSource,
    roots: &FsRoots,
) -> (String, Option<PathBuf>) {
    if let Some(after_first) = pattern.strip_prefix("//") {
        // `//etc/**` → relativePattern `/etc/**`, root `/` (pattern.slice(1)).
        (format!("/{after_first}"), Some(PathBuf::from("/")))
    } else if let Some(rest) = pattern.strip_prefix("~/") {
        // `~/x` → relativePattern `/x` (pattern.slice(1)), root = home.
        match &roots.home {
            Some(h) => (format!("/{rest}"), Some(h.clone())),
            // No home configured: cannot resolve `~` → leave unrooted so it
            // resolves against cwd and effectively never matches a `~` file.
            None => (pattern.to_string(), None),
        }
    } else if pattern.starts_with('/') {
        // Single leading slash → settings-dir-relative.
        (
            pattern.to_string(),
            Some(root_path_for_source(source, roots)),
        )
    } else {
        // No root: strip a leading `./` so `./.env` matches `.env`.
        let normalized = pattern.strip_prefix("./").unwrap_or(pattern);
        (normalized.to_string(), None)
    }
}

/// Lexically expand a tool path to an absolute, normalized path — same
/// `expandPath` mirror as the orchestrator's `absolutize` (trim; bare `~` /
/// `~/…` against home; absolute kept; relative joined to cwd; then `.`/`..`
/// collapsed without touching disk).
///
/// `pub(crate)` so the auto-edit safety guard ([`crate::auto_edit_safety`]) can
/// reuse the SAME lexical expansion claude-code's `expandPath` performs before
/// the danger-segment scan — keeping the two paths byte-identical.
pub(crate) fn expand_path(raw: &str, roots: &FsRoots) -> PathBuf {
    let trimmed = raw.trim();
    let expanded: PathBuf = if trimmed == "~" {
        roots.home.clone().unwrap_or_else(|| PathBuf::from(trimmed))
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        roots
            .home
            .as_ref()
            .map_or_else(|| PathBuf::from(trimmed), |h| h.join(rest))
    } else {
        let p = Path::new(trimmed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            roots.cwd.join(p)
        }
    };
    normalize_lexically(&expanded)
}

/// The path forms permission checks must consider: the lexical form plus, when
/// the filesystem exposes one, the symlink-resolved target path. Mirrors
/// claude-code `getPathsForPermissionCheck` at the containment layer.
fn permission_paths_to_check(path: &Path, roots: &FsRoots) -> PermissionPathForms {
    let absolute = expand_path(&path.to_string_lossy(), roots);
    if path_resolution_must_fail_closed(&absolute) {
        return PermissionPathForms::FailClosed;
    }

    let mut out = vec![absolute.clone()];
    for resolved in resolve_additional_permission_paths(&absolute) {
        if resolved != absolute && !out.contains(&resolved) {
            out.push(resolved);
        }
    }
    PermissionPathForms::Paths(out)
}

fn path_resolution_must_fail_closed(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    meta.file_type().is_symlink()
        && std::fs::canonicalize(path).is_err()
        && resolve_dangling_symlink(path).is_none()
}

fn resolve_additional_permission_paths(absolute_path: &Path) -> Vec<PathBuf> {
    let mut resolved = Vec::new();
    let mut seen = HashSet::new();
    seen.insert(absolute_path.to_path_buf());
    if let Some(collapsed) = resolve_deepest_existing_ancestor(absolute_path) {
        if seen.insert(collapsed.clone()) {
            resolved.push(collapsed);
        }
    }

    let mut current = absolute_path.to_path_buf();
    let mut lineage = HashSet::new();
    for _ in 0..PERMISSION_PATH_RESOLUTION_MAX_HOPS {
        if !lineage.insert(current.clone()) {
            break;
        }

        match std::fs::read_link(&current) {
            Ok(target) => {
                let next = if target.is_absolute() {
                    target
                } else {
                    let Some(parent) = current.parent() else {
                        break;
                    };
                    parent.join(target)
                };
                let next = normalize_lexically(&next);
                if seen.insert(next.clone()) {
                    resolved.push(next.clone());
                }
                current = next;
            }
            Err(_) => break,
        }
    }

    if let Ok(real_path) = std::fs::canonicalize(absolute_path) {
        if seen.insert(real_path.clone()) {
            resolved.push(real_path);
        }
    }

    resolved
}

/// Resolve the deepest existing ancestor of `absolute_path` through the real
/// filesystem, then re-attach any non-existent tail segments. This catches both
/// direct symlinks and parent-directory symlinks for paths that do not yet
/// exist.
fn resolve_deepest_existing_ancestor(absolute_path: &Path) -> Option<PathBuf> {
    for ancestor in absolute_path.ancestors() {
        let Ok(meta) = std::fs::symlink_metadata(ancestor) else {
            continue;
        };
        let resolved_ancestor = match std::fs::canonicalize(ancestor) {
            Ok(resolved) => resolved,
            // SECURITY: canonicalize FOLLOWS the link, so it fails (ENOENT) on a
            // DANGLING symlink — one whose target does not exist yet, i.e. a link
            // you can CREATE a file through. Falling through to the parent here
            // (the old behaviour) re-attached the link NAME lexically and never
            // saw the OUTSIDE target, so a write through `ws/link → /etc/cron.d/x`
            // stayed lexically inside the workspace and evaded the containment
            // ask. The oracle (`XW`) resolves such links via `readlinkSync`, so
            // its check set contains the outside target. Mirror that: chase the
            // link chain to the dangling target and use it.
            Err(_) if meta.file_type().is_symlink() => match resolve_dangling_symlink(ancestor) {
                Some(target) => target,
                // Not resolvable (cycle / too many hops) — keep the lexical
                // form, matching the oracle's best-effort resolver.
                None => continue,
            },
            // A non-symlink canonicalize failure (e.g. EACCES) keeps the prior
            // skip-to-parent behaviour.
            Err(_) => continue,
        };
        let tail = absolute_path.strip_prefix(ancestor).ok()?;
        return Some(resolved_ancestor.join(tail));
    }
    None
}

/// Resolve a DANGLING symlink (one `canonicalize` can't follow because its
/// target does not exist) to the outside path it ultimately points at, chasing
/// the link chain lexically — the port of the oracle's manual 64-hop
/// `readlinkSync` walk (`XW`), which stops at the first non-existent component
/// (`lstatSync` throwing) and returns that target.
fn resolve_dangling_symlink(link: &Path) -> Option<PathBuf> {
    let mut current = link.to_path_buf();
    for _ in 0..64 {
        let target = std::fs::read_link(&current).ok()?;
        let resolved = if target.is_absolute() {
            target
        } else {
            // Relative to the link's PARENT directory, matching
            // `path.resolve(path.dirname(link), target)`.
            current.parent()?.join(target)
        };
        let resolved = normalize_lexically(&resolved);
        match std::fs::symlink_metadata(&resolved) {
            // The chain continues through another symlink — keep chasing.
            Ok(meta) if meta.file_type().is_symlink() => current = resolved,
            // The chain ends at an existing non-symlink OR (the exploit case) a
            // non-existent path: this is the real target the OS would create
            // through, so it is what containment must judge.
            _ => return Some(resolved),
        }
    }
    None
}

/// Collapse `.`/`..` segments without touching the filesystem (Node
/// `path.normalize`/`resolve`). A `..` pops the previous NORMAL component;
/// a leading `..` with nothing to pop is kept.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// POSIX-style lexical relative path from `base` to `target` (both absolute,
/// already normalized) — mirrors `posix.relative`. Returns the `/`-joined
/// relative string; `""` when equal; a `..`-leading string when `target` is
/// outside `base`.
fn posix_relative(base: &Path, target: &Path) -> String {
    let b: Vec<Component> = base.components().collect();
    let t: Vec<Component> = target.components().collect();
    let mut i = 0;
    while i < b.len() && i < t.len() && b[i] == t[i] {
        i += 1;
    }
    let mut parts: Vec<String> = Vec::new();
    for _ in i..b.len() {
        parts.push("..".to_string());
    }
    for c in &t[i..] {
        parts.push(c.as_os_str().to_string_lossy().into_owned());
    }
    parts.join("/")
}

/// Outcome of testing ONE gitignore-style permission rule pattern against a
/// path — the per-pattern half of claude-code's per-source `ignore` matcher.
///
/// claude-code compiles every rule of one settings source into a SINGLE
/// `ignore` matcher (`Zr`, `permissions.ts`), so a `!`-prefixed pattern is a
/// gitignore NEGATION: it does not produce a match of its own, it cancels an
/// earlier match from that same source. Reporting the polarity separately lets
/// the caller replay that state machine over the source's rules — see
/// [`crate::policy`]'s `decide_in_source`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RulePatternMatch {
    /// The pattern does not cover this path.
    NoMatch,
    /// The pattern covers this path and is an ordinary (positive) rule.
    Match,
    /// The pattern covers this path and is `!`-negated — it CANCELS an earlier
    /// match from the same settings source rather than producing one.
    Negated,
}

/// Test whether `input_path` (a tool's raw path arg) matches a single rule
/// `pattern` tagged with `source` — the per-rule slice of claude-code
/// `matchingRuleForInput`. Ignores gitignore negation: a `!`-prefixed pattern
/// reports `false`, which is what a caller that cannot see the rest of its
/// settings source must assume. Source-aware callers use
/// [`test_rule_pattern`].
///
/// ⚠️ Nothing inside this crate calls this any more — every rule walk became
/// source-aware with HP-7. It is kept as the safe public answer for a caller
/// that genuinely has one pattern and no source context, and calling it can
/// only ever be MORE restrictive than the real policy, never less. Do not read
/// its presence as evidence that the per-rule form is still the live path.
#[must_use]
pub fn path_matches_rule_pattern(
    input_path: &str,
    pattern: &str,
    source: PermissionRuleSource,
    behavior: crate::rule::PermissionBehavior,
    roots: &FsRoots,
) -> bool {
    matches!(
        test_rule_pattern(input_path, pattern, source, behavior, roots),
        RulePatternMatch::Match
    )
}

/// Test `pattern` against `input_path`, reporting gitignore negation polarity —
/// the per-rule slice of claude-code `matchingRuleForInput`.
///
/// 1. Expand the input path to an absolute, normalized path.
/// 2. Resolve the pattern's `(relativePattern, root)` via [`pattern_with_root`]
///    (`None` root ⇒ cwd, mirroring `root ?? getCwd()`).
/// 3. Compute the path relative to that root; bail if it escapes the root
///    (`..`-prefixed) or is empty (claude-code skips both).
/// 4. Strip a trailing `/**` (the `ignore` lib treats `path` as matching the
///    path AND everything inside it) and gitignore-test the relative path.
/// 5. Split off a leading `!` (claude-code hands the `!`-prefixed pattern to
///    `ignore` verbatim and lets the library apply gitignore negation) and
///    report it as [`RulePatternMatch::Negated`].
///
/// A BARE `!` — nothing but whitespace after it — is dropped outright, matching
/// `Ki`'s `/^!\s*$/` guard ("a negation of every path"). ⚠️ `Ki` applies that
/// guard only to non-allow rules; for an ALLOW rule upstream keeps the bare `!`
/// and hands it to `ignore`, where it is a negation over the allow matcher.
/// Dropping it on both sides can only make this port refuse where upstream
/// might allow, which is the safe direction, and the alternative depends on
/// npm-`ignore`'s behaviour for an empty pattern body — not something to guess.
///
/// ⚠️ The negation is resolved on the pattern AFTER [`pattern_with_root`], not
/// before: `!` is not one of the root sigils (`/`, `~/`, `./`), so a
/// `!`-prefixed pattern is always rooted at the cwd upstream too. Stripping the
/// `!` first would silently re-root `!/abs/path` at `/abs`.
#[must_use]
pub fn test_rule_pattern(
    input_path: &str,
    pattern: &str,
    source: PermissionRuleSource,
    behavior: crate::rule::PermissionBehavior,
    roots: &FsRoots,
) -> RulePatternMatch {
    let file_abs = expand_path(input_path, roots);
    let (rel_pattern, root_opt) = pattern_with_root(pattern, source, roots);
    let effective_root = root_opt.unwrap_or_else(|| roots.cwd.clone());

    let rel_str = posix_relative(&effective_root, &file_abs);
    if rel_str.is_empty() || rel_str == ".." || rel_str.starts_with("../") {
        // Path is outside the pattern's root (or equals it) → no match.
        // claude-code skips only `""` and a `"../"`-prefix; the extra bare
        // `".."` guard is a deliberate defensive superset — it keeps a synthetic
        // `"/.."` (which carries a `ParentDir` component) from reaching the
        // builder, and the outcome is unchanged (TS's `ig.test("..")` also
        // yields no match for any real glob).
        return RulePatternMatch::NoMatch;
    }

    // Port of the oracle's `FTm` (matchingRuleForInput's pattern rewrite):
    //   if (e.endsWith("/**")) { let r = e.slice(0,-3);
    //     return /[^/]/.test(r) ? (r.includes("/")||!t||/^[!#]/.test(r) ? r : "/"+r) : "/**" }
    //   return e
    // where `t` is `behavior === "allow"`. `ignore` treats `dir` as matching
    // `dir` and everything under it, so the `/**` suffix is redundant — but
    // WHICH form replaces it depends on the rule's behavior.
    // A bare root-anchored `/**` (also `//**` / `~/**`) would strip to "",
    // which is not a pattern at all. claude-code never reaches that state: its
    // `FTm` only rewrites a `/**`-suffixed pattern when the remainder contains
    // a non-slash character (`/[^/]/.test(r)`), so `"/**"` is handed to
    // `ignore().add()` UNCHANGED and matches every path under the root. Keep
    // the whole pattern in that case rather than stripping it away — the empty
    // string only ever existed on our side, and treating it as "matches
    // nothing" silently revoked a grant the oracle honors.
    let stripped: std::borrow::Cow<'_, str> = match rel_pattern.strip_suffix("/**") {
        None => std::borrow::Cow::Borrowed(rel_pattern.as_str()),
        Some(remainder) => {
            if remainder.chars().all(|c| c == '/') {
                // `/[^/]/.test(r)` is false (r is empty or all slashes). The
                // oracle's false arm returns the LITERAL `"/**"`, not `e` —
                // `return /[^/]/.test(r) ? (…) : "/**"`. For the only reachable
                // input the two agree (`pattern_with_root` has already rewritten
                // `//**` to `/**`), but emit the literal so a future reader
                // porting an adjacent branch is not misled.
                std::borrow::Cow::Borrowed("/**")
            } else if remainder.contains('/')
                || behavior != crate::rule::PermissionBehavior::Allow
                || remainder.starts_with(['!', '#'])
            {
                // Multi-segment, non-allow, or gitignore-sigil: left UNANCHORED,
                // so gitignore matches the pattern at any depth. Keeping deny
                // and ask broad is the safe direction.
                std::borrow::Cow::Borrowed(remainder)
            } else {
                // Single-segment ALLOW: re-anchored to the root so
                // `Edit(src/**)` cannot authorize `<root>/vendor/src/...`.
                std::borrow::Cow::Owned(format!("/{remainder}"))
            }
        }
    };
    let stripped = stripped.as_ref();

    // Match the relative path as if rooted at `/`: build a one-pattern matcher
    // anchored at `/` and test `/<relative>`. Computing the relative path
    // ourselves (above) — rather than handing the absolute path to the builder
    // — avoids the `ignore` crate's prefix-strip mis-matching paths that sit
    // OUTSIDE the root (it would otherwise glob-test the unstripped absolute).
    // Split the gitignore negation off LAST, once `pattern_with_root` and the
    // `/**` rewrite have both run on the `!`-prefixed spelling (claude-code's
    // `Xn` keeps a `/^[!#]/` remainder unanchored for exactly this reason).
    let (stripped, negated) = match stripped.strip_prefix('!') {
        // `Ki`: a bare `!` is "a negation of every path" and is dropped.
        Some(rest) if rest.trim().is_empty() => return RulePatternMatch::NoMatch,
        Some(rest) => (rest, true),
        None => (stripped, false),
    };

    let gitignore = cached_gitignore(stripped);
    let Some(gitignore) = gitignore.as_ref() else {
        return RulePatternMatch::NoMatch;
    };
    let target = Path::new("/").join(&rel_str);
    if !gitignore
        .matched_path_or_any_parents(&target, false)
        .is_ignore()
    {
        return RulePatternMatch::NoMatch;
    }
    if negated {
        RulePatternMatch::Negated
    } else {
        RulePatternMatch::Match
    }
}

fn cached_gitignore(pattern: &str) -> Option<std::sync::Arc<ignore::gitignore::Gitignore>> {
    use std::sync::{Arc, Mutex, OnceLock};
    type Cache =
        Mutex<std::collections::HashMap<String, Option<Arc<ignore::gitignore::Gitignore>>>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()));
    {
        let guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(hit) = guard.get(pattern) {
            return hit.clone();
        }
    }
    let mut builder = ignore::gitignore::GitignoreBuilder::new("/");
    if let Err(e) = builder.add_line(None, pattern) {
        // An unbuildable glob (e.g. an unbalanced char class) matches nothing.
        // That is fail-CLOSED for an allow rule (safe) but fail-OPEN for a deny
        // rule (the deny silently does nothing), and it diverges from npm
        // `ignore`, which is more lenient — so surface it rather than swallow.
        tracing::warn!(
            pattern = %pattern,
            error = %e,
            "permission rule has an unparseable glob; the rule will not match"
        );
        let mut guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() < 256 {
            guard.insert(pattern.to_string(), None);
        }
        return None;
    }
    let Ok(gitignore) = builder.build() else {
        return None;
    };
    let compiled = Arc::new(gitignore);
    {
        let mut guard = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if guard.len() < 256 {
            guard.insert(pattern.to_string(), Some(compiled.clone()));
        }
    }
    Some(compiled)
}

/// Normalize a path string for case-insensitive comparison — port of
/// `normalizeCaseForComparison` (`filesystem.ts:90-92`). Always lowercases
/// regardless of platform so a mixed-case `.cLauDe/CoMmAnDs` cannot bypass the
/// security checks on a case-insensitive filesystem (macOS / Windows).
///
/// (A sibling [`crate::auto_edit_safety::normalize_case_for_comparison`] exists
/// for the auto-edit safety guard; both are intentionally the same `to_lowercase`
/// — kept per-module to mirror the two TS call sites without a cross-module dep.)
#[must_use]
pub fn normalize_case_for_comparison(s: &str) -> String {
    s.to_lowercase()
}

/// Apply the macOS symlink rewrites claude-code performs before comparing paths
/// for working-dir containment (`filesystem.ts:716-721`):
/// `/private/var/` → `/var/` and `/private/tmp` (followed by `/` or end) →
/// `/tmp`. Operates on the already-absolute, lexically-expanded path string.
///
/// Only the leading `/private/var/` and `/private/tmp` forms are rewritten (the
/// TS regexes are anchored with `^`); an interior `/private/var` is untouched.
fn rewrite_private_symlinks(abs: &str) -> String {
    if let Some(rest) = abs.strip_prefix("/private/var/") {
        return format!("/var/{rest}");
    }
    // `/private/tmp$` or `/private/tmp/…` (the `(\/|$)` capture is preserved).
    if abs == "/private/tmp" {
        return "/tmp".to_string();
    }
    if let Some(rest) = abs.strip_prefix("/private/tmp/") {
        return format!("/tmp/{rest}");
    }
    abs.to_string()
}

/// Is `path` inside (or equal to) the single working directory `working`? — port
/// of `pathInWorkingPath` (`RM`, `filesystem.ts:709-744`), invoked with
/// `caseFold:false` (2.1.211 `EV` → the path-validation containment and the
/// acceptEdits `alreadyInWorkingDirectory` auto-allow).
///
/// 1. Lexically expand both `path` and `working` to absolute, normalized paths.
/// 2. Apply the macOS `/private/var`→`/var` & `/private/tmp`→`/tmp` rewrites
///    (case-SENSITIVE — `RM` drops the regexes' `i` flag when `caseFold` is
///    false, and [`rewrite_private_symlinks`] matches the prefix case-sensitively).
/// 3. Compute the working-dir-relative path; accept iff it is the same path
///    (`""`), does NOT contain a `..` traversal segment, and is NOT absolute.
///
/// Case-SENSITIVE comparison: 2.1.211's containment (`EV`) passes
/// `caseFold:false`, so `RM` compares `pPt(l, a)` WITHOUT lower-casing (only the
/// RM-default `caseFold:true` path folds via `Jg`). A case-variant path
/// (`/Proj/SRC`) is therefore treated as OUTSIDE a lowercase working dir
/// (`/proj/src`) → asks, rather than being auto-allowed. Every current caller of
/// this function is an `EV`-semantics containment check.
///
/// Lexical only — see the module-header divergence note (no on-disk `realpath`).
#[must_use]
pub fn path_in_working_path(path: &Path, working: &Path, roots: &FsRoots) -> bool {
    let absolute_path = expand_path(&path.to_string_lossy(), roots);
    let absolute_working_path = expand_path(&working.to_string_lossy(), roots);

    // macOS symlink rewrites (`/private/var`→`/var`, `/private/tmp`→`/tmp`),
    // case-sensitive (RM's non-`i` regexes under `caseFold:false`).
    let normalized_path = rewrite_private_symlinks(&absolute_path.to_string_lossy());
    let normalized_working = rewrite_private_symlinks(&absolute_working_path.to_string_lossy());

    // POSIX relative path from working dir to target — case-SENSITIVE
    // (`caseFold:false`, so no `normalize_case_for_comparison` fold).
    let relative = posix_relative(Path::new(&normalized_working), Path::new(&normalized_path));

    // Same path.
    if relative.is_empty() {
        return true;
    }

    // `containsPathTraversal` (`path.ts:133-135`): `(?:^|[\\/])\.\.(?:[\\/]|$)` —
    // a `..` segment bounded by separators / string ends. `posix_relative` only
    // ever emits `..` at the START (or as the whole string), so this rejects an
    // escaping target. We mirror the TS predicate's full segment semantics.
    if contains_path_traversal(&relative) {
        return false;
    }

    // Inside iff the relative path is not itself absolute (`posix.isAbsolute`).
    !Path::new(&relative).is_absolute()
}

/// Port of `containsPathTraversal` (`path.ts:133-135`):
/// JS `/(?:^|[\\/])\.\.(?:[\\/]|$)/` — true when a `..` appears as a whole path
/// segment (bounded by `/`, `\`, or a string boundary on each side). A `..`
/// embedded in a longer name (`..beta`, `x..y`) is NOT a traversal.
fn contains_path_traversal(path: &str) -> bool {
    let bytes = path.as_bytes();
    let len = bytes.len();
    let is_sep = |b: u8| b == b'/' || b == b'\\';
    let mut i = 0;
    while i + 1 < len {
        if bytes[i] == b'.' && bytes[i + 1] == b'.' {
            let left_ok = i == 0 || is_sep(bytes[i - 1]);
            let right_idx = i + 2;
            let right_ok = right_idx == len || is_sep(bytes[right_idx]);
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// Is `path` inside ANY of the allowed working directories? — port of
/// `pathInAllowedWorkingPath` (`filesystem.ts:683-707`). Every lexical/resolved
/// input form must be contained by at least one lexical/resolved working-dir
/// form. Returns `false` for an empty `working_dirs` list (no allowance).
#[must_use]
pub fn path_in_allowed_working_path(
    path: &Path,
    working_dirs: &[PathBuf],
    roots: &FsRoots,
) -> bool {
    let PermissionPathForms::Paths(path_forms) = permission_paths_to_check(path, roots) else {
        return false;
    };
    let working_forms: Vec<PathBuf> = working_dirs
        .iter()
        .filter_map(
            |working_dir| match permission_paths_to_check(working_dir, roots) {
                PermissionPathForms::Paths(forms) => Some(forms),
                PermissionPathForms::FailClosed => None,
            },
        )
        .flatten()
        .collect();
    if working_forms.is_empty() {
        return false;
    }
    path_forms.iter().all(|path_form| {
        working_forms
            .iter()
            .any(|working_form| path_in_working_path(path_form, working_form, roots))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    fn unique_temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "lingxi_permission_{label}_{}_{}",
            std::process::id(),
            nonce
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn roots() -> FsRoots {
        FsRoots {
            cwd: PathBuf::from("/proj"),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn matches(input: &str, pattern: &str, source: PermissionRuleSource) -> bool {
        matches_with(
            input,
            pattern,
            source,
            crate::rule::PermissionBehavior::Allow,
        )
    }

    fn matches_with(
        input: &str,
        pattern: &str,
        source: PermissionRuleSource,
        behavior: crate::rule::PermissionBehavior,
    ) -> bool {
        path_matches_rule_pattern(input, pattern, source, behavior, &roots())
    }

    #[test]
    fn relative_glob_matches_under_cwd() {
        // `Edit(src/**)` in project settings → root = cwd.
        assert!(matches(
            "/proj/src/main.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        assert!(matches(
            "src/lib.rs", // relative input → resolved against cwd
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        // Outside src → no match.
        assert!(!matches(
            "/proj/tests/x.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn dot_slash_prefix_is_stripped() {
        // `./secrets/**` behaves like `secrets/**`.
        assert!(matches(
            "/proj/secrets/key.pem",
            "./secrets/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn bare_dotfile_pattern_matches_basename() {
        assert!(matches(
            "/proj/.env",
            ".env",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        // gitignore basename rule matches at any depth.
        assert!(matches(
            "/proj/nested/.env",
            ".env",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn leading_slash_anchors_to_settings_root() {
        // `/src/**` is anchored at the settings root (cwd for project).
        assert!(matches(
            "/proj/src/a.rs",
            "/src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        // anchored: a nested `src` does NOT match.
        assert!(!matches(
            "/proj/a/src/b.rs",
            "/src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn no_leading_slash_is_unanchored_for_deny() {
        // `src/**` with no leading slash strips to the bare segment `src`,
        // which gitignore matches at ANY depth.
        //
        // This holds for DENY/ASK only. The oracle's `FTm` re-anchors the same
        // pattern to `/src` when the rule is an ALLOW (`!t` short-circuits the
        // rewrite for every other behavior), so the allow direction stays bound
        // to the root — see
        // `single_segment_glob_is_anchored_for_allow_but_not_for_deny`. This
        // test previously asserted the ALLOW case here, which is what let an
        // allow rule reach a nested directory of the same name.
        assert!(matches_with(
            "/proj/a/src/b.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            crate::rule::PermissionBehavior::Deny
        ));
    }

    #[test]
    fn user_settings_root_is_lingxi_home() {
        // `/sub/**` in USER settings resolves against ~/.claude, not cwd.
        assert!(matches(
            "/home/u/.lingxi/sub/x",
            "/sub/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        ));
        // A cwd path is OUTSIDE the user-settings root → no match.
        assert!(!matches(
            "/proj/sub/x",
            "/sub/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        ));
    }

    #[test]
    fn tilde_resolves_against_home() {
        assert!(matches(
            "/home/u/.ssh/id_rsa",
            "~/.ssh/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        ));
        // `~`-expanded input path also resolves to home.
        assert!(matches(
            "~/.ssh/id_rsa",
            "~/.ssh/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::User)
        ));
    }

    #[test]
    fn double_slash_resolves_against_filesystem_root() {
        assert!(matches(
            "/etc/passwd",
            "//etc/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        assert!(!matches(
            "/proj/etc/passwd",
            "//etc/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn path_outside_root_does_not_match() {
        // file above the root → posix_relative starts with `..` → skip.
        assert!(!matches(
            "/other/x.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn glob_extension_pattern() {
        assert!(matches(
            "/proj/a/b.test.ts",
            "**/*.test.ts",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        assert!(!matches(
            "/proj/a/b.ts",
            "**/*.test.ts",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    /// Oracle `FTm` re-ANCHORS a single-segment `<dir>/**` for ALLOW rules
    /// only:
    ///   return /[^/]/.test(r) ? (r.includes("/")||!t||/^[!#]/.test(r) ? r : "/"+r) : "/**"
    /// with `t = (behavior === "allow")`. So `Edit(src/**)` as an ALLOW becomes
    /// `/src` — bound to the root — while the same pattern as a DENY stays the
    /// unanchored `src`, which gitignore matches at ANY depth.
    ///
    /// Dropping that asymmetry made every allow rule as broad as a deny rule:
    /// `Edit(src/**)` would authorize `<root>/vendor/src/...`, which the user
    /// never granted. The deny direction must stay broad.
    #[test]
    fn single_segment_glob_is_anchored_for_allow_but_not_for_deny() {
        use crate::rule::PermissionBehavior::{Allow, Deny};

        // ALLOW: anchored at the root.
        assert!(matches_with(
            "/proj/src/a.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            Allow
        ));
        assert!(
            !matches_with(
                "/proj/vendor/src/a.rs",
                "src/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
                Allow
            ),
            "an allow rule must not reach a nested directory of the same name"
        );

        // DENY: unanchored, so it still catches the nested copy.
        assert!(matches_with(
            "/proj/src/a.rs",
            "src/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            Deny
        ));
        assert!(
            matches_with(
                "/proj/vendor/src/a.rs",
                "src/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
                Deny
            ),
            "a deny rule must stay broad"
        );

        // MULTI-segment patterns skip the explicit `/` prefix (`r.includes("/")`
        // short-circuits), but gitignore anchors any pattern containing a
        // separator anyway — so `a/b/**` is root-bound in both directions.
        assert!(matches_with(
            "/proj/a/b/x.rs",
            "a/b/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            Allow
        ));
        assert!(!matches_with(
            "/proj/nested/a/b/x.rs",
            "a/b/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            Allow
        ));
    }

    #[test]
    fn bare_root_double_star_matches_everything_under_its_root() {
        // Oracle 2.1.235, read directly:
        //   FTm(e,t){ if(e.endsWith("/**")){ let r=e.slice(0,-3);
        //             return /[^/]/.test(r) ? … : "/**" } return e }
        // For e="/**" the remainder r is "", `/[^/]/.test("")` is FALSE, so the
        // pattern is returned UNCHANGED and `ignore().add("/**")` matches every
        // path under the root. (`NTm` also passes "/**" through untouched —
        // its guard regex is /^\s*(?:\/\*\*)?$/.)
        //
        // This previously asserted the opposite, on the claim that claude-code
        // feeds "" to `ignore().add([""])` and drops it as a blank line. That
        // claim was wrong: the "" only ever existed on OUR side, because we
        // stripped the `/**` suffix before handing the pattern over. The test
        // therefore certified the divergence it should have caught.
        assert!(matches(
            "/proj/anything/deep.rs",
            "/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        assert!(matches(
            "/etc/x",
            "//**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        // A NON-empty unanchored `**` still matches everything (no strip).
        assert!(matches(
            "/proj/anything/deep.rs",
            "**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
        // The root anchor still BOUNDS the match: `/**` rooted at the project
        // must not reach a sibling directory outside it.
        assert!(!matches(
            "/elsewhere/secret.rs",
            "/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
        ));
    }

    #[test]
    fn tool_classification() {
        assert_eq!(file_tool_kind("Edit"), FileToolKind::Editor);
        assert_eq!(file_tool_kind("Write"), FileToolKind::Editor);
        assert_eq!(file_tool_kind("NotebookEdit"), FileToolKind::Editor);
        assert_eq!(file_tool_kind("Read"), FileToolKind::Reader);
        assert_eq!(file_tool_kind("Glob"), FileToolKind::Reader);
        assert_eq!(file_tool_kind("Grep"), FileToolKind::Reader);
        assert_eq!(file_tool_kind("LSP"), FileToolKind::Reader);
        assert_eq!(file_tool_kind("Bash"), FileToolKind::NonFile);
    }

    #[test]
    fn input_path_extraction_per_tool() {
        let r = roots();
        let notebook = serde_json::json!({ "notebook_path": "/proj/a.ipynb" });
        assert_eq!(
            input_path_for_tool("NotebookEdit", &notebook, &r).as_deref(),
            Some("/proj/a.ipynb")
        );
        let edit = serde_json::json!({ "file_path": "/proj/x.rs" });
        assert_eq!(
            input_path_for_tool("Edit", &edit, &r).as_deref(),
            Some("/proj/x.rs")
        );
        let grep = serde_json::json!({ "path": "/proj/sub" });
        assert_eq!(
            input_path_for_tool("Grep", &grep, &r).as_deref(),
            Some("/proj/sub")
        );
        let lsp = serde_json::json!({ "filePath": "/proj/src/lib.rs" });
        assert_eq!(
            input_path_for_tool("LSP", &lsp, &r).as_deref(),
            Some("/proj/src/lib.rs")
        );
        // Glob/Grep default to cwd when `path` is absent.
        let grep_no_path = serde_json::json!({ "pattern": "x" });
        assert_eq!(
            input_path_for_tool("Grep", &grep_no_path, &r).as_deref(),
            Some("/proj")
        );
        // Missing required path → None.
        let empty = serde_json::json!({});
        assert!(input_path_for_tool("Edit", &empty, &r).is_none());
    }

    // ── Batch 1: working-dir containment ──────────────────────────────────

    fn in_working(path: &str, working: &str) -> bool {
        path_in_working_path(Path::new(path), Path::new(working), &roots())
    }

    #[test]
    fn normalize_case_lowercases_in_filesystem() {
        assert_eq!(normalize_case_for_comparison(".LINGXI"), ".lingxi");
        assert_eq!(normalize_case_for_comparison("Foo/Bar.RS"), "foo/bar.rs");
    }

    #[test]
    fn path_inside_working_dir_is_contained() {
        assert!(in_working("/proj/src/main.rs", "/proj"));
        // Nested deeper.
        assert!(in_working("/proj/a/b/c.rs", "/proj"));
        // The working dir itself (same path) is contained.
        assert!(in_working("/proj", "/proj"));
        // A relative input path resolves against cwd (= /proj) → inside.
        assert!(in_working("src/main.rs", "/proj"));
    }

    #[test]
    fn path_outside_working_dir_is_not_contained() {
        assert!(!in_working("/other/x.rs", "/proj"));
        // A sibling that shares a prefix but is not under the dir.
        assert!(!in_working("/projector/x.rs", "/proj"));
    }

    #[test]
    fn dotdot_escape_is_rejected() {
        // `..`-escaping the working dir must be rejected even though the lexical
        // expansion of the working dir vs target could otherwise look adjacent.
        assert!(!in_working("/proj/../etc/passwd", "/proj"));
        // A `..` that stays inside is fine (`/proj/a/../b` == `/proj/b`).
        assert!(in_working("/proj/a/../b.rs", "/proj"));
    }

    #[test]
    fn private_var_rewrite_makes_paths_match() {
        // `/private/var/...` target vs `/var/...` working dir → both rewrite to
        // `/var/...` and compare equal-prefix → contained.
        assert!(in_working(
            "/private/var/folders/x/file.rs",
            "/var/folders/x"
        ));
        // And the reverse: `/var/...` target vs `/private/var/...` working dir.
        assert!(in_working(
            "/var/folders/x/file.rs",
            "/private/var/folders/x"
        ));
    }

    #[test]
    fn private_tmp_rewrite_makes_paths_match() {
        assert!(in_working("/private/tmp/work/out.rs", "/tmp/work"));
        assert!(in_working("/tmp/work/out.rs", "/private/tmp/work"));
        // Bare `/private/tmp` rewrites to `/tmp` (the `$`-anchored branch).
        assert!(in_working("/private/tmp", "/tmp"));
    }

    #[test]
    fn comparison_is_case_sensitive() {
        // PERM-PATH-06: 2.1.211's containment (`EV`) uses `caseFold:false`, so a
        // case-variant path is treated as OUTSIDE a differently-cased working
        // dir (no auto-allow — it asks).
        assert!(!in_working("/Proj/SRC/Main.RS", "/proj/src"));
        assert!(!in_working("/proj/.LiNgXi/x", "/proj/.lingxi"));
        // Exact-case containment still holds.
        assert!(in_working("/proj/src/Main.RS", "/proj/src"));
        assert!(in_working("/proj/.lingxi/x", "/proj/.lingxi"));
    }

    #[test]
    fn contains_path_traversal_segment_semantics() {
        // Whole-segment `..` (bounded by separators / boundaries) → true.
        assert!(contains_path_traversal(".."));
        assert!(contains_path_traversal("../x"));
        assert!(contains_path_traversal("a/../b"));
        assert!(contains_path_traversal("a/.."));
        assert!(contains_path_traversal("a\\..\\b"));
        // `..` embedded in a longer name is NOT a traversal.
        assert!(!contains_path_traversal("..beta"));
        assert!(!contains_path_traversal("x..y"));
        assert!(!contains_path_traversal("v2..beta/x"));
        assert!(!contains_path_traversal("a/b/c"));
        assert!(!contains_path_traversal(""));
    }

    #[test]
    fn allowed_working_path_iterates_dirs() {
        let r = roots();
        let dirs = vec![PathBuf::from("/proj"), PathBuf::from("/extra/work")];
        // Inside the first dir.
        assert!(path_in_allowed_working_path(
            Path::new("/proj/src/x.rs"),
            &dirs,
            &r
        ));
        // Inside the second (additional) dir.
        assert!(path_in_allowed_working_path(
            Path::new("/extra/work/y.rs"),
            &dirs,
            &r
        ));
        // Inside neither.
        assert!(!path_in_allowed_working_path(
            Path::new("/nope/z.rs"),
            &dirs,
            &r
        ));
        // Empty working-dir list → never contained.
        assert!(!path_in_allowed_working_path(
            Path::new("/proj/src/x.rs"),
            &[],
            &r
        ));
    }

    #[cfg(unix)]
    #[test]
    fn allowed_working_path_checks_resolved_symlink_target() {
        let tmp = unique_temp_dir("resolved_symlink_target");
        let workspace = tmp.join("workspace");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };

        assert!(
            !path_in_allowed_working_path(&workspace.join("link/secret.txt"), &[workspace], &roots),
            "resolved target outside the working dir must be rejected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn allowed_working_path_checks_nonexistent_tail_under_symlink_parent() {
        let tmp = unique_temp_dir("nonexistent_symlink_tail");
        let workspace = tmp.join("workspace");
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };

        assert!(
            !path_in_allowed_working_path(
                &workspace.join("link/newdir/newfile.txt"),
                &[workspace],
                &roots
            ),
            "a non-existent tail beneath a symlinked parent must still resolve outside"
        );
    }

    /// SECURITY (ultra-review HIGH): a DANGLING symlink — one whose target does
    /// NOT exist yet — is exactly the exploitable case: a link you can CREATE a
    /// file through. `canonicalize` fails on it (ENOENT), so the resolver must
    /// `readlink` to the outside target rather than skip to the parent and
    /// re-attach the link name (which kept it lexically inside the workspace and
    /// evaded the containment ask).
    #[cfg(unix)]
    #[test]
    fn allowed_working_path_rejects_write_through_dangling_symlink() {
        let tmp = unique_temp_dir("dangling_symlink");
        let workspace = tmp.join("workspace");
        std::fs::create_dir_all(workspace.join("build")).unwrap();
        // Target does NOT exist — a dangling link pointing outside the workspace.
        let outside_target = tmp.join("outside").join("cron.d").join("job");
        std::os::unix::fs::symlink(&outside_target, workspace.join("build/out")).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };

        // The resolver must surface the outside target…
        let resolved = resolve_dangling_symlink(&workspace.join("build/out"))
            .expect("dangling link resolves to its target");
        assert!(
            !resolved.starts_with(&workspace),
            "resolved dangling target must be OUTSIDE the workspace, got {resolved:?}"
        );
        // …so a write through it is NOT contained (the exploit is blocked).
        assert!(
            !path_in_allowed_working_path(
                &workspace.join("build/out"),
                &[workspace.clone()],
                &roots
            ),
            "writing through a dangling symlink to an outside target must not be contained"
        );

        // A dangling link whose target is INSIDE the workspace stays contained
        // (no over-ask on a legitimate not-yet-created in-workspace file).
        let inside_target = workspace.join("data").join("real.txt");
        std::os::unix::fs::symlink(&inside_target, workspace.join("build/in")).unwrap();
        assert!(
            path_in_allowed_working_path(&workspace.join("build/in"), &[workspace], &roots),
            "a dangling link to an in-workspace target stays contained"
        );
    }

    #[cfg(unix)]
    #[test]
    fn allowed_working_path_skips_fail_closed_working_dir_if_another_contains_target() {
        let tmp = unique_temp_dir("skip_fail_closed_workdir");
        let workspace = tmp.join("workspace");
        let cycle = tmp.join("cycle");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::os::unix::fs::symlink(&cycle, &cycle).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };
        let target = workspace.join("src/lib.rs");

        assert_eq!(
            permission_paths_to_check(&cycle, &roots),
            PermissionPathForms::FailClosed
        );
        assert!(
            path_in_allowed_working_path(&target, &[cycle, workspace], &roots),
            "an unresolvable working-dir entry must not poison a valid containing dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn allowed_working_path_rejects_when_all_working_dirs_fail_closed() {
        let tmp = unique_temp_dir("all_fail_closed_workdirs");
        let workspace = tmp.join("workspace");
        let cycle = tmp.join("cycle");
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::os::unix::fs::symlink(&cycle, &cycle).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };

        assert!(
            !path_in_allowed_working_path(&workspace.join("src/lib.rs"), &[cycle], &roots),
            "containment must still deny when every working-dir entry is unresolvable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn allowed_working_path_fail_closes_unresolvable_target_path() {
        let tmp = unique_temp_dir("fail_closed_target_path");
        let workspace = tmp.join("workspace");
        let cycle = tmp.join("cycle");
        std::fs::create_dir_all(&workspace).unwrap();
        std::os::unix::fs::symlink(&cycle, &cycle).unwrap();

        let roots = FsRoots {
            cwd: workspace.clone(),
            home: None,
            lingxi_home: tmp.join(".lingxi"),
        };

        assert!(
            !path_in_allowed_working_path(&cycle, &[workspace], &roots),
            "an unresolvable target path must keep fail-closed behavior"
        );
    }

    // ---- HP-7: gitignore `!` negation in permission rule patterns ----------

    fn kind_of(input: &str, pattern: &str, source: PermissionRuleSource) -> RulePatternMatch {
        test_rule_pattern(
            input,
            pattern,
            source,
            crate::rule::PermissionBehavior::Deny,
            &roots(),
        )
    }

    #[test]
    fn a_negated_pattern_reports_its_polarity_instead_of_a_match() {
        // claude-code hands `!src/public/**` to the `ignore` library verbatim,
        // where it is a gitignore NEGATION over that source's matcher. The
        // pattern still has to COVER the path — it just cancels instead of
        // matching.
        assert_eq!(
            kind_of(
                "/proj/src/public/index.html",
                "!src/public/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
            ),
            RulePatternMatch::Negated
        );
        // A path the negation does not cover is simply unmatched.
        assert_eq!(
            kind_of(
                "/proj/src/secret.rs",
                "!src/public/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
            ),
            RulePatternMatch::NoMatch
        );
        // The source-blind wrapper keeps the conservative answer: a caller that
        // cannot see the rest of the settings source must not read a negation
        // as a match.
        assert!(!path_matches_rule_pattern(
            "/proj/src/public/index.html",
            "!src/public/**",
            PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project),
            crate::rule::PermissionBehavior::Deny,
            &roots(),
        ));
    }

    #[test]
    fn a_negation_is_rooted_at_the_cwd_because_the_bang_hides_the_root_sigil() {
        // `!` is not one of `$We`'s root sigils (`/`, `~/`, `./`), so upstream
        // resolves a `!`-prefixed pattern against the CWD — even when the text
        // after the `!` looks absolute. Stripping the `!` before the root
        // resolution would silently re-root this at `/etc` and let a project
        // file un-deny `/etc/passwd`.
        assert_eq!(
            kind_of(
                "/etc/passwd",
                "!/etc/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
            ),
            RulePatternMatch::NoMatch
        );
        // Same spelling under the cwd DOES resolve — proving the assertion
        // above is about the ROOT, not about the pattern failing to compile.
        assert_eq!(
            kind_of(
                "/proj/etc/passwd",
                "!/etc/**",
                PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
            ),
            RulePatternMatch::Negated
        );
    }

    #[test]
    fn a_bare_bang_covers_nothing() {
        // `Ki`'s `/^!\s*$/` guard: "a negation of every path" is dropped.
        // ⚠️ This pins BEHAVIOUR, not the guard — with the explicit early
        // return removed the `ignore` crate also treats the empty remainder as
        // a blank line and matches nothing, so seeding the guard away does not
        // redden this. The guard is kept because it states the oracle's rule
        // rather than leaning on a third-party blank-line convention.
        for pattern in ["!", "!  "] {
            assert_eq!(
                kind_of(
                    "/proj/src/main.rs",
                    pattern,
                    PermissionRuleSource::Settings(lingxi_core::types::SettingsScope::Project)
                ),
                RulePatternMatch::NoMatch,
                "bare {pattern:?} must not cover anything"
            );
        }
    }
}
