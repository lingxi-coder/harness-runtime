//! Auto-edit path-safety guard — port of claude-code
//! `utils/permissions/filesystem.ts` `checkPathSafetyForAutoEdit`
//! (`:620-665`) and its two helpers `isDangerousFilePathToAutoEdit`
//! (`:435-488`) and `hasSuspiciousWindowsPathPattern` (`:537-602`), plus the
//! `DANGEROUS_FILES` / `DANGEROUS_DIRECTORIES` lists (`:57-79`) verbatim.
//!
//! The guard answers: "even in `acceptEdits` / inside a working directory, is
//! this path too sensitive to silently auto-allow an edit to?" When it returns
//! [`AutoEditSafety::Unsafe`] the caller must fall through to an interactive
//! ask instead of auto-allowing. It protects `.git/`, `.vscode/`, `.idea/`,
//! `.lingxi/` (except the structural `.lingxi/worktrees/`), shell/config
//! dotfiles (`.bashrc`, `.gitconfig`, `.mcp.json`, …), and a battery of
//! suspicious Windows path shapes (NTFS ADS, 8.3 short names, long-path
//! prefixes, trailing dot/space, DOS device names, `...`, UNC).
//!
//! ## Wiring is DEFERRED (PERM.1 prerequisite)
//! claude-code runs these safety asks FIRST (`filesystem.ts:1242`+), ahead of
//! the `mode === 'acceptEdits' && isInWorkingDir` auto-allow branch
//! (`:1360-1375`). That auto-allow branch is PERM Batch 1 and has **not landed
//! yet** — [`crate::policy::PermissionPolicy::authorize`] has no `AcceptEdits`
//! working-dir auto-allow to gate. So this batch lands the pure guard + its
//! unit tests + the `lib.rs` re-export ONLY; it does NOT touch `policy.rs`'s
//! decision flow and is fully behavior-neutral at runtime (exercised only by
//! the tests below). When PERM.1 lands the `AcceptEdits` branch, it should call
//! [`check_path_safety_for_auto_edit`] BEFORE the working-dir allow and, on
//! [`AutoEditSafety::Unsafe`], surface the reason via
//! [`crate::result::PermissionDecisionReason::SafetyCheck`] and fall through to
//! ask (never auto-allow).
//!
//! ## Documented divergences from `filesystem.ts` (forced / bounded, not gaps)
//! - **Lexical, not `realpath`.** claude-code's `checkPathSafetyForAutoEdit`
//!   checks BOTH the original path AND the on-disk symlink-resolved path set
//!   (`getPathsForPermissionCheck`, `realpathSync`) to defeat symlink-escape.
//!   This port is lexical-only (consistent with [`crate::filesystem`]'s
//!   already-accepted "Lexical, not `realpath`" decision): we check only the
//!   one lexically-expanded path. Symlink-escape hardening is a known,
//!   pre-accepted divergence in this crate.
//! - **ADS colon check is `cfg!(windows)`-gated, not WSL-aware.** TS gates the
//!   NTFS-ADS colon scan on `getPlatform() === 'windows' || getPlatform() ===
//!   'wsl'`. The Rust build has no `getPlatform()` / WSL probe, so the colon
//!   check runs under `cfg!(windows)` only — a Linux/macOS host that is really
//!   WSL/DrvFs will NOT get the colon check. Documented WSL gap.
//! - **UNC `containsVulnerableUncPath` is `cfg!(windows)`-gated.** In TS that
//!   helper returns early `false` off-Windows (`getPlatform() !== 'windows'`),
//!   so its use inside `hasSuspiciousWindowsPathPattern` (step 6) is already
//!   Windows-only despite the "all platforms" comment. The Rust port mirrors
//!   that short-circuit with `cfg!(windows)`. (The simpler `\\`/`//` UNC PREFIX
//!   check in [`is_dangerous_file_path_to_auto_edit`] is platform-INDEPENDENT,
//!   matching TS `:442`.)
//!
//! Everything else is byte-faithful: the same `DANGEROUS_FILES` /
//! `DANGEROUS_DIRECTORIES` lists, the same `.claude`→`worktrees` skip, the same
//! regexes translated to the `regex` crate, the same lowercase case-fold, and
//! the same branch order (suspicious-windows → claude-config → dangerous-file).

use crate::filesystem::FsRoots;
use std::path::Path;
use std::sync::OnceLock;

