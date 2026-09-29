//! Dir-up walker: managed → user-home → parents → cwd.

use std::path::{Path, PathBuf};

/// Filename of the project memory file (case-sensitive).
pub const FILE_NAME: &str = branding::MEMORY_FILE;
/// Filename of the local-override memory file.
pub const LOCAL_OVERRIDE_NAME: &str = branding::MEMORY_LOCAL_FILE;

/// The two ancestor lists a nested-memory lookup walks for `file`.
///
/// See [`split_ancestors`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestors {
    /// Directories from `file`'s own directory up to, but NOT including, `cwd`
    /// — ordered outermost-first (closest to `cwd` first).
    pub nested: Vec<PathBuf>,
    /// `cwd` and each of ITS ancestors up to the filesystem root, ordered
    /// outermost-first (root first).
    pub cwd_level: Vec<PathBuf>,
}

/// Split `file`'s ancestor directories into the two lists claude-code walks when
/// loading nested memory.
///
/// 1:1 port of `Aop` (2.1.220 @237714114):
///
/// ```js
/// function Aop(e,t){
///   let r=Vx.dirname(Vx.resolve(e));
///   if(!r.startsWith(t)) try{ let s=Jt().realpathSync(r); if(s.startsWith(t)) r=s }catch{}
///   let n=[],o=r;
///   while(o!==t&&o!==Vx.parse(o).root){ if(o.startsWith(t)) n.push(o); o=Vx.dirname(o) }
///   n.reverse();
///   let i=[]; o=t;
///   while(o!==Vx.parse(o).root) i.push(o), o=Vx.dirname(o);
///   return i.reverse(),{nestedDirs:n,cwdLevelDirs:i}
/// }
/// ```
///
/// Three details that are easy to lose:
///
/// - The **realpath fallback**. When `file`'s directory is not LEXICALLY under
///   `cwd`, it is retried through symlinks and only adopted if that brings it
///   under `cwd`. On macOS a `/var/...` cwd really lives at `/private/var/...`,
///   so without this the nested list comes back empty for every such session.
///   Failure is swallowed — a missing directory just means "no nested dirs".
/// - The `if(o.startsWith(t))` guard INSIDE the loop is not redundant with the
///   loop condition: it drops any component that escaped `cwd`.
/// - `cwd_level` includes `cwd` itself and stops BEFORE the root, whereas
///   `nested` stops before `cwd`. The two lists are disjoint.
#[must_use]
pub fn split_ancestors(file: &Path, cwd: &Path) -> Ancestors {
    let mut dir = file.parent().map_or_else(PathBuf::new, Path::to_path_buf);
    if !dir.starts_with(cwd) {
        if let Ok(real) = std::fs::canonicalize(&dir) {
            if real.starts_with(cwd) {
                dir = real;
            }
        }
    }

    let mut nested: Vec<PathBuf> = Vec::new();
    let mut cur = dir;
    while cur != cwd && cur.parent().is_some() {
        if cur.starts_with(cwd) {
            nested.push(cur.clone());
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    nested.reverse();

    let mut cwd_level: Vec<PathBuf> = Vec::new();
    let mut cur = cwd.to_path_buf();
    while cur.parent().is_some() {
        cwd_level.push(cur.clone());
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
    cwd_level.reverse();

    Ancestors { nested, cwd_level }
}

/// Resolve the USER-tier `.claude` config directory, honoring
/// `$LINGXI_CONFIG_DIR` (claude-code `tr()`: `process.env.LINGXI_CONFIG_DIR ??
/// join(homedir(), ".lingxi")`). When the env var is SET its value is the config
/// dir verbatim — including a set-but-EMPTY value, which `??` honors (config-home
/// then resolves cwd-relative), exactly as claude-code v2.1.181 does; only an
/// UNSET var falls back to `<home>/.claude`. `home` is the caller-supplied home
/// (production passes `dirs::home_dir()`; tests pass a temp dir) so the walk
/// stays hermetic while the env override wins. (NOTE: the GLOBAL `~/.lingxi.json`
/// resolver in `migrations::global_config` uses `||` and so treats empty as
/// unset — that asymmetry is itself faithful to claude-code.)
#[must_use]
pub fn user_config_dir(home: &Path) -> PathBuf {
    resolve_user_config_dir(home, std::env::var_os(branding::CONFIG_DIR_ENV))
}

/// Pure core of [`user_config_dir`] — the `$LINGXI_CONFIG_DIR` value is injected
/// so the resolution logic is testable without mutating process env.
fn resolve_user_config_dir(home: &Path, config_dir_env: Option<std::ffi::OsString>) -> PathBuf {
    match config_dir_env {
        // `??`: a SET value wins verbatim, even when empty (claude-code resolves
        // it cwd-relative); only an UNSET var falls back to `<home>/.claude`.
        Some(dir) => PathBuf::from(dir),
        None => home.join(DOT_LINGXI),
    }
}

/// Env var that overrides the managed-settings directory for tests/demos so
/// hermetic hierarchy tests stay machine-independent. When set, its value is
/// used verbatim as the managed dir; otherwise [`managed_path`] returns the
/// platform default. Loosely mirrors claude-code's
/// `CLAUDE_CODE_MANAGED_SETTINGS_PATH` override (managedPath.ts:11-15).
pub const MANAGED_DIR_ENV: &str = "LINGXI_MANAGED_DIR";

/// Resolve the managed-settings directory.
///
/// Ports claude-code `getManagedFilePath` (settings/managedPath.ts:8-25): a
/// per-platform absolute system path holding enterprise/managed policy. The
/// `<managed>/LINGXI.md` + `<managed>/.lingxi/rules/**` discovered under it form
/// the always-on Managed tier.
///
/// - **macOS**:   `/Library/Application Support/LingXi`
/// - **Windows**: `C:\Program Files\LingXi`
/// - **other**:   `/etc/lingxi`
///
/// The [`MANAGED_DIR_ENV`] environment variable overrides the platform default
/// (used by hermetic tests so they don't depend on a real system path).
#[must_use]
pub fn managed_path() -> PathBuf {
    if let Some(over) = std::env::var_os(MANAGED_DIR_ENV) {
        if !over.is_empty() {
            return PathBuf::from(over);
        }
    }
    if cfg!(target_os = "macos") {
        PathBuf::from(branding::MANAGED_DIR_MACOS)
    } else if cfg!(target_os = "windows") {
        PathBuf::from(branding::MANAGED_DIR_WINDOWS)
    } else {
        PathBuf::from(branding::MANAGED_DIR_UNIX)
    }
}

/// One discovered LINGXI.md (or local override) location, post-walk.
#[derive(Debug, Clone)]
pub struct HierarchyEntry {
    /// Absolute path to the file on disk.
    pub path: PathBuf,
    /// Whether this is a `LINGXI.local.md` (true) or `LINGXI.md` (false).
    pub is_local_override: bool,
    /// Whether the actual filename's bytes matched `FILE_NAME` exactly
    /// (false on case-insensitive filesystems that lowercased it).
    pub exact_case: bool,
    /// Which LINGXI.md tier this file was discovered in. Drives the injection
    /// description (`getLingxiMds`, claudemd.ts:1168-1186) and the `@import`
    /// external-include policy (only [`super::LingxiMdTier::User`] gets
    /// unconditional external includes).
    pub tier: super::LingxiMdTier,
}

/// Snapshot of discovered memory-file locations, innermost-first
/// (cwd's files, then each parent's, then User). Consumers reverse this
/// to reach claude-code's splice order. See [`walk`].
#[derive(Debug, Default)]
pub struct Hierarchy {
    /// Discovered entries in walk order.
    pub entries: Vec<HierarchyEntry>,
}

/// Directory name used for nested config (`.lingxi/` → `.lingxi/`).
const DOT_LINGXI: &str = branding::DOT_DIR;
/// Subdirectory under `.lingxi/` holding `*.md` rule files.
const RULES_DIR: &str = "rules";

/// Discover the claude-code memory-file set, in `getMemoryFiles`
/// tier order (`utils/claudemd.ts:790-934`):
///   1. **Managed**: `<managed>/LINGXI.md`, then `<managed>/.lingxi/rules/**.md`
///      (always loaded, lowest priority — spliced first).
///   2. **User**:    `<home>/.lingxi/LINGXI.md`, then `<home>/.lingxi/rules/**.md`.
///   3. **Project + Local**, from the filesystem root DOWN to `cwd`; per dir:
///      `LINGXI.md`, `.lingxi/LINGXI.md`, `.lingxi/rules/**.md`, `LINGXI.local.md`.
///
/// claude-code splices files in exactly that order (Managed first, the innermost
/// `cwd` last). The orchestrator (`prompt::memory_block`) **reverses** this
/// primitive's output to reach that splice order, so `walk` upholds an
/// innermost-first contract: it builds the list in claude-code splice order and
/// reverses it once at the end, yielding `cwd` … parents … User … Managed.
///
/// `managed_dir` is the managed-settings directory (typically
/// [`managed_path`]); pass `None` to skip the Managed tier entirely (hermetic
/// tests that don't exercise it). claude-code always probes Managed and never
/// settings-gates it (claudemd.ts:803-823); the `Option` here is purely a
/// test seam so unit tests need not touch an absolute system path.
///
/// Every file is emitted at most once, mirroring claude-code's shared
/// `processedPaths` set. Missing dirs/files are silently skipped (NOT an error).
///
/// Answers: what order `LINGXI.md` files are spliced into the prompt. One of
/// several orderings over these rungs; `lingxi_core::types::scope`'s module docs index
/// them all and say which question each answers.
#[must_use]
pub fn walk(cwd: &Path, home: &Path, managed_dir: Option<&Path>) -> Hierarchy {
    use super::LingxiMdTier;
    let mut out = Vec::new();
    let mut processed = std::collections::HashSet::new();

    // (1) Managed tier — `<managed>/LINGXI.md` + `<managed>/.lingxi/rules/**`.
    //     Always loaded, never settings-gated (claudemd.ts:803-823). Probed
    //     FIRST so that after the final reverse it sorts first in splice order.
    if let Some(managed) = managed_dir {
        emit_probe(
            managed,
            FILE_NAME,
            false,
            LingxiMdTier::Managed,
            &mut out,
            &mut processed,
        );
        collect_rules(
            &managed.join(DOT_LINGXI).join(RULES_DIR),
            LingxiMdTier::Managed,
            &mut out,
            &mut processed,
        );
    }

    // (2) User tier — `<config-home>/LINGXI.md` + `<config-home>/rules/**`, where
    //     config-home honors `$LINGXI_CONFIG_DIR` (else `<home>/.claude`). This
    //     keeps the loaded user-tier file in sync with `/memory`'s edit target.
    let user_dir = user_config_dir(home);
    emit_probe(
        &user_dir,
        FILE_NAME,
        false,
        LingxiMdTier::User,
        &mut out,
        &mut processed,
    );
    collect_rules(
        &user_dir.join(RULES_DIR),
        LingxiMdTier::User,
        &mut out,
        &mut processed,
    );

    // (3) Project + Local tier — filesystem root DOWN to cwd.
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur: Option<&Path> = Some(cwd);
    while let Some(dir) = cur {
        dirs.push(dir.to_path_buf());
        cur = dir.parent();
    }
    dirs.reverse(); // root → cwd
    for dir in &dirs {
        // Project: `LINGXI.md`, then `.lingxi/LINGXI.md`, then `.lingxi/rules/**`.
        emit_probe(
            dir,
            FILE_NAME,
            false,
            LingxiMdTier::Project,
            &mut out,
            &mut processed,
        );
        emit_probe(
            &dir.join(DOT_LINGXI),
            FILE_NAME,
            false,
            LingxiMdTier::Project,
            &mut out,
            &mut processed,
        );
        collect_rules(
            &dir.join(DOT_LINGXI).join(RULES_DIR),
            LingxiMdTier::Project,
            &mut out,
            &mut processed,
        );
        // Local override LAST within the directory (claude-code emits Local
        // after Project so it wins the model's recency attention).
        emit_probe(
            dir,
            LOCAL_OVERRIDE_NAME,
            true,
            LingxiMdTier::Local,
            &mut out,
            &mut processed,
        );
    }

    // `out` is now in claude-code splice order (Managed → User → root … → cwd).
    // Reverse to the innermost-first contract this primitive promises; the
    // orchestrator reverses again to restore the splice order.
    out.reverse();
    Hierarchy { entries: out }
}

/// Probe `dir` for a file named `want` (case-insensitive) and, when found and
/// not already emitted, push a [`HierarchyEntry`]. Mirrors a single
/// `processMemoryFile` call guarded by the shared `processedPaths` set.
/// Probe ONE directory for nested memory, in claude-code's `ffo` order.
///
/// 1:1 with `ffo` (2.1.220 @230809989):
///
/// ```js
/// if(o){ CLAUDE.md ; .claude/CLAUDE.md }
/// if(localSettings){ CLAUDE.local.md }
/// if(o){ .claude/rules  (unconditional, then conditional) }
/// ```
///
/// Every name comes from the `branding` constants ([`FILE_NAME`],
/// [`LOCAL_OVERRIDE_NAME`], [`DOT_LINGXI`], [`RULES_DIR`]) — this project's
/// files are `LINGXI.md` / `.lingxi`, NOT the oracle's literals.
///
/// ⚠️ This ORDER DIFFERS from [`walk`]'s per-directory order, which probes rules
/// BEFORE the local override. That is not a bug in either: `walk` ports the
/// eager hierarchy builder and is byte-parity tested, while this ports the
/// nested-memory probe. They must NOT be refactored into a shared body — doing
/// so silently changes one of them.
///
/// The unconditional/conditional split inside `rules` is one scan here rather
/// than the oracle's two (`ZPt` then `lfo`): [`collect_rules`] yields both and
/// `globs.is_some()` distinguishes them, so the caller splits. Callers MUST emit
/// the unconditional ones first to preserve the oracle's ordering.
pub fn probe_dir_nested(
    dir: &Path,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    emit_probe(
        dir,
        FILE_NAME,
        false,
        super::LingxiMdTier::Project,
        out,
        processed,
    );
    emit_probe(
        &dir.join(DOT_LINGXI),
        FILE_NAME,
        false,
        super::LingxiMdTier::Project,
        out,
        processed,
    );
    emit_probe(
        dir,
        LOCAL_OVERRIDE_NAME,
        true,
        super::LingxiMdTier::Local,
        out,
        processed,
    );
    collect_rules(
        &dir.join(DOT_LINGXI).join(RULES_DIR),
        super::LingxiMdTier::Project,
        out,
        processed,
    );
}

/// Probe the MANAGED tier's rules dir — the Managed half of claude-code `NLu`
/// (@230809780), which loads Managed + User rules for a trigger file.
///
/// Rules only: the tier's unconditional memory file is already in the eager
/// block, so only the `paths:`-gated half can be news for a touched file.
pub fn probe_managed_rules(
    managed_dir: &Path,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    collect_rules(
        &managed_dir.join(DOT_LINGXI).join(RULES_DIR),
        super::LingxiMdTier::Managed,
        out,
        processed,
    );
}

/// Probe the USER tier's rules dir — the User half of `NLu`.
///
/// Resolves through [`user_config_dir`], so `$LINGXI_CONFIG_DIR` is honored
/// exactly as the eager walk honors it.
pub fn probe_user_rules(
    home: &Path,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    collect_rules(
        &user_config_dir(home).join(RULES_DIR),
        super::LingxiMdTier::User,
        out,
        processed,
    );
}

/// Probe ONE cwd-level directory — claude-code `FLu` (@230810574), which loads
/// `.lingxi/rules` ONLY (no `LINGXI.md`, no local override) and, at the oracle,
/// only the CONDITIONAL half. The caller applies the conditional filter.
pub fn probe_dir_cwd_level(
    dir: &Path,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    collect_rules(
        &dir.join(DOT_LINGXI).join(RULES_DIR),
        super::LingxiMdTier::Project,
        out,
        processed,
    );
}

fn emit_probe(
    dir: &Path,
    want: &str,
    is_local: bool,
    tier: super::LingxiMdTier,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    if let Some(entry) = probe(dir, want, is_local, tier) {
        if processed.insert(entry.path.clone()) {
            out.push(entry);
        }
    }
}

/// Recursively collect every `*.md` file under `rules_dir`, mirroring
/// claude-code `processMdRules` (`utils/claudemd.ts:697-788`): descend into
/// subdirectories depth-first, guard against symlink cycles with a visited-dir
/// set, and silently ignore a missing / non-dir / permission-denied directory.
///
/// claude-code iterates entries in raw `readdir` order; that OS order is
/// unstable and cannot be pinned in a fixture, so the Rust port sorts entries
/// by file name for reproducibility. Only files whose name ends in `.md`
/// (case-sensitive, matching `endsWith('.md')`) are emitted, each at most once
/// via the shared `processed` set.
fn collect_rules(
    rules_dir: &Path,
    tier: super::LingxiMdTier,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
) {
    let mut visited = std::collections::HashSet::new();
    collect_rules_inner(rules_dir, tier, out, processed, &mut visited);
}

fn collect_rules_inner(
    rules_dir: &Path,
    tier: super::LingxiMdTier,
    out: &mut Vec<HierarchyEntry>,
    processed: &mut std::collections::HashSet<PathBuf>,
    visited: &mut std::collections::HashSet<PathBuf>,
) {
    // visitedDirs cycle guard: key on the canonical (symlink-resolved) path so
    // a symlink loop (A → B → A) terminates. Fall back to the raw path when it
    // can't be canonicalized (e.g. missing — read_dir below then bails).
    let key = std::fs::canonicalize(rules_dir).unwrap_or_else(|_| rules_dir.to_path_buf());
    if !visited.insert(key) {
        return;
    }

    // Missing / non-dir / permission-denied → silently ignored.
    let Ok(read) = std::fs::read_dir(rules_dir) else {
        return;
    };

    // Sort by file name for deterministic ordering (see fn docs).
    let mut items: Vec<std::fs::DirEntry> = read.flatten().collect();
    items.sort_by_key(std::fs::DirEntry::file_name);

    for ent in items {
        let path = ent.path();
        let Ok(ft) = ent.file_type() else { continue };
        // Resolve symlinks via stat() to classify the target, matching
        // processMdRules' safeResolvePath handling.
        let (is_dir, is_file) = if ft.is_symlink() {
            match std::fs::metadata(&path) {
                Ok(m) => (m.is_dir(), m.is_file()),
                Err(_) => continue,
            }
        } else {
            (ft.is_dir(), ft.is_file())
        };

        if is_dir {
            collect_rules_inner(&path, tier, out, processed, visited);
        } else if is_file
            && ent.file_name().to_string_lossy().ends_with(".md")
            && processed.insert(path.clone())
        {
            out.push(HierarchyEntry {
                path,
                is_local_override: false,
                exact_case: true,
                tier,
            });
        }
    }
}

fn probe(
    dir: &Path,
    want: &str,
    is_local: bool,
    tier: super::LingxiMdTier,
) -> Option<HierarchyEntry> {
    // Scan the directory and compare names case-insensitively. We rely on
    // the dirent listing (NOT `Path::is_file`) so that case-insensitive
    // filesystems (macOS APFS-default, Windows NTFS) still produce a
    // `exact_case == false` signal when the on-disk name differs from
    // `want` in case. The event hook lives in [`emit_case_mismatch`].
    let want_lc = want.to_ascii_lowercase();
    let read = std::fs::read_dir(dir).ok()?;
    for ent in read.flatten() {
        let name = ent.file_name();
        let s = name.to_string_lossy();
        if s.to_ascii_lowercase() == want_lc && ent.file_type().ok()?.is_file() {
            return Some(HierarchyEntry {
                path: ent.path(),
                is_local_override: is_local,
                exact_case: s == want,
                tier,
            });
        }
    }
    None
}

use std::sync::Arc;

/// Telemetry event name emitted when a LINGXI.md is found under a
/// different case (e.g. `claude.md` on macOS APFS).
pub const TENGU_MEMORY_CASE_MISMATCH: &str = "tengu_memory_case_mismatch";

/// Emit `tengu_memory_case_mismatch` for entries where `exact_case == false`.
///
/// `actual` is the filename component (NOT the full path); the full path is
/// PII-tagged via `_PROTO_path`. No-op when `bus` is `None`.
pub async fn emit_case_mismatch(
    bus: Option<&Arc<telemetry::AnalyticsBus>>,
    path: &std::path::Path,
    actual: &str,
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
    md.insert(
        "actual".into(),
        telemetry::sink::AnalyticsValue::String(
            telemetry::pii::Verified::assert_safe(actual.to_string()).into_inner(),
        ),
    );
    bus.log_event(TENGU_MEMORY_CASE_MISMATCH, md).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &std::path::Path, name: &str) {
        fs::write(dir.join(name), b"# notes\n").unwrap();
    }

    #[test]
    fn user_config_dir_env_override_else_home() {
        use std::ffi::OsString;
        // A set `$LINGXI_CONFIG_DIR` is the `.claude` dir verbatim.
        assert_eq!(
            resolve_user_config_dir(Path::new("/h"), Some(OsString::from("/explicit/cfg"))),
            PathBuf::from("/explicit/cfg")
        );
        // A set-but-EMPTY `$LINGXI_CONFIG_DIR` is honored verbatim (claude-code
        // `??` resolves it cwd-relative), NOT treated as unset.
        assert_eq!(
            resolve_user_config_dir(Path::new("/h"), Some(OsString::new())),
            PathBuf::from("")
        );
        // Unset → `<home>/.claude` (the pre-change default).
        assert_eq!(
            resolve_user_config_dir(Path::new("/h"), None),
            PathBuf::from("/h/.lingxi")
        );
    }

    #[test]
    fn walks_cwd_then_parents_then_home() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".lingxi")).unwrap();
        touch(&home.join(".lingxi"), "LINGXI.md");
        let outer = tmp.path().join("repo");
        let inner = outer.join("pkg");
        fs::create_dir_all(&inner).unwrap();
        touch(&outer, "LINGXI.md");
        touch(&inner, "LINGXI.md");

        let h = walk(&inner, &home, None);
        let paths: Vec<_> = h.entries.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                inner.join("LINGXI.md"),
                outer.join("LINGXI.md"),
                home.join(".lingxi").join("LINGXI.md"),
            ],
            "walk order must be cwd → parents → home"
        );
        assert!(h.entries.iter().all(|e| !e.is_local_override));
    }

    #[test]
    fn surfaces_local_override_alongside_canonical() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".lingxi")).unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        touch(&cwd, "LINGXI.md");
        touch(&cwd, "LINGXI.local.md");

        let h = walk(&cwd, &home, None);
        let kinds: Vec<_> = h.entries.iter().map(|e| e.is_local_override).collect();
        // LINGXI.local.md MUST come before LINGXI.md at the same level
        // so it can shadow the canonical entry.
        assert_eq!(kinds, vec![true, false]);
    }

    #[test]
    fn stops_at_filesystem_root_no_panic() {
        // walk(&Path::new("/"), &home) must not loop or panic
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".lingxi")).unwrap();
        let _h = walk(std::path::Path::new("/"), &home, None);
    }

    #[test]
    fn missing_files_produce_empty_hierarchy() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("empty");
        fs::create_dir_all(&cwd).unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap(); // .claude does NOT exist
        let h = walk(&cwd, &home, None);
        assert!(h.entries.is_empty());
    }

    /// Helper: relative paths (under `root`) of the entries, in the
    /// orchestrator's *splice* order (reverse of the innermost-first walk).
    fn splice_order(h: &Hierarchy, root: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = h
            .entries
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(root)
                    .unwrap_or(&e.path)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        v.reverse();
        v
    }

    #[test]
    fn discovers_dot_claude_lingxi_md() {
        // `.lingxi/LINGXI.md` (Project) must be found right after `LINGXI.md`.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(repo.join(".lingxi")).unwrap();
        touch(&repo, "LINGXI.md");
        touch(&repo.join(".lingxi"), "LINGXI.md");

        let h = walk(&repo, &home, None);
        assert_eq!(
            splice_order(&h, tmp.path()),
            vec!["repo/LINGXI.md", "repo/.lingxi/LINGXI.md"],
            "splice order: LINGXI.md then .lingxi/LINGXI.md"
        );
    }

    #[test]
    fn discovers_dot_claude_rules_including_nested_in_order() {
        // `.lingxi/rules/**/*.md` (Project), recursive, sorted by name.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = tmp.path().join("repo");
        let rules = repo.join(".lingxi").join("rules");
        fs::create_dir_all(rules.join("sub")).unwrap();
        touch(&repo, "LINGXI.md");
        touch(&rules, "b.md");
        touch(&rules, "a.md");
        touch(&rules, "ignored.txt"); // non-.md skipped
        touch(&rules.join("sub"), "z.md");

        let h = walk(&repo, &home, None);
        // Sorted readdir: `a.md`, `b.md`, then the `sub/` directory (recursed).
        assert_eq!(
            splice_order(&h, tmp.path()),
            vec![
                "repo/LINGXI.md",
                "repo/.lingxi/rules/a.md",
                "repo/.lingxi/rules/b.md",
                "repo/.lingxi/rules/sub/z.md",
            ],
            "rules discovered recursively, sorted, .md only"
        );
    }

    #[test]
    fn discovers_user_rules_tier() {
        // `~/.lingxi/rules/**/*.md` (User) come right after `~/.lingxi/LINGXI.md`.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let user = home.join(".lingxi");
        fs::create_dir_all(user.join("rules")).unwrap();
        touch(&user, "LINGXI.md");
        touch(&user.join("rules"), "u1.md");
        touch(&user.join("rules"), "u2.md");
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        touch(&cwd, "LINGXI.md");

        let h = walk(&cwd, &home, None);
        assert_eq!(
            splice_order(&h, tmp.path()),
            vec![
                "home/.lingxi/LINGXI.md",
                "home/.lingxi/rules/u1.md",
                "home/.lingxi/rules/u2.md",
                "repo/LINGXI.md",
            ],
            "User LINGXI.md + rules precede the project tier"
        );
    }

    #[test]
    fn full_tier_order_managed_user_project_local() {
        // GAP 1: the MANAGED tier loads FIRST (lowest priority, spliced first).
        // The managed dir is injected explicitly (hermetic — no real system
        // path, no env-var races). Assert Managed → User → Project → Local with
        // `.lingxi/LINGXI.md`, rules, and the local override, AND that each
        // entry carries the right `LingxiMdTier`.
        use super::super::LingxiMdTier;
        let tmp = TempDir::new().unwrap();

        let managed = tmp.path().join("managed");
        fs::create_dir_all(managed.join(".lingxi").join("rules")).unwrap();
        touch(&managed, "LINGXI.md");
        touch(&managed.join(".lingxi").join("rules"), "mr.md");

        let home = tmp.path().join("home");
        let user = home.join(".lingxi");
        fs::create_dir_all(user.join("rules")).unwrap();
        touch(&user, "LINGXI.md");
        touch(&user.join("rules"), "ur.md");

        let repo = tmp.path().join("repo");
        let pkg = repo.join("pkg");
        fs::create_dir_all(pkg.join(".lingxi").join("rules")).unwrap();
        touch(&repo, "LINGXI.md");
        touch(&pkg, "LINGXI.md");
        touch(&pkg.join(".lingxi"), "LINGXI.md");
        touch(&pkg.join(".lingxi").join("rules"), "pr.md");
        touch(&pkg, "LINGXI.local.md");

        let h = walk(&pkg, &home, Some(&managed));
        assert_eq!(
            splice_order(&h, tmp.path()),
            vec![
                "managed/LINGXI.md",
                "managed/.lingxi/rules/mr.md",
                "home/.lingxi/LINGXI.md",
                "home/.lingxi/rules/ur.md",
                "repo/LINGXI.md",
                "repo/pkg/LINGXI.md",
                "repo/pkg/.lingxi/LINGXI.md",
                "repo/pkg/.lingxi/rules/pr.md",
                "repo/pkg/LINGXI.local.md",
            ],
            "tier order: Managed → User → Project(root→cwd) → Local"
        );

        // Tier tagging: build a path→tier map and spot-check each tier.
        let tier_of = |suffix: &str| -> LingxiMdTier {
            h.entries
                .iter()
                .find(|e| e.path.to_string_lossy().ends_with(suffix))
                .unwrap_or_else(|| panic!("entry ending {suffix} not found"))
                .tier
        };
        assert_eq!(tier_of("managed/LINGXI.md"), LingxiMdTier::Managed);
        assert_eq!(
            tier_of("managed/.lingxi/rules/mr.md"),
            LingxiMdTier::Managed
        );
        assert_eq!(tier_of("home/.lingxi/LINGXI.md"), LingxiMdTier::User);
        assert_eq!(tier_of("home/.lingxi/rules/ur.md"), LingxiMdTier::User);
        assert_eq!(tier_of("repo/LINGXI.md"), LingxiMdTier::Project);
        assert_eq!(tier_of("pkg/.lingxi/rules/pr.md"), LingxiMdTier::Project);
        assert_eq!(tier_of("LINGXI.local.md"), LingxiMdTier::Local);
    }

    #[test]
    fn managed_path_honours_env_override_else_platform_default() {
        // `managed_path()` reads MANAGED_DIR_ENV first; otherwise the OS default.
        // (Single-threaded by default in cargo's test runner; this is the only
        // test touching MANAGED_DIR_ENV.)
        let prev = std::env::var_os(MANAGED_DIR_ENV);
        std::env::set_var(MANAGED_DIR_ENV, "/tmp/lingxi-managed-override");
        assert_eq!(
            managed_path(),
            PathBuf::from("/tmp/lingxi-managed-override"),
            "env override must win"
        );

        std::env::remove_var(MANAGED_DIR_ENV);
        let def = managed_path();
        // The platform default is an absolute path under one of the three
        // documented roots.
        assert!(def.is_absolute());
        if cfg!(target_os = "macos") {
            assert_eq!(def, PathBuf::from("/Library/Application Support/LingXi"));
        } else if cfg!(target_os = "windows") {
            assert_eq!(def, PathBuf::from(r"C:\Program Files\LingXi"));
        } else {
            assert_eq!(def, PathBuf::from("/etc/lingxi"));
        }

        // Restore prior env state for other tests.
        match prev {
            Some(v) => std::env::set_var(MANAGED_DIR_ENV, v),
            None => std::env::remove_var(MANAGED_DIR_ENV),
        }
    }

    #[test]
    fn missing_dot_claude_and_rules_dirs_silently_ignored() {
        // No `.lingxi/` dir at all: walk yields only the plain LINGXI.md, no panic.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        touch(&repo, "LINGXI.md");

        let h = walk(&repo, &home, None);
        assert_eq!(splice_order(&h, tmp.path()), vec!["repo/LINGXI.md"]);
    }

    #[test]
    fn rules_recursion_visited_guard_breaks_symlink_cycle() {
        // A symlink loop inside the rules tree must not recurse forever.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        fs::create_dir_all(&home).unwrap();
        let repo = tmp.path().join("repo");
        let rules = repo.join(".lingxi").join("rules");
        fs::create_dir_all(&rules).unwrap();
        touch(&repo, "LINGXI.md");
        touch(&rules, "r.md");

        // rules/loop -> rules (cycle). Skip the test if symlinks are unsupported.
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&rules, rules.join("loop")).is_ok();
        #[cfg(not(unix))]
        let made = false;

        let h = walk(&repo, &home, None);
        let order = splice_order(&h, tmp.path());
        // Must terminate and still surface r.md (exactly once).
        assert!(order.contains(&"repo/.lingxi/rules/r.md".to_string()));
        assert_eq!(
            order.iter().filter(|p| p.ends_with("r.md")).count(),
            1,
            "r.md emitted exactly once despite the cycle (made_symlink={made})"
        );
    }

    #[test]
    fn case_mismatch_flag_set_on_lowercased_filename() {
        // On a case-sensitive filesystem we simulate a mismatch by writing
        // the file as `lingxi.md` (all-lowercase) and asserting exact_case=false.
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        fs::create_dir_all(&cwd).unwrap();
        touch(&cwd, "lingxi.md");
        let home = tmp.path().join("home");
        fs::create_dir_all(home.join(".lingxi")).unwrap();

        let h = walk(&cwd, &home, None);
        assert_eq!(h.entries.len(), 1, "lowercased file must still be found");
        assert!(!h.entries[0].exact_case, "exact_case must be false");
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use telemetry::{sink::LogEventMetadata, AnalyticsBus, AnalyticsSink, AnalyticsValue};

    struct CapturingSink {
        events: Mutex<Vec<(String, LogEventMetadata)>>,
    }

    #[async_trait::async_trait]
    impl AnalyticsSink for CapturingSink {
        async fn log_event(&self, name: &str, metadata: LogEventMetadata) {
            self.events.lock().unwrap().push((name.into(), metadata));
        }
        async fn log_event_async(&self, name: &str, metadata: LogEventMetadata) {
            self.log_event(name, metadata).await;
        }
        fn name(&self) -> &str {
            "capturing"
        }
    }

    #[tokio::test]
    async fn emit_case_mismatch_writes_event_with_pii_tagged_path() {
        let bus = Arc::new(AnalyticsBus::new());
        let sink = Arc::new(CapturingSink {
            events: Mutex::new(Vec::new()),
        });
        bus.attach_sink(sink.clone()).await;

        emit_case_mismatch(
            Some(&bus),
            std::path::Path::new("/Users/u/proj/claude.md"),
            "claude.md",
        )
        .await;

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let (name, md) = &events[0];
        assert_eq!(name, "tengu_memory_case_mismatch");
        assert!(md.contains_key("_PROTO_path"));
        match md.get("actual") {
            Some(AnalyticsValue::String(s)) => assert_eq!(s, "claude.md"),
            _ => panic!("actual must be a string"),
        }
    }
}

