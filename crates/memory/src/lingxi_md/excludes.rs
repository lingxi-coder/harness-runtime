//! `claudeMdExcludes` — exclude `LINGXI.md` files matching glob patterns or
//! absolute paths from loading. 1:1 with claude-code `isLingxiMdExcluded`
//! (`claudemd.ts:547-573`) + `resolveExcludePatterns`. Only `User` / `Project` /
//! `Local` tier files are excludable; `Managed` (and AutoMem/TeamMem) are never
//! excluded.

use crate::lingxi_md::LingxiMdTier;
use glob::{MatchOptions, Pattern};
use std::path::Path;

// picomatch `{ dot: true }`: `*` matches leading dots; `*` does NOT cross `/`
// (path separator), `**` does.
fn match_opts() -> MatchOptions {
    MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    }
}

/// Compiled `claudeMdExcludes` matcher. Build once from the merged settings
/// patterns; query per file via [`Self::is_excluded`].
#[derive(Debug, Default, Clone)]
pub struct LingxiMdExcluder {
    patterns: Vec<Pattern>,
}

impl LingxiMdExcluder {
    /// Build from the merged `claudeMdExcludes` patterns. Each ABSOLUTE pattern
    /// also contributes a realpath-resolved variant (resolving its longest
    /// existing directory prefix) so a pattern written against `/tmp/...` still
    /// matches a file the OS resolved to `/private/tmp/...` (claude-code
    /// `resolveExcludePatterns`). Unparseable patterns are skipped.
    #[must_use]
    pub fn new(raw_patterns: &[String]) -> Self {
        let mut patterns = Vec::new();
        for expanded in resolve_exclude_patterns(raw_patterns) {
            if let Ok(p) = Pattern::new(&expanded) {
                patterns.push(p);
            }
        }
        Self { patterns }
    }

    /// `true` when no patterns are configured (filtering is a no-op).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Whether the `LINGXI.md` at `path` (tier `tier`) is excluded. Only
    /// `User` / `Project` / `Local` are excludable; `Managed` is never excluded
    /// (`isLingxiMdExcluded`, `claudemd.ts:548-550`).
    #[must_use]
    pub fn is_excluded(&self, path: &Path, tier: LingxiMdTier) -> bool {
        if self.patterns.is_empty()
            || !matches!(
                tier,
                LingxiMdTier::User | LingxiMdTier::Project | LingxiMdTier::Local
            )
        {
            return false;
        }
        let normalized = path.to_string_lossy().replace('\\', "/");
        let candidate = Path::new(&normalized);
        let opts = match_opts();
        self.patterns
            .iter()
            .any(|p| p.matches_path_with(candidate, opts))
    }
}

/// Expand exclude patterns by ADDING a realpath-resolved variant for each
/// ABSOLUTE pattern — keeping the original. 1:1 with claude-code `_wd`
/// (@197183958): realpath the `dirname` of the pattern's static prefix (the
/// part before the first glob char) and, if it differs, push
/// `realpath(dirname) + pattern.slice(dirname.len)`. Only the DIRNAME is
/// resolved (so a symlinked final component of the static prefix stays
/// unresolved — matching the binary, NOT the longest-existing-prefix). A
/// non-existent dirname (realpath throws) adds no variant.
fn resolve_exclude_patterns(patterns: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in patterns {
        let normalized = raw.replace('\\', "/");
        if normalized.is_empty() {
            continue;
        }
        out.push(normalized.clone());
        if !normalized.starts_with('/') {
            continue;
        }
        let glob_start = normalized
            .find(['*', '?', '{', '['])
            .unwrap_or(normalized.len());
        let static_prefix = &normalized[..glob_start];
        let dirname = node_dirname(static_prefix);
        if let Ok(real) = std::fs::canonicalize(dirname) {
            let real = real.to_string_lossy().replace('\\', "/");
            if real != dirname {
                out.push(format!("{real}{}", &normalized[dirname.len()..]));
            }
        }
    }
    out
}

/// Node `path.dirname` for a POSIX path: drop one trailing `/`, then return the
/// portion before the last `/`. `/tmp/foo`→`/tmp`, `/tmp/foo/`→`/tmp`,
/// `/tmp`→`/`, `/`→`/`. The result is always a byte-prefix of the input (so the
/// `pattern.slice(dirname.len)` re-append is correct).
fn node_dirname(s: &str) -> &str {
    let trimmed = s.strip_suffix('/').unwrap_or(s);
    if trimmed.is_empty() {
        return "/";
    }
    match trimmed.rfind('/') {
        Some(0) => "/",
        Some(i) => &trimmed[..i],
        None => ".",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_excluder_excludes_nothing() {
        let ex = LingxiMdExcluder::new(&[]);
        assert!(ex.is_empty());
        assert!(!ex.is_excluded(Path::new("/p/LINGXI.md"), LingxiMdTier::Project));
    }

    #[test]
    fn managed_tier_is_never_excluded() {
        let ex = LingxiMdExcluder::new(&["**/LINGXI.md".to_string()]);
        // A glob that DOES match the path, but Managed tier is exempt.
        assert!(!ex.is_excluded(Path::new("/mgr/LINGXI.md"), LingxiMdTier::Managed));
        assert!(ex.is_excluded(Path::new("/proj/LINGXI.md"), LingxiMdTier::Project));
    }

    #[test]
    fn node_dirname_matches_posix_semantics() {
        // Node path.dirname semantics; result is always a byte-prefix of input.
        assert_eq!(node_dirname("/tmp/foo"), "/tmp");
        assert_eq!(node_dirname("/tmp/foo/"), "/tmp");
        assert_eq!(node_dirname("/tmp/foo/bar.md"), "/tmp/foo");
        assert_eq!(node_dirname("/tmp"), "/");
        assert_eq!(node_dirname("/tmp/"), "/");
        assert_eq!(node_dirname("/"), "/");
        // The static prefix of `/a/b/*.md` is `/a/b/`; its dirname is `/a` — so
        // only `/a` (not `/a/b`) is realpath-resolved, matching the binary `_wd`.
        assert_eq!(node_dirname("/a/b/"), "/a");
    }

    #[test]
    fn glob_and_absolute_patterns_match() {
        let ex = LingxiMdExcluder::new(&[
            "**/secret/LINGXI.md".to_string(),
            "/etc/proj/LINGXI.md".to_string(),
        ]);
        assert!(ex.is_excluded(Path::new("/a/secret/LINGXI.md"), LingxiMdTier::User));
        assert!(ex.is_excluded(Path::new("/etc/proj/LINGXI.md"), LingxiMdTier::Local));
        assert!(!ex.is_excluded(Path::new("/a/public/LINGXI.md"), LingxiMdTier::User));
        // `*` does not cross `/`.
        assert!(
            !ex.is_excluded(Path::new("/a/b/LINGXI.md"), LingxiMdTier::Project)
                && LingxiMdExcluder::new(&["/a/*/LINGXI.md".to_string()])
                    .is_excluded(Path::new("/a/b/LINGXI.md"), LingxiMdTier::Project)
        );
    }
}
