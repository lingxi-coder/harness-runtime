//! claude-code path-pattern resolution. Ports
//! `resolvePathPatternForSandbox` from
//! `claude-code/src/utils/sandbox/sandbox-adapter.ts`.
//!
//! Three CC-specific conventions:
//! - `//path` → absolute from filesystem root (strip one leading `/`).
//! - `/path`  → relative to the settings file directory.
//! - `~/path`, `./path`, bare `path` → passthrough; the sandbox-runtime layer
//!   handles tilde expansion / cwd relativization later.

use std::path::{Path, PathBuf};

/// Lexically collapse `.` and `..` segments in an ABSOLUTE POSIX path, matching
/// Node's `path.normalize`/`path.resolve` segment logic WITHOUT touching the
/// filesystem (the target may not exist, so we must NOT canonicalize).
///
/// claude-code resolves these patterns through Node `resolve()`
/// (sandbox-adapter.ts:113/135) and `normalize()`/`resolve()` inside `expandPath`
/// (path.ts:79-84), both of which fold `.`/`..` lexically. We mirror that:
/// - `.` segments are dropped;
/// - `..` pops the previous real segment;
/// - a `..` with nothing to pop clamps at root (Node: `normalize("/..") === "/"`);
/// - a trailing slash is NOT preserved (Node collapses `/a/b/` only when the
///   last segment is `.`/`..`; for parity with our join-then-normalize callers
///   we keep it simple and drop trailing separators, which matches how these
///   resolved paths are consumed downstream).
///
/// Input MUST be absolute (start with `/`); callers only normalize after they
/// have produced an absolute path.
pub(crate) fn lexically_normalize_absolute(path: &str) -> String {
    debug_assert!(
        path.starts_with('/'),
        "lexically_normalize_absolute requires an absolute path"
    );
    let mut stack: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {} // skip empty (from leading/duplicate `/`) and `.`
            ".." => {
                // Pop the previous real segment; clamp at root if none.
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", stack.join("/"))
    }
}

/// Resolve `pattern` according to claude-code's three permission-rule path
/// prefix conventions.
///
/// `settings_dir` is the directory the settings file with this rule lives in.
/// For `~/.lingxi/settings.json` that's `~/.claude`. For
/// `<project>/.lingxi/settings.json` that's `<project>/.claude`. For ad-hoc /
/// in-memory settings, callers may pass any path; only `/path` patterns are
/// affected.
#[must_use]
pub fn resolve_path_pattern_for_sandbox(pattern: &str, settings_dir: &Path) -> String {
    // `//path` → strip ONE leading slash. `//etc` → `/etc`.
    if let Some(stripped) = pattern.strip_prefix("//") {
        // The remaining string is already absolute-from-root (claude-code uses
        // this to escape the `/path = settings-relative` convention).
        return format!("/{stripped}");
    }

    // `/path` → relative to settings file directory.
    if let Some(stripped) = pattern.strip_prefix('/') {
        // Skip empty strip (the only way is the input was a single `/`, which
        // we treat as "the settings dir itself").
        let mut out: PathBuf = settings_dir.to_path_buf();
        if !stripped.is_empty() {
            out.push(stripped);
        }
        let joined = out.to_string_lossy().into_owned();
        // claude-code uses Node `resolve(root, slice(1))` here, which lexically
        // collapses `.`/`..` (sandbox-adapter.ts:113). Mirror that — but only
        // when the join produced an absolute path (settings_dir normally is).
        if joined.starts_with('/') {
            return lexically_normalize_absolute(&joined);
        }
        return joined;
    }

    // Everything else passes through unchanged — sandbox-runtime
    // (or the Rust equivalent) will handle `~/`, `./`, and bare paths.
    pattern.to_string()
}

#[cfg(test)]
mod tests {
    use super::{lexically_normalize_absolute, resolve_path_pattern_for_sandbox};
    use std::path::Path;

    #[test]
    fn normalize_collapses_dot_dot() {
        assert_eq!(lexically_normalize_absolute("/a/../b"), "/b");
        assert_eq!(lexically_normalize_absolute("/a/./b"), "/a/b");
        assert_eq!(lexically_normalize_absolute("/a/b/../c"), "/a/c");
        assert_eq!(lexically_normalize_absolute("/a//b"), "/a/b");
    }

    #[test]
    fn normalize_clamps_at_root() {
        // Node: normalize("/..") === "/"; trailing `..` past root clamps.
        assert_eq!(lexically_normalize_absolute("/.."), "/");
        assert_eq!(lexically_normalize_absolute("/a/../.."), "/");
        assert_eq!(lexically_normalize_absolute("/"), "/");
    }

    #[test]
    fn path_pattern_normalizes_settings_relative() {
        let dir = Path::new("/proj/.lingxi");
        // `/x/../y` joined under settings dir then collapsed → `<dir>/y`.
        assert_eq!(
            resolve_path_pattern_for_sandbox("/x/../y", dir),
            "/proj/.lingxi/y"
        );
        // `//abs` escape strips one slash and does NOT settings-relativize.
        assert_eq!(resolve_path_pattern_for_sandbox("//etc", dir), "/etc");
        // passthrough patterns are untouched.
        assert_eq!(resolve_path_pattern_for_sandbox("./src", dir), "./src");
        assert_eq!(resolve_path_pattern_for_sandbox("~/x", dir), "~/x");
    }
}
