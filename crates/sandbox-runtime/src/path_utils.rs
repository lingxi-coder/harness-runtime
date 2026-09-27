//! Pure path/glob utilities ported 1:1 from the upstream
//! `@anthropic-ai/sandbox-runtime@0.0.54` package.
//!
//! Reference of truth (source-line cites throughout):
//! `docs/superpowers/references/sandbox-runtime-0.0.54/dist/sandbox/sandbox-utils.js`.
//!
//! These feed the sandbox filesystem allowlist, so the symlink-boundary check
//! ([`is_symlink_outside_boundary`]) and the path normalizer
//! ([`normalize_path_for_sandbox`]) are security-relevant: they must preserve
//! the exact upstream semantics.
//!
//! The pure logic (dangerous sets, case fold, glob detection, glob→regex,
//! normalization, boundary check) takes the home directory and current working
//! directory as parameters so it is deterministic and unit-testable; thin
//! wrappers ([`normalize_path_for_sandbox`], [`get_default_write_paths`],
//! [`expand_glob_pattern`]) read the real `dirs::home_dir()` /
//! `std::env::current_dir()`.

use std::path::Path;

use regex::Regex;

/// Dangerous files that should be protected from writes.
///
/// These files can be used for code execution or data exfiltration.
///
/// Ported from `sandbox-utils.js:10-20` (`DANGEROUS_FILES`).
pub const DANGEROUS_FILES: [&str; 9] = [
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".ripgreprc",
    ".mcp.json",
];

/// Dangerous directories that should be protected from writes.
///
/// These directories contain sensitive configuration or executable files.
///
/// Ported from `sandbox-utils.js:25` (`DANGEROUS_DIRECTORIES`).
pub const DANGEROUS_DIRECTORIES: [&str; 3] = [".git", ".vscode", ".idea"];

/// Get the list of dangerous directories to deny writes to.
///
/// Excludes `.git` since we need it writable for git operations — instead we
/// block specific paths within `.git` (hooks and config).
///
/// Ported from `sandbox-utils.js:31-37` (`getDangerousDirectories`).
#[must_use]
pub fn get_dangerous_directories() -> Vec<String> {
    let mut out: Vec<String> = DANGEROUS_DIRECTORIES
        .iter()
        .filter(|d| **d != ".git")
        .map(|d| (*d).to_string())
        .collect();
    out.push(format!("{}/commands", branding::DOT_DIR));
    out.push(format!("{}/agents", branding::DOT_DIR));
    out
}

/// Normalizes a path for case-insensitive comparison.
///
/// This prevents bypassing security checks using mixed-case paths on
/// case-insensitive filesystems (macOS/Windows) like
/// `.cLauDe/Settings.locaL.json`.
///
/// We always normalize to lowercase regardless of platform for consistent
/// security.
///
/// Ported from `sandbox-utils.js:47-49` (`normalizeCaseForComparison`).
#[must_use]
pub fn normalize_case_for_comparison(path_str: &str) -> String {
    path_str.to_lowercase()
}

/// Check if a path pattern contains glob characters.
///
/// Ported from `sandbox-utils.js:53-58` (`containsGlobChars`).
#[must_use]
pub fn contains_glob_chars(path_pattern: &str) -> bool {
    path_pattern.contains('*')
        || path_pattern.contains('?')
        || path_pattern.contains('[')
        || path_pattern.contains(']')
}

/// Remove trailing `/**` glob suffix from a path pattern.
///
/// Used to normalize path patterns since `/**` just means "directory and
/// everything under it". If stripping leaves an empty string, returns `/`.
///
/// Ported from `sandbox-utils.js:63-66` (`removeTrailingGlobSuffix`).
#[must_use]
pub fn remove_trailing_glob_suffix(path_pattern: &str) -> String {
    let stripped = path_pattern.strip_suffix("/**").unwrap_or(path_pattern);
    if stripped.is_empty() {
        "/".to_string()
    } else {
        stripped.to_string()
    }
}