#[cfg(test)]
mod split_ancestors_tests {
    use super::*;

    /// The ordinary case: dirs between cwd and the file, outermost-first, with
    /// cwd itself excluded from `nested` and included in `cwd_level`.
    #[test]
    fn splits_nested_and_cwd_level_at_the_cwd_boundary() {
        let a = split_ancestors(
            Path::new("/w/repo/pkg/api/handler.rs"),
            Path::new("/w/repo"),
        );
        assert_eq!(
            a.nested,
            vec![
                PathBuf::from("/w/repo/pkg"),
                PathBuf::from("/w/repo/pkg/api")
            ],
            "outermost-first, cwd excluded"
        );
        assert_eq!(
            a.cwd_level,
            vec![PathBuf::from("/w"), PathBuf::from("/w/repo")],
            "root-first, cwd included, root itself excluded"
        );
    }

    /// A file directly in cwd has NO nested dirs — the loop stops immediately.
    #[test]
    fn a_file_in_cwd_has_no_nested_dirs() {
        let a = split_ancestors(Path::new("/w/repo/main.rs"), Path::new("/w/repo"));
        assert!(a.nested.is_empty());
        assert_eq!(a.cwd_level.last().unwrap(), &PathBuf::from("/w/repo"));
    }

    /// A file OUTSIDE cwd contributes no nested dirs; `cwd_level` is unaffected
    /// because it is derived from cwd alone.
    #[test]
    fn a_file_outside_cwd_yields_no_nested_dirs() {
        let a = split_ancestors(Path::new("/elsewhere/x/y.rs"), Path::new("/w/repo"));
        assert!(a.nested.is_empty(), "nothing under cwd, got {:?}", a.nested);
        assert!(!a.cwd_level.is_empty());
    }

