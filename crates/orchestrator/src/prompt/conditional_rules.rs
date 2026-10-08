//! §F lazy conditional-rule activation: path-gated LINGXI.md rules (those
//! carrying `paths:` globs) that activate when an edited/opened file matches
//! their globs.
//!
//! 1:1 with claude-code `processConditionedMdRules` (claudemd.ts:1354-1397) +
//! the `getNestedMemoryAttachmentsForFile` render seam (attachments.ts:1792).
//! A conditional rule is loaded eagerly into NEITHER the system prompt nor the
//! eager memory block; instead, once a touched file matches its globs, the rule
//! is injected ONCE as a per-turn, transient `<system-reminder>` meta user
//! message (the `nested_memory` attachment, messages.ts:3700-3707) and never
//! re-injected (the sent-set dedup, mirroring TS `loadedNestedMemoryPaths`).
//!
//! This module is the (no-orchestrator-state) core: [`base_dir`] derives a
//! rule's match root, [`rule_matches_touched_file`] performs the gitignore-style
//! glob test (with a cc 2.1.198 realpath symlink fallback in
//! [`relative_path_for_match`] — the only disk access here), and
//! [`render_reminder`] formats a matched rule. The orchestrator
//! ([`crate::conversation::ConversationOrchestrator::nested_memory_reminder_messages`])
//! owns fresh rule selection + the session sent-set and drives these helpers.
#![forbid(unsafe_code)]

use crate::prompt::MemoryFile;
use memory::lingxi_md::LingxiMdTier;
use std::path::{Path, PathBuf};

/// Derive the directory a conditional rule's globs are resolved against, 1:1
/// with claudemd.ts:1376-1380.
///
/// - **Project** tier: globs are relative to the directory CONTAINING `.claude`.
///   A project rule lives at `<base>/.lingxi/rules/x.md`, so the base is the
///   rule file's grandparent's parent — `dirname(dirname(dirname(path)))` (TS
///   `dirname(dirname(rulesDir))`, where `rulesDir = <base>/.lingxi/rules`).
/// - **Managed / User / Local** tiers: globs are relative to the original cwd
///   (`getOriginalCwd()`), passed in as `cwd`.
///
/// Returns `None` for a Project rule whose path has fewer than three ancestors
/// (a malformed/non-`.lingxi/rules/` location) — such a rule can match nothing.
#[must_use]
pub fn base_dir(rule_path: &Path, tier: LingxiMdTier, cwd: &Path) -> Option<PathBuf> {
    match tier {
        LingxiMdTier::Project => {
            // path = <base>/.lingxi/rules/x.md
            //   parent()        -> <base>/.lingxi/rules
            //   .parent()       -> <base>/.claude
            //   .parent()       -> <base>
            rule_path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(Path::to_path_buf)
        }
        LingxiMdTier::Managed | LingxiMdTier::User | LingxiMdTier::Local => Some(cwd.to_path_buf()),
    }
}

/// Lexical relative-path core (no disk access): the path of `touched` relative
/// to `base_dir` with claude-code's guards (claudemd.ts:1382-1393):
///
/// - if `touched` is absolute → relativize against `base_dir`; else use it as-is
/// - return `None` (no match) if the relative path is empty, starts with `..`
///   (escapes the base), or is still absolute after relativizing
///
/// The returned string uses forward slashes (gitignore semantics; also the only
/// separator on the POSIX targets this ships on).
#[must_use]
fn lexical_relative(touched: &Path, base_dir: &Path) -> Option<String> {
    let rel: PathBuf = if touched.is_absolute() {
        // `strip_prefix` is the lexical `relative(base, touched)` analog. When
        // `touched` is not under `base_dir` it fails → no match (TS would get a
        // `..`-prefixed path, which the guard below also rejects).
        touched.strip_prefix(base_dir).ok()?.to_path_buf()
    } else {
        touched.to_path_buf()
    };

    let rel_str = rel.to_string_lossy().replace('\\', "/");
    // Guards: empty, `..`-escape, or still-absolute → no match.
    if rel_str.is_empty()
        || rel_str == ".."
        || rel_str.starts_with("../")
        || rel_str.starts_with('/')
    {
        return None;
    }
    Some(rel_str)
}