/// POSIX equivalent of Node's `path.normalize`: collapse duplicate slashes and
/// resolve `.` / `..` segments while preserving a leading `/` and a trailing
/// `/` (if present). Mirrors Node's behavior closely enough for the absolute
/// paths these utilities operate on.
fn posix_normalize(input: &str) -> String {
    if input.is_empty() {
        return ".".to_string();
    }
    let is_absolute = input.starts_with('/');
    let has_trailing_slash = input.len() > 1 && input.ends_with('/');

    let mut parts: Vec<&str> = Vec::new();
    for segment in input.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if let Some(last) = parts.last() {
                    if *last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                if !is_absolute {
                    parts.push("..");
                }
                // For absolute paths, `..` at root is dropped.
            }
            other => parts.push(other),
        }
    }

    let mut joined = parts.join("/");
    if is_absolute {
        joined = format!("/{joined}");
    } else if joined.is_empty() {
        joined = ".".to_string();
    }
    if has_trailing_slash && !joined.ends_with('/') {
        joined.push('/');
    }
    joined
}

/// POSIX equivalent of Node's `path.resolve(base, p)` where `base` is already
/// absolute. Joins `p` onto `base` (unless `p` is itself absolute) and
/// normalizes the result to an absolute path with no trailing slash (matching
/// Node, which strips trailing slashes from `resolve` output).
fn posix_resolve(base: &str, p: &str) -> String {
    let combined = if p.starts_with('/') {
        p.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), p)
    };
    let normalized = posix_normalize(&combined);
    // Node's path.resolve strips trailing slashes (except for root "/").
    if normalized.len() > 1 {
        normalized.trim_end_matches('/').to_string()
    } else {
        normalized
    }
}

/// POSIX equivalent of Node's `path.dirname`.
fn posix_dirname(p: &str) -> String {
    let normalized = p.trim_end_matches('/');
    match normalized.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(idx) => normalized[..idx].to_string(),
    }
}

/// Check if a symlink resolution crosses expected path boundaries.
///
/// When resolving symlinks for sandbox path normalization, we need to ensure
/// the resolved path doesn't unexpectedly broaden the scope. This function
/// returns `true` if the resolved path is an ancestor of the original path or
/// resolves to a system root, which would indicate the symlink points outside
/// expected boundaries.
///
/// Ported from `sandbox-utils.js:80-157` (`isSymlinkOutsideBoundary`).
#[must_use]
pub fn is_symlink_outside_boundary(original_path: &str, resolved_path: &str) -> bool {
    let normalized_original = posix_normalize(original_path);
    let normalized_resolved = posix_normalize(resolved_path);

    // Same path after normalization - OK. (`sandbox-utils.js:84-86`)
    if normalized_resolved == normalized_original {
        return false;
    }

    // Handle macOS /tmp -> /private/tmp canonical resolution.
    // (`sandbox-utils.js:91-107`)
    if normalized_original.starts_with("/tmp/")
        && normalized_resolved == format!("/private{normalized_original}")
    {
        return false;
    }
    if normalized_original.starts_with("/var/")
        && normalized_resolved == format!("/private{normalized_original}")
    {
        return false;
    }
    if normalized_original.starts_with("/private/tmp/")
        && normalized_resolved == normalized_original
    {
        return false;
    }
    if normalized_original.starts_with("/private/var/")
        && normalized_resolved == normalized_original
    {
        return false;
    }

    // If resolved path is "/" it's outside expected boundaries.
    // (`sandbox-utils.js:109-111`)
    if normalized_resolved == "/" {
        return true;
    }

    // If resolved path is very short (single component like /tmp, /usr, /var),
    // it's likely outside expected boundaries. (`sandbox-utils.js:114-117`)
    let resolved_parts: Vec<&str> = normalized_resolved
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if resolved_parts.len() <= 1 {
        return true;
    }

    // If original path starts with resolved path, the resolved path is an
    // ancestor. (`sandbox-utils.js:120-122`)
    if normalized_original.starts_with(&format!("{normalized_resolved}/")) {
        return true;
    }

    // Also check the canonical form of the original path for macOS.
    // (`sandbox-utils.js:125-135`)
    // TS branches /tmp/ and /var/ separately (sandbox-utils.js:126-131); both
    // prepend "/private", so they fold into one condition here.
    let canonical_original =
        if normalized_original.starts_with("/tmp/") || normalized_original.starts_with("/var/") {
            format!("/private{normalized_original}")
        } else {
            normalized_original.clone()
        };
    if canonical_original != normalized_original
        && canonical_original.starts_with(&format!("{normalized_resolved}/"))
    {
        return true;
    }

    // STRICT CHECK: Only allow resolutions that stay within the expected path
    // tree. (`sandbox-utils.js:136-156`)
    let resolved_starts_with_original =
        normalized_resolved.starts_with(&format!("{normalized_original}/"));
    let resolved_starts_with_canonical = canonical_original != normalized_original
        && normalized_resolved.starts_with(&format!("{canonical_original}/"));
    let resolved_is_canonical =
        canonical_original != normalized_original && normalized_resolved == canonical_original;
    let resolved_is_same = normalized_resolved == normalized_original;

    if !resolved_is_same
        && !resolved_is_canonical
        && !resolved_starts_with_original
        && !resolved_starts_with_canonical
    {
        return true;
    }

    // Allow resolution to same directory level or deeper within expected tree.
    false
}