/// Dangerous files that should be protected from auto-editing. These files can
/// be used for code execution or data exfiltration. 1:1 with the 2.1.195 binary
/// `Sjo` (the leaked `filesystem.ts:57-68` list was stale and much shorter).
/// The binary's `.claude.json` maps to this build's [`branding::GLOBAL_CONFIG_FILE`].
pub const DANGEROUS_FILES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".bash_login",
    ".bash_aliases",
    ".bash_logout",
    ".envrc",
    ".ripgreprc",
    ".mcp.json",
    branding::GLOBAL_CONFIG_FILE,
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnp.cjs",
    ".pnp.loader.mjs",
    ".pnpmfile.cjs",
    "bunfig.toml",
    ".bunfig.toml",
    ".bazelrc",
    ".bazelversion",
    ".bazeliskrc",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    ".lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yaml",
    "gradle-wrapper.properties",
    "maven-wrapper.properties",
    ".devcontainer.json",
    "pyrightconfig.json",
];

/// Dangerous directories that should be protected from auto-editing. These
/// directories contain sensitive configuration or executable files. 1:1 with the
/// 2.1.195 binary `Xsc`. The binary's `.claude` maps to [`branding::DOT_DIR`].
pub const DANGEROUS_DIRECTORIES: &[&str] = &[
    ".git",
    ".vscode",
    ".idea",
    branding::DOT_DIR,
    ".husky",
    ".cargo",
    ".devcontainer",
    ".yarn",
    ".mvn",
];

/// Dangerous `/`-joined directory PATHS — matched as a run of consecutive path
/// segments anywhere in the path. 1:1 with the 2.1.195 binary `Qsc`
/// (`DANGEROUS_DIRECTORY_PATHS`).
pub const DANGEROUS_DIRECTORY_PATHS: &[&str] = &[".config/git"];

/// Result of [`check_path_safety_for_auto_edit`]. `Safe` ⇒ the path may be
/// auto-allowed by the (future) `acceptEdits` working-dir branch; `Unsafe` ⇒
/// the caller MUST fall through to an interactive ask.
///
/// Mirrors the TS union `{ safe: true } | { safe: false; message;
/// classifierApprovable }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoEditSafety {
    /// All safety checks passed.
    Safe,
    /// A safety check failed; the call must be asked, not auto-allowed.
    Unsafe {
        /// Human-readable explanation (byte-faithful to the TS messages).
        message: String,
        /// Whether the LLM auto-mode classifier could still approve this call.
        /// `false` for suspicious-Windows paths (hard block); `true` for the
        /// claude-config and dangerous-file branches.
        classifier_approvable: bool,
    },
}

/// Normalize a path for case-insensitive comparison — port of
/// `filesystem.ts:90-92` (`normalizeCaseForComparison`). Always lowercases
/// regardless of platform, so a mixed-case `.cLauDe/Settings.locaL.json`
/// cannot bypass the security checks on a case-insensitive filesystem.
#[must_use]
pub fn normalize_case_for_comparison(s: &str) -> String {
    s.to_lowercase()
}

/// Whether editing `path` (already lexically expanded to an absolute path)
/// should be blocked from silent auto-editing — port of
/// `isDangerousFilePathToAutoEdit` (`filesystem.ts:435-488`).
///
/// `raw` is the ORIGINAL (pre-expansion) path string; TS's UNC `\\`/`//`
/// prefix check (`:442`) runs against it, while the segment scan and the
/// basename check run against the expanded `path`.
///
/// Checks, in TS order:
/// 1. UNC prefix `\\` or `//` on the raw path → dangerous (platform-independent).
/// 2. Any path segment (case-folded) matching a [`DANGEROUS_DIRECTORIES`]
///    entry → dangerous, EXCEPT a `.claude` segment immediately followed by a
///    `worktrees` segment (structural worktree path; `break` and keep scanning
///    later segments).
/// 3. The basename (case-folded) matching a [`DANGEROUS_FILES`] entry →
///    dangerous.
#[must_use]
pub fn is_dangerous_file_path_to_auto_edit(path: &Path, raw: &str) -> bool {
    // 1. UNC prefix on the RAW path (defense-in-depth, platform-independent —
    //    TS `:442`). Block anything starting with `\\` or `//`.
    if raw.starts_with("\\\\") || raw.starts_with("//") {
        return true;
    }

    // Split into segments the way TS `absolutePath.split(sep)` does. We operate
    // on the lossy string form so the case-fold matches TS exactly; using `/`
    // as the separator is correct for the POSIX target (and Windows `\` paths
    // would already have been UNC-prefix-blocked / are out of the lexical port).
    let abs = path.to_string_lossy();
    let segments: Vec<&str> = abs.split('/').collect();
    let file_name = segments.last().copied();

    // 2. Dangerous directory segments (case-insensitive), with the
    //    `.claude`→`worktrees` structural skip (`:447-472`).
    for (i, segment) in segments.iter().enumerate() {
        let normalized_segment = normalize_case_for_comparison(segment);
        for dir in DANGEROUS_DIRECTORIES {
            if normalized_segment != normalize_case_for_comparison(dir) {
                continue;
            }

            // Special case: `.lingxi/worktrees/` is a structural path (where
            // Claude stores git worktrees), not a user-created dangerous
            // directory. Skip THIS `.claude` segment when it is immediately
            // followed by `worktrees`; keep scanning later segments so a
            // nested `.claude` inside the worktree is still blocked.
            if *dir == branding::DOT_DIR {
                if let Some(next) = segments.get(i + 1) {
                    if normalize_case_for_comparison(next) == "worktrees" {
                        break; // skip this `.claude`, continue outer scan
                    }
                }
            }

            return true;
        }
    }

    // 2b. Dangerous directory PATHS (`Qsc` / `DANGEROUS_DIRECTORY_PATHS`): a run
    //     of consecutive segments (case-folded) matching a `/`-joined entry, e.g.
    //     `.config/git`. Binary `Hef`:
    //       for(l of Qsc){c=l.split("/"); for(u…) if(c.every((d,p)=>fold(o[u+p])===fold(d)))return!0}
    for path_entry in DANGEROUS_DIRECTORY_PATHS {
        let parts: Vec<&str> = path_entry.split('/').collect();
        for window in segments.windows(parts.len()) {
            if window.iter().zip(parts.iter()).all(|(seg, part)| {
                normalize_case_for_comparison(seg) == normalize_case_for_comparison(part)
            }) {
                return true;
            }
        }
    }

    // 3. Dangerous configuration files by basename (case-insensitive) (`:474-485`).
    if let Some(name) = file_name {
        let normalized_file_name = normalize_case_for_comparison(name);
        if DANGEROUS_FILES
            .iter()
            .any(|d| normalize_case_for_comparison(d) == normalized_file_name)
        {
            return true;
        }
    }

    false
}