/// Compute the path of `touched` relative to `base_dir` for glob matching,
/// 1:1 with the binary conditional-rule filter `pqt` (claudemd.ts).
///
/// The primary computation is [`lexical_relative`] (a pure `path.relative`).
/// The cc 2.1.198 fix adds a **symlink fallback** (binary: `if(isAbsolute(e) &&
/// (!a||a.startsWith("..")||isAbsolute(a))){ let l=dirname(e),{resolvedPath:c}=
/// jd(fs,l); if(c!==l) a=relative(i,join(c,basename(e))) }`): when `touched` is
/// absolute AND the lexical relative failed (empty / `..`-escape / absolute),
/// resolve the REALPATH of the file's directory (`jd` → `realpathSync`) and, only
/// if it differs (`c!==l`, i.e. a symlink was resolved), recompute the relative
/// path from the canonical directory. So a file reached through a symlinked path
/// that resolves back under the rule's base dir still matches its canonical
/// location. Like the binary, ONLY the touched file's directory is realpath'd —
/// `base_dir` is assumed canonical (it derives from the realpath'd
/// `getOriginalCwd`). A non-existent directory (canonicalize errors → `jd`
/// returns the path unchanged) skips the fallback.
#[must_use]
fn relative_path_for_match(touched: &Path, base_dir: &Path) -> Option<String> {
    if let Some(rel) = lexical_relative(touched, base_dir) {
        return Some(rel);
    }
    // Lexical relativization failed. Symlink fallback (absolute paths only).
    if !touched.is_absolute() {
        return None;
    }
    let parent = touched.parent()?;
    let name = touched.file_name()?;
    // `jd(fs, dirname(e))` → realpathSync, falling back to the input on error.
    let canonical_parent = std::fs::canonicalize(parent).ok()?;
    // `if(c!==l)`: only recompute when a symlink was actually resolved.
    if canonical_parent == parent {
        return None;
    }
    lexical_relative(&canonical_parent.join(name), base_dir)
}

/// Test whether `touched` matches the conditional `rule`'s globs, 1:1 with the
/// final line of `processConditionedMdRules` (claudemd.ts:1395):
/// `ignore().add(file.globs).ignores(relativePath)`.
///
/// Mirrors the `ignore`-crate idiom used by `permission::filesystem`
/// (`path_matches_rule_pattern`): build ONE `GitignoreBuilder` rooted at `/`,
/// add every glob line, and test `/<relative>` with
/// `matched_path_or_any_parents(..).is_ignore()`. `matched_path_or_any_parents`
/// (not the parent-blind `matched`) is the faithful analogue of npm `ignore`'s
/// `.ignores()`, which matches a file when an ANCESTOR dir matches — so a glob
/// like `src` (a `paths: src/**` with the `/**` already stripped upstream by
/// `parse_frontmatter_paths`) matches `src/x.rs` via its `src` parent.
///
/// Returns `false` when the rule carries no globs, when the relativized path is
/// guarded out, or when no glob matches.
#[must_use]
pub fn rule_matches_touched_file(rule: &MemoryFile, touched: &Path, cwd: &Path) -> bool {
    let Some(globs) = rule.globs.as_ref() else {
        return false; // unconditional file — never matched lazily
    };
    if globs.is_empty() {
        return false;
    }
    let Some(base) = base_dir(&rule.path, rule.tier, cwd) else {
        return false;
    };
    let Some(rel) = relative_path_for_match(touched, &base) else {
        return false;
    };

    let mut builder = ignore::gitignore::GitignoreBuilder::new("/");
    for g in globs {
        // A pattern the builder can't parse (unbalanced char class, …) is
        // skipped; npm `ignore` is more lenient, so surface it rather than
        // silently drop the whole rule.
        if let Err(e) = builder.add_line(None, g) {
            tracing::warn!(
                pattern = %g,
                error = %e,
                "conditional-rule glob is unparseable; skipping this pattern"
            );
        }
    }
    let Ok(gitignore) = builder.build() else {
        return false;
    };
    let target = Path::new("/").join(&rel);
    gitignore
        .matched_path_or_any_parents(&target, false)
        .is_ignore()
}