    /// THE REALPATH BRANCH. A cwd reached through a symlink is not a lexical
    /// prefix of the file's real directory, so without the fallback `nested`
    /// comes back empty. This is the macOS `/var` -> `/private/var` shape that
    /// already shipped a silent bug once in the seeded-dedup port.
    #[test]
    fn a_symlinked_path_is_resolved_before_the_prefix_test() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let real = tmp.path().join("real");
        std::fs::create_dir_all(real.join("pkg")).unwrap();
        let link = tmp.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return;

        // cwd is the RESOLVED root; the file is addressed through the symlink,
        // so `dirname(file)` is not lexically under cwd.
        let cwd = std::fs::canonicalize(&real).unwrap();
        let file = link.join("pkg").join("f.rs");
        let a = split_ancestors(&file, &cwd);
        assert_eq!(
            a.nested,
            vec![cwd.join("pkg")],
            "the symlinked dir must resolve under cwd; got {:?}",
            a.nested
        );
    }
}

#[cfg(test)]
mod nested_probe_tests {
    use super::*;

    fn touch(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// `ffo`'s order: memory file, dot-dir memory file, LOCAL OVERRIDE, then
    /// rules. Note the local override comes BEFORE rules here — the opposite of
    /// `walk`'s per-directory order.
    #[test]
    fn nested_probe_uses_the_ffo_order_not_walks() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        touch(&d.join(FILE_NAME), "a");
        touch(&d.join(DOT_LINGXI).join(FILE_NAME), "b");
        touch(&d.join(LOCAL_OVERRIDE_NAME), "c");
        touch(&d.join(DOT_LINGXI).join(RULES_DIR).join("r.md"), "d");

        let mut out = Vec::new();
        let mut processed = std::collections::HashSet::new();
        probe_dir_nested(d, &mut out, &mut processed);

        let got: Vec<PathBuf> = out.iter().map(|e| e.path.clone()).collect();
        assert_eq!(
            got,
            vec![
                d.join(FILE_NAME),
                d.join(DOT_LINGXI).join(FILE_NAME),
                d.join(LOCAL_OVERRIDE_NAME),
                d.join(DOT_LINGXI).join(RULES_DIR).join("r.md"),
            ],
            "local override must precede rules (ffo), unlike walk"
        );
    }