/// Detects suspicious Windows path patterns that could bypass security checks
/// through path-canonicalization vulnerabilities — port of
/// `hasSuspiciousWindowsPathPattern` (`filesystem.ts:537-602`). When any of the
/// six checks fires the path must always require manual approval.
///
/// Runs on `raw` (the original, un-expanded path string) exactly as TS does.
///
/// Checks, in TS order:
/// 1. NTFS Alternate Data Streams — a `:` after position 2 (skips the `C:\`
///    drive letter). `cfg!(windows)`-gated (TS: `windows`/`wsl`-gated); see the
///    module-header WSL divergence.
/// 2. 8.3 short names — `~` followed by a digit (`/~\d/`). Platform-independent.
/// 3. Long-path prefixes — `\\?\`, `\\.\`, `//?/`, `//./`. Platform-independent.
/// 4. Trailing dots/spaces Windows strips during resolution (`/[.\s]+$/`),
///    tested PER PATH SEGMENT (2.1.211 `eat` splits on `/[/\\]/`, skips
///    ""/"."/".." segments, tests each). Platform-independent.
/// 5. DOS device names — `.(CON|PRN|AUX|NUL|COM1-9|LPT1-9)` at end,
///    case-insensitive. Platform-independent.
/// 6. Three-or-more consecutive dots used as a path component
///    (`/(^|\/|\\)\.{3,}(\/|\\|$)/`). Platform-independent.
/// 7. Vulnerable UNC paths (`containsVulnerableUncPath`). `cfg!(windows)`-gated
///    (matches the TS helper's `getPlatform() !== 'windows'` early return).
#[must_use]
pub fn has_suspicious_windows_path_pattern(raw: &str) -> bool {
    // 1. NTFS Alternate Data Streams — `:` after position 2 (skip `C:\`).
    //    Examples: `file.txt::$DATA`, `.bashrc:hidden`, `settings.json:stream`.
    //    `cfg!(windows)`-gated; TS additionally gates on `wsl` (see module doc).
    if cfg!(windows) {
        // `path.indexOf(':', 2) !== -1` — a colon at byte index >= 2.
        if raw.char_indices().any(|(idx, c)| idx >= 2 && c == ':') {
            return true;
        }
    }

    // 2. 8.3 short names — `~` followed by a digit. Examples: `GIT~1`,
    //    `CLAUDE~1`, `SETTIN~1.JSON`.
    if eight_dot_three_re().is_match(raw) {
        return true;
    }

    // 3. Long-path prefixes (backslash and forward-slash variants). Examples:
    //    `\\?\C:\Users\...`, `\\.\C:\...`, `//?/C:/...`, `//./C:/...`.
    if raw.starts_with("\\\\?\\")
        || raw.starts_with("\\\\.\\")
        || raw.starts_with("//?/")
        || raw.starts_with("//./")
    {
        return true;
    }

    // 4. Trailing dots/spaces Windows strips during path resolution, tested PER
    //    PATH SEGMENT. 2.1.211 `eat` splits the path on /[/\\]/, skips the
    //    ""/"."/".." segments, and tests aMi=/[.\s]+$/ against EACH remaining
    //    segment — so a mid-path trailing-dot/space segment (`/proj/dir./file`,
    //    `.claude /x`) is suspicious, not only a trailing dot/space at the very
    //    end of the whole path. Examples: `.git.`, `.claude /x`, `dir./file`,
    //    `.bashrc...`, `settings.json.`.
    for segment in raw.split(['/', '\\']) {
        if segment.is_empty() || segment == "." || segment == ".." {
            continue;
        }
        if trailing_dot_space_re().is_match(segment) {
            return true;
        }
    }

    // 5. DOS device names Windows treats as special devices. Examples:
    //    `.git.CON`, `settings.json.PRN`, `.bashrc.AUX`.
    if dos_device_re().is_match(raw) {
        return true;
    }

    // 6. Three-or-more consecutive dots used as a path component. Only blocks
    //    when the dots are bounded by path separators (or string ends), which
    //    permits legitimate uses like Next.js catch-all routes `[...]name]`.
    if triple_dot_re().is_match(raw) {
        return true;
    }

    // 7. Vulnerable UNC paths (`cfg!(windows)`-gated; TS helper returns false
    //    off-Windows). Examples: `\\server\share`, `//192.168.1.1/share`.
    if contains_vulnerable_unc_path(raw) {
        return true;
    }

    false
}