/// Render a matched conditional rule as the body of a per-turn meta user
/// message, 1:1 with the `nested_memory` attachment renderer
/// (messages.ts:3700-3707): `Contents of {path}:\n\n{body}` wrapped in a
/// `<system-reminder>` envelope (messages.ts:3097 `wrapInSystemReminder`).
///
/// NOTE the shape differs from the EAGER memory block ([`memory_block::format`],
/// which carries the `MEMORY_INSTRUCTION_PROMPT` preamble + a per-tier
/// description): a lazily-activated rule is a bare
/// `Contents of {path}:\n\n{body}` with NO tier description, matching the TS
/// `nested_memory` case exactly.
///
/// [`memory_block::format`]: crate::prompt::memory_block::format
#[must_use]
pub fn render_reminder(rule: &MemoryFile) -> String {
    let inner = format!("Contents of {}:\n\n{}", rule.path.display(), rule.body);
    format!("<system-reminder>\n{inner}\n</system-reminder>")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(path: &str, tier: LingxiMdTier, globs: Option<Vec<&str>>) -> MemoryFile {
        MemoryFile {
            parent: None,
            source_content: None,
            path: PathBuf::from(path),
            body: "RULE BODY".into(),
            is_local_override: tier == LingxiMdTier::Local,
            tier,
            globs: globs.map(|v| v.into_iter().map(String::from).collect()),
            raw_content: "RULE BODY".into(),
            content_differs_from_disk: false,
        }
    }

    #[test]
    fn base_dir_project_is_three_levels_up_from_rule_file() {
        // <base>/.lingxi/rules/x.md -> <base>
        let b = base_dir(
            Path::new("/proj/.lingxi/rules/x.md"),
            LingxiMdTier::Project,
            Path::new("/some/cwd"),
        );
        assert_eq!(b, Some(PathBuf::from("/proj")));
    }

    #[test]
    fn base_dir_user_managed_local_is_cwd() {
        let cwd = Path::new("/work/repo");
        for tier in [
            LingxiMdTier::User,
            LingxiMdTier::Managed,
            LingxiMdTier::Local,
        ] {
            let b = base_dir(Path::new("/home/u/.lingxi/rules/y.md"), tier, cwd);
            assert_eq!(b, Some(cwd.to_path_buf()), "tier {tier:?}");
        }
    }

    #[test]
    fn matches_project_rule_against_relative_touched_file() {
        // paths: src/** -> globs ["src"]; touched "src/x.rs" matches via parent.
        let r = rule(
            "/proj/.lingxi/rules/r.md",
            LingxiMdTier::Project,
            Some(vec!["src"]),
        );
        assert!(rule_matches_touched_file(
            &r,
            Path::new("src/x.rs"),
            Path::new("/proj")
        ));
        assert!(!rule_matches_touched_file(
            &r,
            Path::new("docs/y.md"),
            Path::new("/proj")
        ));
    }

    #[test]
    fn matches_project_rule_against_absolute_touched_file() {
        // An absolute touched path is relativized against the rule's base dir.
        let r = rule(
            "/proj/.lingxi/rules/r.md",
            LingxiMdTier::Project,
            Some(vec!["src"]),
        );
        assert!(rule_matches_touched_file(
            &r,
            Path::new("/proj/src/deep/x.rs"),
            Path::new("/proj"),
        ));
        // A file outside the base dir relativizes to a `..`-escape → no match.
        assert!(!rule_matches_touched_file(
            &r,
            Path::new("/elsewhere/src/x.rs"),
            Path::new("/proj"),
        ));
    }

    #[test]
    fn user_rule_resolves_globs_against_cwd() {
        let r = rule(
            "/home/u/.lingxi/rules/r.md",
            LingxiMdTier::User,
            Some(vec!["lib"]),
        );
        let cwd = Path::new("/work/repo");
        assert!(rule_matches_touched_file(
            &r,
            Path::new("/work/repo/lib/a.rs"),
            cwd
        ));
        assert!(rule_matches_touched_file(&r, Path::new("lib/a.rs"), cwd));
        assert!(!rule_matches_touched_file(&r, Path::new("src/a.rs"), cwd));
    }

    #[test]
    fn glob_extension_pattern_matches() {
        // paths: **/*.rs -> globs ["**/*.rs"]; matches any .rs at any depth.
        let r = rule(
            "/proj/.lingxi/rules/r.md",
            LingxiMdTier::Project,
            Some(vec!["**/*.rs"]),
        );
        assert!(rule_matches_touched_file(
            &r,
            Path::new("a/b/c.rs"),
            Path::new("/proj")
        ));
        assert!(!rule_matches_touched_file(
            &r,
            Path::new("a/b/c.md"),
            Path::new("/proj")
        ));
    }

    #[test]
    fn unconditional_rule_never_matches() {
        let r = rule("/proj/LINGXI.md", LingxiMdTier::Project, None);
        assert!(!rule_matches_touched_file(
            &r,
            Path::new("src/x.rs"),
            Path::new("/proj")
        ));
    }

    /// cc 2.1.198 fix: a file reached through a SYMLINKED path whose canonical
    /// location is under the rule's base dir MATCHES. Lexically `<T>/link/x.rs`
    /// escapes the base `<T>/proj` (a `..`), but `realpath(dirname)` resolves it
    /// to `<T>/proj/realsrc/x.rs` → `realsrc/x.rs` → matches glob `realsrc`.
    #[cfg(unix)]
    #[test]
    fn symlinked_touched_file_matches_via_realpath_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        // Canonicalize the root so the base dir is canonical (as production's
        // realpath'd getOriginalCwd is) — otherwise macOS /var→/private/var
        // would make the resolved touched path escape the base.
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let base = root.join("proj");
        let real_dir = base.join("realsrc");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::write(real_dir.join("x.rs"), "fn main(){}").unwrap();
        // A symlink OUTSIDE the base that points INTO it.
        let link = root.join("link");
        std::os::unix::fs::symlink(&real_dir, &link).unwrap();

        let r = rule(
            &base.join(".lingxi/rules/r.md").to_string_lossy(),
            LingxiMdTier::Project,
            Some(vec!["realsrc"]),
        );
        // Touched via the symlink: lexically `<root>/link/x.rs` is NOT under
        // `<base>`, so only the realpath fallback can match it.
        let touched_via_link = link.join("x.rs");
        assert!(
            rule_matches_touched_file(&r, &touched_via_link, &base),
            "symlinked path resolving under the base must match its canonical location"
        );
        // Sanity: the canonical path matches too (lexical, no fallback needed).
        assert!(rule_matches_touched_file(&r, &real_dir.join("x.rs"), &base));
    }

    /// A symlink resolving OUTSIDE the base dir still does NOT match — the fix
    /// only rescues files whose canonical path is genuinely under the base.
    #[cfg(unix)]
    #[test]
    fn symlink_resolving_outside_base_still_does_not_match() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let base = root.join("proj");
        std::fs::create_dir_all(base.join("realsrc")).unwrap();
        // Real dir OUTSIDE the base.
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.rs"), "x").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let r = rule(
            &base.join(".lingxi/rules/r.md").to_string_lossy(),
            LingxiMdTier::Project,
            Some(vec!["realsrc"]),
        );
        // Resolves to `<root>/outside/x.rs` → relative to base is a `..`-escape.
        assert!(
            !rule_matches_touched_file(&r, &link.join("x.rs"), &base),
            "symlink resolving outside the base must not match"
        );
    }

    /// The realpath fallback is a RESCUE only: a file whose lexical relative
    /// already succeeds is never re-resolved (no disk access changes the match),
    /// and a non-existent absolute path (canonicalize errors) simply fails.
    #[test]
    fn nonexistent_absolute_path_does_not_panic_and_does_not_match() {
        let r = rule(
            "/proj/.lingxi/rules/r.md",
            LingxiMdTier::Project,
            Some(vec!["src"]),
        );
        // Absolute, outside base, non-existent → lexical fails, canonicalize
        // errors → no match (and no panic).
        assert!(!rule_matches_touched_file(
            &r,
            Path::new("/nope/does/not/exist/x.rs"),
            Path::new("/proj"),
        ));
    }

    #[test]
    fn render_reminder_is_bare_contents_in_system_reminder() {
        let r = rule(
            "/proj/.lingxi/rules/r.md",
            LingxiMdTier::Project,
            Some(vec!["src"]),
        );
        let out = render_reminder(&r);
        assert_eq!(
            out,
            "<system-reminder>\nContents of /proj/.lingxi/rules/r.md:\n\nRULE BODY\n</system-reminder>"
        );
        // No eager-block preamble / tier description.
        assert!(!out.contains("Codebase and user instructions"));
        assert!(!out.contains("project instructions, checked into"));
    }
}