/// Pure core of [`normalize_path_for_sandbox`]: takes the home directory and
/// current working directory as parameters so it is deterministic, and a
/// `realpath` resolver closure so the filesystem touch is injectable.
///
/// The closure receives a path and returns its canonicalized form, or `None`
/// if the path doesn't exist / can't be resolved (mirroring the upstream
/// `try { fs.realpathSync(...) } catch {}`).
///
/// Ported from `sandbox-utils.js:169-230` (`normalizePathForSandbox`).
pub fn normalize_path_for_sandbox_with<F>(
    path_pattern: &str,
    home_dir: &str,
    cwd: &str,
    realpath: F,
) -> String
where
    F: Fn(&str) -> Option<String>,
{
    // Expand ~ to home directory / resolve relatives to absolute.
    // (`sandbox-utils.js:171-186`)
    let mut normalized_path = if path_pattern == "~" {
        home_dir.to_string()
    } else if let Some(rest) = path_pattern.strip_prefix("~/") {
        // TS: homedir() + pathPattern.slice(1) — slice(1) keeps the leading
        // "/", so this is home + "/" + rest.
        format!("{home_dir}/{rest}")
    } else if path_pattern.starts_with("./")
        || path_pattern.starts_with("../")
        || !Path::new(path_pattern).is_absolute()
    {
        // TS keeps these as two separate branches (./ ../ then the generic
        // non-absolute case at sandbox-utils.js:179-186), but both call
        // path.resolve(cwd, pathPattern) identically.
        posix_resolve(cwd, path_pattern)
    } else {
        path_pattern.to_string()
    };

    // For glob patterns, resolve symlinks for the directory portion only.
    // (`sandbox-utils.js:188-213`)
    if contains_glob_chars(&normalized_path) {
        let static_prefix = split_at_first_glob(&normalized_path);
        if !static_prefix.is_empty() && static_prefix != "/" {
            let base_dir = if static_prefix.ends_with('/') {
                static_prefix[..static_prefix.len() - 1].to_string()
            } else {
                posix_dirname(&static_prefix)
            };
            if let Some(resolved_base_dir) = realpath(&base_dir) {
                if !is_symlink_outside_boundary(&base_dir, &resolved_base_dir) {
                    let pattern_suffix = &normalized_path[base_dir.len()..];
                    return format!("{resolved_base_dir}{pattern_suffix}");
                }
                // If resolution would broaden scope, keep original pattern.
            }
            // If directory doesn't exist or can't be resolved, keep original.
        }
        return normalized_path;
    }

    // Resolve symlinks to real paths to avoid bwrap issues.
    // (`sandbox-utils.js:216-228`)
    if let Some(resolved_path) = realpath(&normalized_path) {
        if !is_symlink_outside_boundary(&normalized_path, &resolved_path) {
            normalized_path = resolved_path;
        }
        // else: symlink points outside expected boundaries - keep original.
    }
    // else: path doesn't exist or can't be resolved - keep normalized path.

    normalized_path
}