/// Check if a path is safe for silent auto-editing (`acceptEdits` mode) — port
/// of `checkPathSafetyForAutoEdit` (`filesystem.ts:620-665`).
///
/// Branch order (byte-faithful):
/// 1. Suspicious Windows path pattern → `Unsafe { classifier_approvable: false }`.
/// 2. Claude config file (`.lingxi/settings.json`, `.lingxi/commands|agents|
///    skills/…`) → `Unsafe { classifier_approvable: true }`.
/// 3. Dangerous file/directory ([`is_dangerous_file_path_to_auto_edit`]) →
///    `Unsafe { classifier_approvable: true }`.
/// 4. Otherwise → [`AutoEditSafety::Safe`].
///
/// Lexical-only path set (one expanded path, no symlink resolution) — see the
/// module-header divergence note.
#[must_use]
pub fn check_path_safety_for_auto_edit(raw: &str, roots: &FsRoots) -> AutoEditSafety {
    let expanded = crate::filesystem::expand_path(raw, roots);

    // 1. Suspicious Windows path patterns (runs on the original path string).
    if has_suspicious_windows_path_pattern(raw) {
        return AutoEditSafety::Unsafe {
            message: format!(
                "Claude requested permissions to write to {raw}, which contains a suspicious Windows path pattern that requires manual approval."
            ),
            classifier_approvable: false,
        };
    }

    // 2. Claude config files.
    if is_lingxi_config_file_path(&expanded, roots) {
        return AutoEditSafety::Unsafe {
            message: format!(
                "Claude requested permissions to write to {raw}, but you haven't granted it yet."
            ),
            classifier_approvable: true,
        };
    }

    // 3. Dangerous files / directories.
    if is_dangerous_file_path_to_auto_edit(&expanded, raw) {
        return AutoEditSafety::Unsafe {
            message: format!(
                "Claude requested permissions to edit {raw} which is a sensitive file."
            ),
            classifier_approvable: true,
        };
    }

    // 4. All safety checks passed.
    AutoEditSafety::Safe
}

/// Whether `expanded` (an absolute, lexically-expanded path) is one of Claude
/// Code's own config files that must always be asked before editing — port of
/// `isClaudeConfigFilePath` (`filesystem.ts:225-242`) composed with the
/// structural half of `isClaudeSettingsPath` (`:200-222`).
///
/// We port the universal, structural checks that need no process-global state:
/// - ends with `/.lingxi/settings.json` or `/.lingxi/settings.local.json`
///   (case-folded), matching the "include even for other projects" arm; and
/// - lives inside `<cwd>/.lingxi/commands`, `<cwd>/.lingxi/agents`, or
///   `<cwd>/.lingxi/skills`.
///
/// The TS additionally compares against every resolved settings-file path
/// returned by `getSettingsPaths()` (managed/CLI-arg settings). Those depend on
/// `SETTING_SOURCES` process state not plumbed into this crate, so they are
/// NOT ported here. This is a safe under-approximation: anything under
/// `.lingxi/` is already caught by [`is_dangerous_file_path_to_auto_edit`]
/// (`.claude` ∈ [`DANGEROUS_DIRECTORIES`]) with the SAME
/// `classifier_approvable: true` outcome — so the only observable effect of
/// this branch is preferring the "haven't granted it yet" message over the
/// "sensitive file" message for the settings files it does match.
fn is_lingxi_config_file_path(expanded: &Path, roots: &FsRoots) -> bool {
    let normalized = normalize_case_for_comparison(&expanded.to_string_lossy());

    // `isClaudeSettingsPath` structural arm — POSIX separator (`/`).
    if normalized.ends_with("/.lingxi/settings.json")
        || normalized.ends_with("/.lingxi/settings.local.json")
    {
        return true;
    }

    // Inside `<cwd>/.lingxi/{commands,agents,skills}` — `pathInWorkingPath`
    // (case-insensitive containment). We reuse the lexical containment shape:
    // the expanded path must be at-or-under the directory.
    let cwd = &roots.cwd;
    for sub in ["commands", "agents", "skills"] {
        let dir = cwd.join(branding::DOT_DIR).join(sub);
        if path_at_or_under(
            &normalized,
            &normalize_case_for_comparison(&dir.to_string_lossy()),
        ) {
            return true;
        }
    }

    false
}