    /// `FLu` loads rules ONLY — a cwd-level directory contributes no LINGXI.md
    /// and no local override.
    #[test]
    fn cwd_level_probe_loads_rules_only() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        touch(&d.join(FILE_NAME), "a");
        touch(&d.join(LOCAL_OVERRIDE_NAME), "c");
        touch(&d.join(DOT_LINGXI).join(RULES_DIR).join("r.md"), "d");

        let mut out = Vec::new();
        let mut processed = std::collections::HashSet::new();
        probe_dir_cwd_level(d, &mut out, &mut processed);

        let got: Vec<PathBuf> = out.iter().map(|e| e.path.clone()).collect();
        assert_eq!(got, vec![d.join(DOT_LINGXI).join(RULES_DIR).join("r.md")]);
    }

    /// The shared `processed` set is what stops one file being surfaced twice
    /// when two ancestors resolve to the same path.
    #[test]
    fn the_processed_set_suppresses_a_repeat_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        touch(&d.join(FILE_NAME), "a");
        let mut out = Vec::new();
        let mut processed = std::collections::HashSet::new();
        probe_dir_nested(d, &mut out, &mut processed);
        let first = out.len();
        probe_dir_nested(d, &mut out, &mut processed);
        assert_eq!(
            out.len(),
            first,
            "second probe of the same dir adds nothing"
        );
    }
}