/// Splits a path at the first glob character (`*`, `?`, `[`, `]`) and returns
/// the static prefix preceding it, matching the TS `split(/[*?[\]]/)[0]`.
fn split_at_first_glob(s: &str) -> String {
    match s.find(['*', '?', '[', ']']) {
        Some(idx) => s[..idx].to_string(),
        None => s.to_string(),
    }
}

/// Normalize a path for use in sandbox configurations.
///
/// Handles tilde (`~`) expansion, relative→absolute conversion, and symlink
/// resolution (with the boundary check from [`is_symlink_outside_boundary`]).
/// Glob patterns preserve their wildcards after path normalization.
///
/// Thin wrapper over [`normalize_path_for_sandbox_with`] that reads the real
/// home directory and current working directory and uses
/// [`std::fs::canonicalize`] as the `realpath` resolver.
///
/// Ported from `sandbox-utils.js:169-230` (`normalizePathForSandbox`).
#[must_use]
pub fn normalize_path_for_sandbox(path_pattern: &str) -> String {
    let home_dir = dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cwd = std::env::current_dir()
        .map_or_else(|_| "/".to_string(), |p| p.to_string_lossy().into_owned());
    normalize_path_for_sandbox_with(path_pattern, &home_dir, &cwd, |p| {
        std::fs::canonicalize(p)
            .ok()
            .map(|c| c.to_string_lossy().into_owned())
    })
}

/// Pure core of [`get_default_write_paths`]: takes the home directory as a
/// parameter.
///
/// Ported from `sandbox-utils.js:238-252` (`getDefaultWritePaths`).
#[must_use]
pub fn get_default_write_paths_with(home_dir: &str) -> Vec<String> {
    vec![
        "/dev/stdout".to_string(),
        "/dev/stderr".to_string(),
        "/dev/null".to_string(),
        "/dev/tty".to_string(),
        "/dev/dtracehelper".to_string(),
        "/dev/autofs_nowait".to_string(),
        "/tmp/claude".to_string(),
        "/private/tmp/claude".to_string(),
        format!("{home_dir}/.npm/_logs"),
        format!("{home_dir}/.lingxi/debug"),
    ]
}

/// Get recommended system paths that should be writable for commands to work
/// properly.
///
/// WARNING: these default paths are intentionally broad for compatibility but
/// may allow access to files from other processes. In highly security-sensitive
/// environments, configure more restrictive write paths.
///
/// Thin wrapper over [`get_default_write_paths_with`] reading the real home
/// directory.
///
/// Ported from `sandbox-utils.js:238-252` (`getDefaultWritePaths`).
#[must_use]
pub fn get_default_write_paths() -> Vec<String> {
    let home_dir = dirs::home_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    get_default_write_paths_with(&home_dir)
}