/// Lexical, case-folded containment: is `path` equal to `base` or under it
/// (`base` followed by a `/`)? Both args are already lowercased.
fn path_at_or_under(path: &str, base: &str) -> bool {
    if path == base {
        return true;
    }
    let with_sep = format!("{base}/");
    path.starts_with(&with_sep)
}

/// Port of `containsVulnerableUncPath` (`readOnlyCommandValidation.ts:1562-1638`).
/// `cfg!(windows)`-gated to mirror the TS `getPlatform() !== 'windows'` early
/// return — off-Windows it is always `false`, so the regexes below never run at
/// runtime. They are NOT dead code: `cfg!(windows)` is a runtime `bool` const
/// (not a `#[cfg]` attribute), so the call sites remain compiled and reachable
/// on every target, keeping the helpers exercisable.
fn contains_vulnerable_unc_path(raw: &str) -> bool {
    if !cfg!(windows) {
        return false;
    }

    // 1. Backslash UNC: `\\server`, `\\server\share`, `\\server@port\share`.
    if unc_backslash_re().is_match(raw) {
        return true;
    }
    // 2. Forward-slash UNC, excluding URL schemes. The TS uses a negative
    //    lookbehind `(?<!:)` which the `regex` crate does not support; we
    //    replicate it with an explicit predicate (see [`forward_slash_unc`]).
    if forward_slash_unc(raw) {
        return true;
    }
    // 3. Mixed-separator UNC `/\\…`.
    if unc_mixed_slash_re().is_match(raw) {
        return true;
    }
    // 4. Reverse mixed-separator UNC `\\…/…`.
    if unc_reverse_mixed_slash_re().is_match(raw) {
        return true;
    }
    // 5. WebDAV SSL/port markers.
    if unc_webdav_ssl_re().is_match(raw) {
        return true;
    }
    // 6. DavWWWRoot WebDAV redirector marker.
    if unc_davwwwroot_re().is_match(raw) {
        return true;
    }
    // 7. UNC with IPv4 address.
    if unc_ipv4_re().is_match(raw) {
        return true;
    }
    // 8. UNC with bracketed IPv6 address.
    if unc_ipv6_re().is_match(raw) {
        return true;
    }

    false
}

/// Replicates the TS forward-slash UNC regex `/(?<!:)\/\/[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)/i`.
/// The `regex` crate lacks look-behind, so we scan each `//` occurrence and
/// reject only the ones immediately preceded by `:` (URL schemes like
/// `https://`), then test the look-behind-free remainder of the pattern from
/// that position.
fn forward_slash_unc(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = raw[search_from..].find("//") {
        let pos = search_from + rel;
        // Negative look-behind `(?<!:)`: reject `//` preceded by ':'.
        let preceded_by_colon = pos > 0 && bytes[pos - 1] == b':';
        if !preceded_by_colon && forward_slash_unc_tail_re().is_match(&raw[pos..]) {
            return true;
        }
        search_from = pos + 1; // advance past this `/` so overlapping `//` are seen
    }
    false
}

// --- Compiled regexes (lazy, leak-free `OnceLock`). Each translated directly
// from the corresponding JS literal; flags noted inline. ---

fn eight_dot_three_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /~\d/
    RE.get_or_init(|| regex::Regex::new(r"~\d").unwrap())
}

fn trailing_dot_space_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /[.\s]+$/
    RE.get_or_init(|| regex::Regex::new(r"[.\s]+$").unwrap())
}

fn dos_device_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /\.(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$/i
    RE.get_or_init(|| regex::Regex::new(r"(?i)\.(CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$").unwrap())
}

fn triple_dot_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /(^|\/|\\)\.{3,}(\/|\\|$)/
    RE.get_or_init(|| regex::Regex::new(r"(^|/|\\)\.{3,}(/|\\|$)").unwrap())
}

fn unc_backslash_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /\\\\[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)/i
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)\\\\[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)").unwrap()
    })
}

fn forward_slash_unc_tail_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS tail (look-behind stripped, anchored at the `//`):
    //   /^\/\/[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)/i
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^//[^\s\\/]+(?:@(?:\d+|ssl))?(?:[\\/]|$|\s)").unwrap()
    })
}