/// Convert a gitignore-style glob pattern into a regex source string.
///
/// Semantics (`sandbox-utils.js:394-398`):
/// - `*` matches any characters except `/` (e.g. `*.ts` matches `foo.ts` but
///   not `foo/bar.ts`).
/// - `**` matches any characters including `/`.
/// - `?` matches any single character except `/`.
/// - `[abc]` matches any character in the set.
///
/// Ported from `sandbox-utils.js:402-417` (`globToRegex`). The replacement
/// pipeline is reproduced step-for-step.
#[must_use]
pub fn glob_to_regex(glob_pattern: &str) -> String {
    static SPECIAL: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static UNCLOSED: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let special = SPECIAL.get_or_init(|| Regex::new(r"[.^$+{}()|\\]").expect("static regex"));
    let unclosed = UNCLOSED.get_or_init(|| Regex::new(r"\[([^\]]*?)$").expect("static regex"));
    // Step 1: escape regex special characters (except glob chars * ? [ ]).
    // TS: .replace(/[.^$+{}()|\\]/g, '\\$&')
    let step1 = special.replace_all(glob_pattern, r"\$0").into_owned();

    // Step 2: escape unclosed brackets (no matching ]).
    // TS: .replace(/\[([^\]]*?)$/g, '\\[$1')
    let step2 = unclosed.replace_all(&step1, r"\[$1").into_owned();

    // Step 3: convert glob patterns to regex (order matters - ** before *).
    let step3 = step2
        .replace("**/", "__GLOBSTAR_SLASH__") // Placeholder for **/
        .replace("**", "__GLOBSTAR__"); // Placeholder for **
    let step4 = step3
        .replace('*', "[^/]*") // * matches anything except /
        .replace('?', "[^/]"); // ? matches single character except /

    // Step 5: restore placeholders.
    let restored = step4
        .replace("__GLOBSTAR_SLASH__", "(.*/)?") // **/ matches zero or more dirs
        .replace("__GLOBSTAR__", ".*"); // ** matches anything including /

    format!("^{restored}$")
}

/// Pure core of [`expand_glob_pattern`]: takes the already-normalized glob
/// pattern, the base directory, and a recursive directory lister (returning
/// absolute paths of every entry under the base directory).
///
/// Returns the absolute paths that match `glob_to_regex(normalized_pattern)`.
///
/// Filesystem touch (existence + recursive walk) lives in the
/// [`expand_glob_pattern`] wrapper; this core is pure given the entry list.
fn filter_glob_matches(normalized_pattern: &str, entries: &[String]) -> Vec<String> {
    let Ok(regex) = Regex::new(&glob_to_regex(normalized_pattern)) else {
        return Vec::new();
    };
    entries
        .iter()
        .filter(|full_path| regex.is_match(full_path))
        .cloned()
        .collect()
}

/// Recursively collect absolute paths of every entry under `base_dir`,
/// matching Node's `fs.readdirSync(baseDir, { recursive: true })` enumeration
/// (every file and directory below `base_dir`, joined onto its parent).
fn read_dir_recursive(base_dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(base_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        out.push(path.to_string_lossy().into_owned());
        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            read_dir_recursive(&path, out);
        }
    }
}

/// Expand a glob pattern into concrete file paths.
///
/// Used on Linux where bubblewrap doesn't support glob patterns natively.
/// Resolves the static directory prefix, lists files recursively, and filters
/// using [`glob_to_regex`].
///
/// Ported from `sandbox-utils.js:429-471` (`expandGlobPattern`).
#[must_use]
pub fn expand_glob_pattern(glob_path: &str) -> Vec<String> {
    let normalized_pattern = normalize_path_for_sandbox(glob_path);

    // Extract the static directory prefix before any glob characters.
    let static_prefix = split_at_first_glob(&normalized_pattern);
    if static_prefix.is_empty() || static_prefix == "/" {
        tracing::debug!("[Sandbox] Glob pattern too broad, skipping: {glob_path}");
        return Vec::new();
    }

    // Get the base directory from the static prefix.
    let base_dir = if static_prefix.ends_with('/') {
        static_prefix[..static_prefix.len() - 1].to_string()
    } else {
        posix_dirname(&static_prefix)
    };

    if !Path::new(&base_dir).exists() {
        tracing::debug!("[Sandbox] Base directory for glob does not exist: {base_dir}");
        return Vec::new();
    }

    let mut entries = Vec::new();
    read_dir_recursive(Path::new(&base_dir), &mut entries);
    filter_glob_matches(&normalized_pattern, &entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DANGEROUS_FILES / get_dangerous_directories (sandbox-utils.js:10-37) ---

    #[test]
    fn dangerous_files_exact_set() {
        assert_eq!(
            DANGEROUS_FILES,
            [
                ".gitconfig",
                ".gitmodules",
                ".bashrc",
                ".bash_profile",
                ".zshrc",
                ".zprofile",
                ".profile",
                ".ripgreprc",
                ".mcp.json",
            ]
        );
    }

    #[test]
    fn dangerous_directories_const_exact_set() {
        assert_eq!(DANGEROUS_DIRECTORIES, [".git", ".vscode", ".idea"]);
    }

    #[test]
    fn dangerous_directories_excludes_git_adds_claude() {
        // TS filters out .git and appends .lingxi/commands + .lingxi/agents.
        assert_eq!(
            get_dangerous_directories(),
            vec![
                ".vscode".to_string(),
                ".idea".to_string(),
                ".lingxi/commands".to_string(),
                ".lingxi/agents".to_string(),
            ]
        );
    }

    // --- normalize_case_for_comparison (sandbox-utils.js:47-49) ---

    #[test]
    fn normalize_case_always_lowercases() {
        assert_eq!(
            normalize_case_for_comparison(".lInGxi/Settings.locaL.json"),
            ".lingxi/settings.local.json"
        );
        assert_eq!(normalize_case_for_comparison("/Foo/BAR"), "/foo/bar");
    }

    // --- contains_glob_chars (sandbox-utils.js:53-58) ---

    #[test]
    fn contains_glob_chars_detects_each() {
        assert!(contains_glob_chars("a/*.ts"));
        assert!(contains_glob_chars("file?.txt"));
        assert!(contains_glob_chars("file[abc]"));
        assert!(contains_glob_chars("file]"));
        assert!(!contains_glob_chars("/plain/path/file.txt"));
    }

    // --- remove_trailing_glob_suffix (sandbox-utils.js:63-66) ---

    #[test]
    fn remove_trailing_glob_suffix_cases() {
        assert_eq!(remove_trailing_glob_suffix("a/b/**"), "a/b");
        // Only trailing /** is stripped; a/*.ts is unchanged.
        assert_eq!(remove_trailing_glob_suffix("a/*.ts"), "a/*.ts");
        // ** in the middle is unchanged.
        assert_eq!(remove_trailing_glob_suffix("a/**/b"), "a/**/b");
        // Stripping to empty yields "/".
        assert_eq!(remove_trailing_glob_suffix("/**"), "/");
    }

    // --- is_symlink_outside_boundary (sandbox-utils.js:80-157) ---

    #[test]
    fn symlink_same_path_is_inside() {
        assert!(!is_symlink_outside_boundary(
            "/Users/me/project",
            "/Users/me/project"
        ));
    }

    #[test]
    fn symlink_resolving_deeper_is_inside() {
        // Resolved is under the original tree -> inside.
        assert!(!is_symlink_outside_boundary(
            "/Users/me/project",
            "/Users/me/project/real"
        ));
    }

    #[test]
    fn symlink_escaping_to_etc_is_outside() {
        // /Users/me/project resolving to /etc (unrelated short path) -> outside.
        assert!(is_symlink_outside_boundary("/Users/me/project", "/etc"));
    }

    #[test]
    fn symlink_resolving_to_root_is_outside() {
        assert!(is_symlink_outside_boundary("/Users/me/project", "/"));
    }

    #[test]
    fn symlink_ancestor_is_outside() {
        // original starts with resolved + "/" -> resolved is an ancestor.
        assert!(is_symlink_outside_boundary(
            "/Users/me/project/sub",
            "/Users/me/project"
        ));
    }

    #[test]
    fn symlink_macos_tmp_canonicalization_is_inside() {
        // /tmp/claude -> /private/tmp/claude is the legitimate macOS symlink.
        assert!(!is_symlink_outside_boundary(
            "/tmp/claude",
            "/private/tmp/claude"
        ));
        assert!(!is_symlink_outside_boundary(
            "/var/folders/x",
            "/private/var/folders/x"
        ));
    }

    #[test]
    fn symlink_unrelated_long_path_is_outside() {
        // /tmp/claude -> /Users/dworken/other is outside expected bounds.
        assert!(is_symlink_outside_boundary(
            "/tmp/claude",
            "/Users/dworken/other"
        ));
    }

    // --- normalize_path_for_sandbox_with (sandbox-utils.js:169-230) ---

    // A realpath resolver that returns the input unchanged (path "exists" and
    // is its own realpath) — exercises the symlink branch without FS. The
    // `Option` is required by the resolver contract, not gratuitous.
    #[allow(clippy::unnecessary_wraps)]
    fn identity_realpath(p: &str) -> Option<String> {
        Some(p.to_string())
    }

    // A realpath resolver that always fails (path "doesn't exist").
    fn missing_realpath(_p: &str) -> Option<String> {
        None
    }

    #[test]
    fn normalize_tilde_only_expands_to_home() {
        let out = normalize_path_for_sandbox_with("~", "/home/me", "/cwd", identity_realpath);
        assert_eq!(out, "/home/me");
    }

    #[test]
    fn normalize_tilde_slash_expands_to_home_subpath() {
        let out = normalize_path_for_sandbox_with("~/x/y", "/home/me", "/cwd", identity_realpath);
        assert_eq!(out, "/home/me/x/y");
    }

    #[test]
    fn normalize_relative_dot_resolves_against_cwd() {
        let out =
            normalize_path_for_sandbox_with("./foo", "/home/me", "/work/dir", identity_realpath);
        assert_eq!(out, "/work/dir/foo");
    }

    #[test]
    fn normalize_relative_dotdot_resolves_against_cwd() {
        let out =
            normalize_path_for_sandbox_with("../foo", "/home/me", "/work/dir", identity_realpath);
        assert_eq!(out, "/work/foo");
    }

    #[test]
    fn normalize_bare_relative_resolves_against_cwd() {
        let out =
            normalize_path_for_sandbox_with("foo/bar", "/home/me", "/work/dir", identity_realpath);
        assert_eq!(out, "/work/dir/foo/bar");
    }

    #[test]
    fn normalize_absolute_with_dotdot_is_collapsed_via_realpath_noop() {
        // Absolute paths are passed through; realpath identity keeps them.
        let out = normalize_path_for_sandbox_with("/a/b/c", "/home/me", "/cwd", identity_realpath);
        assert_eq!(out, "/a/b/c");
    }

    #[test]
    fn normalize_keeps_path_when_realpath_missing() {
        let out = normalize_path_for_sandbox_with("/a/b/c", "/home/me", "/cwd", missing_realpath);
        assert_eq!(out, "/a/b/c");
    }

    #[test]
    fn normalize_glob_pattern_preserves_wildcards() {
        // Glob branch: static prefix /a/b/, base dir /a/b resolves via identity.
        let out =
            normalize_path_for_sandbox_with("/a/b/*.ts", "/home/me", "/cwd", identity_realpath);
        assert_eq!(out, "/a/b/*.ts");
    }

    #[test]
    fn normalize_glob_keeps_pattern_when_base_outside_boundary() {
        // Base dir /a/b resolves to /etc (outside) -> keep original pattern.
        let out = normalize_path_for_sandbox_with("/a/b/*.ts", "/home/me", "/cwd", |p| {
            if p == "/a/b" {
                Some("/etc".to_string())
            } else {
                Some(p.to_string())
            }
        });
        assert_eq!(out, "/a/b/*.ts");
    }

    #[test]
    fn normalize_glob_uses_resolved_base_when_inside_boundary() {
        // Base dir /a/b resolves deeper to /a/b/real (inside) -> reconstruct.
        let out = normalize_path_for_sandbox_with("/a/b/*.ts", "/home/me", "/cwd", |p| {
            if p == "/a/b" {
                Some("/a/b/real".to_string())
            } else {
                Some(p.to_string())
            }
        });
        assert_eq!(out, "/a/b/real/*.ts");
    }

    // --- get_default_write_paths_with (sandbox-utils.js:238-252) ---

    #[test]
    fn default_write_paths_exact_set() {
        assert_eq!(
            get_default_write_paths_with("/home/me"),
            vec![
                "/dev/stdout".to_string(),
                "/dev/stderr".to_string(),
                "/dev/null".to_string(),
                "/dev/tty".to_string(),
                "/dev/dtracehelper".to_string(),
                "/dev/autofs_nowait".to_string(),
                "/tmp/claude".to_string(),
                "/private/tmp/claude".to_string(),
                "/home/me/.npm/_logs".to_string(),
                "/home/me/.lingxi/debug".to_string(),
            ]
        );
    }

    // --- glob_to_regex (sandbox-utils.js:402-417) ---

    #[test]
    fn glob_star_does_not_cross_slash() {
        let re = Regex::new(&glob_to_regex("*.ts")).unwrap();
        assert!(re.is_match("foo.ts"));
        assert!(!re.is_match("foo/bar.ts"));
    }

    #[test]
    fn glob_globstar_crosses_slash() {
        let re = Regex::new(&glob_to_regex("src/**/*.ts")).unwrap();
        assert!(re.is_match("src/a.ts"));
        assert!(re.is_match("src/a/b/c.ts"));
        assert!(!re.is_match("other/a.ts"));
    }

    #[test]
    fn glob_question_matches_single_non_slash() {
        let re = Regex::new(&glob_to_regex("file?.txt")).unwrap();
        assert!(re.is_match("file1.txt"));
        assert!(!re.is_match("file.txt"));
        assert!(!re.is_match("file/.txt"));
    }

    #[test]
    fn glob_char_class_matches_set() {
        let re = Regex::new(&glob_to_regex("file[0-9].txt")).unwrap();
        assert!(re.is_match("file3.txt"));
        assert!(!re.is_match("filea.txt"));
    }

    #[test]
    fn glob_escapes_regex_specials() {
        // A dot is escaped so it only matches a literal dot.
        let re = Regex::new(&glob_to_regex("a.b")).unwrap();
        assert!(re.is_match("a.b"));
        assert!(!re.is_match("axb"));
    }

    #[test]
    fn glob_unclosed_bracket_is_escaped() {
        // Unclosed [ is escaped -> matches a literal "[".
        let src = glob_to_regex("foo[bar");
        let re = Regex::new(&src).unwrap();
        assert!(re.is_match("foo[bar"));
    }

    #[test]
    fn glob_regex_exact_output_simple() {
        // Pin the exact regex source for a representative pattern.
        assert_eq!(glob_to_regex("src/**/*.ts"), r"^src/(.*/)?[^/]*\.ts$");
    }

    // --- filter_glob_matches (pure core of expand_glob_pattern) ---

    #[test]
    fn filter_glob_matches_filters_by_regex() {
        let entries = vec![
            "/a/b/foo.ts".to_string(),
            "/a/b/bar.js".to_string(),
            "/a/b/sub/baz.ts".to_string(),
        ];
        // *.ts does not cross /, so only the top-level .ts matches.
        let out = filter_glob_matches("/a/b/*.ts", &entries);
        assert_eq!(out, vec!["/a/b/foo.ts".to_string()]);
    }
}