fn unc_mixed_slash_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /\/\\{2,}[^\s\\/]/
    RE.get_or_init(|| regex::Regex::new(r"/\\{2,}[^\s\\/]").unwrap())
}

fn unc_reverse_mixed_slash_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /\\{2,}\/[^\s\\/]/
    RE.get_or_init(|| regex::Regex::new(r"\\{2,}/[^\s\\/]").unwrap())
}

fn unc_webdav_ssl_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /@SSL@\d+/i OR /@\d+@SSL/i — combined with alternation.
    RE.get_or_init(|| regex::Regex::new(r"(?i)(@SSL@\d+|@\d+@SSL)").unwrap())
}

fn unc_davwwwroot_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /DavWWWRoot/i
    RE.get_or_init(|| regex::Regex::new(r"(?i)DavWWWRoot").unwrap())
}

fn unc_ipv4_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /^\\\\(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})[\\/]/ OR the // variant.
    RE.get_or_init(|| {
        regex::Regex::new(r"^(\\\\|//)(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})[\\/]").unwrap()
    })
}

fn unc_ipv6_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // JS: /^\\\\(\[[\da-fA-F:]+\])[\\/]/ OR the // variant.
    RE.get_or_init(|| regex::Regex::new(r"^(\\\\|//)(\[[\da-fA-F:]+\])[\\/]").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn roots() -> FsRoots {
        FsRoots {
            cwd: PathBuf::from("/proj"),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn expand(raw: &str) -> PathBuf {
        crate::filesystem::expand_path(raw, &roots())
    }

    fn is_dangerous(raw: &str) -> bool {
        is_dangerous_file_path_to_auto_edit(&expand(raw), raw)
    }

    // --- is_dangerous_file_path_to_auto_edit (spec table) ---

    #[test]
    fn git_config_is_dangerous() {
        // `.git/` directory segment → dangerous.
        assert!(is_dangerous("/proj/.git/config"));
        // `.gitconfig` basename → dangerous file.
        assert!(is_dangerous("/proj/.gitconfig"));
    }

    #[test]
    fn claude_settings_is_dangerous_directory() {
        // `.lingxi/` directory segment → dangerous.
        assert!(is_dangerous("/proj/.lingxi/settings.json"));
    }

    #[test]
    fn claude_worktrees_is_not_dangerous() {
        // `.lingxi/worktrees/...` is structural — NOT dangerous.
        assert!(!is_dangerous("/proj/.lingxi/worktrees/x/file.rs"));
    }

    #[test]
    fn nested_claude_inside_worktree_is_dangerous() {
        // A nested `.claude` NOT followed by `worktrees` is still blocked.
        assert!(is_dangerous(
            "/proj/.lingxi/worktrees/x/.lingxi/settings.json"
        ));
    }

    #[test]
    fn vscode_dir_is_dangerous() {
        assert!(is_dangerous("/proj/.vscode/launch.json"));
    }

    #[test]
    fn idea_dir_is_dangerous() {
        assert!(is_dangerous("/proj/.idea/workspace.xml"));
    }

    #[test]
    fn bashrc_is_dangerous() {
        // `~/.bashrc` → expands under home, basename `.bashrc` ∈ DANGEROUS_FILES.
        assert!(is_dangerous("~/.bashrc"));
    }

    #[test]
    fn mixed_case_git_dir_is_dangerous() {
        // Case-insensitive: `.GiT/` still matches `.git`.
        assert!(is_dangerous("/proj/.GiT/config"));
        // Mixed-case `.lInGxI` still matches `.lingxi`.
        assert!(is_dangerous("/proj/.lInGxI/settings.json"));
    }

    #[test]
    fn unc_prefix_is_dangerous() {
        // Raw `//` and `\\` prefixes are blocked regardless of expansion.
        assert!(is_dangerous("//server/share/file"));
        assert!(is_dangerous("\\\\server\\share\\file"));
    }

    #[test]
    fn ordinary_source_file_is_not_dangerous() {
        assert!(!is_dangerous("/proj/src/main.rs"));
        assert!(!is_dangerous("/proj/README.md"));
    }

    #[test]
    fn dangerous_files_list_is_verbatim() {
        // Guard against accidental edits — 1:1 with the 2.1.195 binary `Sjo`
        // (`.claude.json`→`.lingxi.json`), `Xsc` (`.claude`→`.lingxi`), `Qsc`.
        assert_eq!(
            DANGEROUS_FILES,
            &[
                ".gitconfig",
                ".gitmodules",
                ".bashrc",
                ".bash_profile",
                ".zshrc",
                ".zprofile",
                ".profile",
                ".zshenv",
                ".zlogin",
                ".zlogout",
                ".bash_login",
                ".bash_aliases",
                ".bash_logout",
                ".envrc",
                ".ripgreprc",
                ".mcp.json",
                ".lingxi.json",
                ".npmrc",
                ".yarnrc",
                ".yarnrc.yml",
                ".pnp.cjs",
                ".pnp.loader.mjs",
                ".pnpmfile.cjs",
                "bunfig.toml",
                ".bunfig.toml",
                ".bazelrc",
                ".bazelversion",
                ".bazeliskrc",
                ".pre-commit-config.yaml",
                "lefthook.yml",
                ".lefthook.yml",
                "lefthook.yaml",
                ".lefthook.yaml",
                "gradle-wrapper.properties",
                "maven-wrapper.properties",
                ".devcontainer.json",
                "pyrightconfig.json",
            ]
        );
        assert_eq!(
            DANGEROUS_DIRECTORIES,
            &[
                ".git",
                ".vscode",
                ".idea",
                ".lingxi",
                ".husky",
                ".cargo",
                ".devcontainer",
                ".yarn",
                ".mvn",
            ]
        );
        assert_eq!(DANGEROUS_DIRECTORY_PATHS, &[".config/git"]);
    }

    #[test]
    fn newly_added_dangerous_paths_are_blocked() {
        // Sjo additions (basename) + Xsc additions (segment) + Qsc (path run).
        assert!(is_dangerous("/proj/.npmrc"));
        assert!(is_dangerous("/proj/sub/pyrightconfig.json"));
        assert!(is_dangerous("/home/u/.cargo/config.toml"));
        assert!(is_dangerous("/proj/.yarn/releases/x.cjs"));
        // Qsc: `.config/git` as consecutive segments anywhere in the path.
        assert!(is_dangerous("/home/u/.config/git/config"));
        assert!(is_dangerous("/home/u/.config/git/ignore"));
        // `.config` alone (without a following `git`) is NOT a Qsc match.
        assert!(!is_dangerous("/home/u/.config/other/file"));
    }

    // --- has_suspicious_windows_path_pattern (spec table) ---

    #[test]
    fn short_name_8_3_is_suspicious() {
        // `~` + digit anywhere → 8.3 short name. Platform-independent.
        assert!(has_suspicious_windows_path_pattern("foo~1\\bar"));
        assert!(has_suspicious_windows_path_pattern("GIT~1"));
        assert!(has_suspicious_windows_path_pattern("SETTIN~1.JSON"));
    }

    #[test]
    fn long_path_prefix_is_suspicious() {
        assert!(has_suspicious_windows_path_pattern("//?/C:/x"));
        assert!(has_suspicious_windows_path_pattern("//./C:/x"));
        assert!(has_suspicious_windows_path_pattern("\\\\?\\C:\\x"));
        assert!(has_suspicious_windows_path_pattern("\\\\.\\C:\\x"));
    }

    #[test]
    fn trailing_dot_or_space_is_suspicious() {
        // `.git.` → trailing dot.
        assert!(has_suspicious_windows_path_pattern(".git."));
        // trailing space.
        assert!(has_suspicious_windows_path_pattern(".claude "));
        // multiple trailing dots.
        assert!(has_suspicious_windows_path_pattern(".bashrc..."));
    }

    #[test]
    fn trailing_dot_or_space_is_checked_per_segment() {
        // PERM-AUTO-05: 2.1.211 `eat` tests aMi=/[.\s]+$/ PER path segment, not
        // just the whole string. A mid-path segment with a trailing dot/space
        // (which the whole-string check missed) must be flagged suspicious.
        assert!(has_suspicious_windows_path_pattern("/proj/dir./file"));
        assert!(has_suspicious_windows_path_pattern("/proj/.claude /x"));
        assert!(has_suspicious_windows_path_pattern("dir. /file.txt"));
        // Backslash separators split the same way.
        assert!(has_suspicious_windows_path_pattern("proj\\dir.\\file"));
        // `""`, `.`, `..` segments are skipped, so a plain relative path with a
        // leading `./` or a `..` component is not falsely flagged by this check.
        assert!(!has_suspicious_windows_path_pattern("./proj/src/main.rs"));
        assert!(!has_suspicious_windows_path_pattern("a/../b/main.rs"));
    }

    #[test]
    fn dos_device_name_is_suspicious() {
        // `.bashrc.CON` → DOS device suffix (case-insensitive).
        assert!(has_suspicious_windows_path_pattern(".bashrc.CON"));
        assert!(has_suspicious_windows_path_pattern("settings.json.PRN"));
        assert!(has_suspicious_windows_path_pattern("x.com1"));
        // COM0 / LPT0 are NOT device names.
        assert!(!has_suspicious_windows_path_pattern("x.COM0"));
    }

    #[test]
    fn triple_dot_component_is_suspicious() {
        // `a/.../b` → `...` bounded by separators.
        assert!(has_suspicious_windows_path_pattern("a/.../b"));
        // leading `.../file`.
        assert!(has_suspicious_windows_path_pattern(".../file.txt"));
        // backslash-bounded.
        assert!(has_suspicious_windows_path_pattern("path\\...\\file"));
    }

    #[test]
    fn legitimate_paths_are_not_suspicious() {
        assert!(!has_suspicious_windows_path_pattern("/proj/src/main.rs"));
        // A single `..` (not 3+) bounded by separators is fine.
        assert!(!has_suspicious_windows_path_pattern("a/../b"));
        // A `..` between filename parts (e.g. `v2..beta`) is fine — not
        // separator-bounded, only 2 dots.
        assert!(!has_suspicious_windows_path_pattern("/proj/v2..beta/x"));
        // A normal file with dots in the name.
        assert!(!has_suspicious_windows_path_pattern("/proj/a.test.ts"));
    }

    #[cfg(windows)]
    #[test]
    fn unc_server_share_is_suspicious_on_windows() {
        // `\\server\share` → UNC (Windows-gated).
        assert!(has_suspicious_windows_path_pattern("\\\\server\\share"));
    }

    #[cfg(windows)]
    #[test]
    fn ads_colon_is_suspicious_on_windows() {
        // `file.txt::$DATA` — colon after position 2 (Windows-gated).
        assert!(has_suspicious_windows_path_pattern("file.txt::$DATA"));
    }

    #[cfg(not(windows))]
    #[test]
    fn unc_and_ads_are_not_gated_off_windows() {
        // Off Windows, the colon-ADS and UNC `containsVulnerableUncPath`
        // checks do not fire (documented WSL/platform divergence). The simpler
        // UNC PREFIX check lives in is_dangerous_file_path_to_auto_edit, not
        // here.
        assert!(!has_suspicious_windows_path_pattern("file.txt::$DATA"));
        assert!(!has_suspicious_windows_path_pattern("\\\\server\\share"));
    }

    // --- check_path_safety_for_auto_edit (branch + classifier_approvable) ---

    fn safety(raw: &str) -> AutoEditSafety {
        check_path_safety_for_auto_edit(raw, &roots())
    }

    #[test]
    fn safe_path_passes() {
        assert_eq!(safety("/proj/src/main.rs"), AutoEditSafety::Safe);
    }

    #[test]
    fn suspicious_windows_is_unsafe_not_classifier_approvable() {
        match safety("/proj/GIT~1/x") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                message,
            } => {
                assert!(!classifier_approvable);
                assert!(message.contains("suspicious Windows path pattern"));
            }
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn claude_settings_is_unsafe_classifier_approvable() {
        // `.lingxi/settings.json` → claude-config branch ("haven't granted").
        match safety("/proj/.lingxi/settings.json") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                message,
            } => {
                assert!(classifier_approvable);
                assert!(message.contains("haven't granted it yet"));
            }
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn claude_commands_dir_is_unsafe_classifier_approvable() {
        // `<cwd>/.lingxi/commands/x.md` → claude-config branch.
        match safety("/proj/.lingxi/commands/x.md") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                message,
            } => {
                assert!(classifier_approvable);
                assert!(message.contains("haven't granted it yet"));
            }
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn dangerous_file_is_unsafe_classifier_approvable() {
        // `~/.bashrc` is NOT under `.claude`, so it takes the dangerous-file
        // branch ("sensitive file"), not the claude-config branch.
        match safety("~/.bashrc") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                message,
            } => {
                assert!(classifier_approvable);
                assert!(message.contains("sensitive file"));
            }
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn dangerous_git_dir_is_unsafe_classifier_approvable() {
        // `.git/config` → dangerous-directory branch.
        match safety("/proj/.git/config") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                message,
            } => {
                assert!(classifier_approvable);
                assert!(message.contains("sensitive file"));
            }
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn worktrees_path_is_safe() {
        // `.lingxi/worktrees/x/file.rs` passes all branches → Safe.
        assert_eq!(
            safety("/proj/.lingxi/worktrees/x/file.rs"),
            AutoEditSafety::Safe
        );
    }

    #[test]
    fn windows_branch_wins_over_dangerous_file() {
        // A path that is BOTH suspicious-windows AND a dangerous file must
        // report the windows branch first (classifier_approvable=false).
        match safety("/proj/.bashrc.CON") {
            AutoEditSafety::Unsafe {
                classifier_approvable,
                ..
            } => assert!(!classifier_approvable),
            AutoEditSafety::Safe => panic!("expected Unsafe"),
        }
    }

    #[test]
    fn normalize_case_lowercases() {
        assert_eq!(normalize_case_for_comparison(".LINGXI"), ".lingxi");
        assert_eq!(normalize_case_for_comparison("Foo/Bar"), "foo/bar");
    }
}
