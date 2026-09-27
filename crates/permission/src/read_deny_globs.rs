//! `read_deny_exclude_globs` — turn active `Read`-`deny` permission rules into
//! ripgrep `--glob !pattern` exclude strings, so the `Grep`/`Glob` tools never
//! surface a path the user denied reads to.
//!
//! 1:1 port of claude-code's `F4e(U4e(<context>), <cwd>)` (used by `GrepTool`'s
//! `call()` and `glob.ts`'s `lLa()`):
//!
//! - `U4e(ctx)` (`buildReadDenyPatternsByRoot`): walk the active `read`/`deny`
//!   rules, resolve each to `(relativePattern, root)` via `Iqt`/`k_m`
//!   (LingXi: [`crate::filesystem::pattern_with_root`] +
//!   [`crate::filesystem::root_path_for_source`]), collapse repeated slashes,
//!   and group the relative patterns by their resolved root (`null` root = a
//!   bare relative/anywhere pattern).
//! - `F4e(byRoot, cwd)` (`flattenReadDenyGlobs`): the `null`-root patterns pass
//!   through verbatim; every rooted pattern is rebased onto `cwd` via
//!   `H_m`/`AHo` ([`relativize_for_cwd`]) — producing a `/`-anchored glob
//!   exclude (or being dropped when the pattern's root is unreachable from cwd).
//!
//! The Windows-specific normalizations inside `Iqt`/`IMl` are out of scope for
//! the posix-first port (documented residual in `read_deny_globs` and mirrored
//! by [`crate::filesystem::pattern_with_root`], which already omits them).
//!
//! Consumed by `tool-file`'s `Grep`/`Glob` (via `BuiltinToolContext`), which
//! prefix each returned string per the reference: `Grep` emits
//! `P.startsWith("/") ? "!"+P : "!**/"+P`, `Glob` emits `"!"+P` (because every
//! rooted entry already carries a leading `/`).

use crate::filesystem::{pattern_with_root, FsRoots};
use crate::policy::PermissionPolicy;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// Resolve every active `Read`-`deny` rule in `policy` to a ripgrep `--glob`
/// exclude string (WITHOUT the leading `!`), rebased onto `cwd`.
///
/// 1:1 with claude-code `F4e(U4e(toolPermissionContext), cwd)`. The returned
/// strings are de-duplicated (preserving first-seen order) and each is either:
/// - a `/`-anchored path (rooted patterns, e.g. `/secrets/**`), which `Grep`
///   prefixes `!` and `Glob` prefixes `!`; or
/// - a bare relative pattern (unrooted, e.g. `.env`), which `Grep` prefixes
///   `!**/` and `Glob` (after F4e prepends nothing) keeps relative.
///
/// `cwd` should be the search root the `ignore`/`rg` walk is anchored at (the
/// reference passes `Pt()` for `Grep` and the search dir `i` for `Glob`).
///
/// When the policy has no `Read`-deny rules the result is empty and the
/// caller's behavior is unchanged.
#[must_use]
pub fn read_deny_exclude_globs(policy: &PermissionPolicy, cwd: &Path) -> Vec<String> {
    // Resolve patterns against the same roots the policy matches with. Without
    // roots, `root_path_for_source` is meaningless; fall back to a roots view
    // anchored at `cwd` so rooted patterns still rebase correctly (claude-code
    // always has a populated context here).
    let roots_owned;
    let roots = match policy.roots.as_ref() {
        Some(r) => r,
        None => {
            roots_owned = FsRoots {
                cwd: cwd.to_path_buf(),
                home: None,
                lingxi_home: cwd.to_path_buf(),
            };
            &roots_owned
        }
    };

    // ── U4e: group read-deny relativePatterns by resolved root ──────────────
    // `null` root patterns live under the `None` key; rooted patterns under
    // `Some(root)`. A BTreeMap keeps iteration deterministic across runs.
    let mut by_root: BTreeMap<Option<PathBuf>, Vec<String>> = BTreeMap::new();
    for (&source, rules) in &policy.deny_rules {
        for rule in rules {
            // Read-deny only: the rule must target the `Read` tool and carry a
            // content (path) pattern (`ruleContentField: "path"`). Tool-wide
            // `Read` deny rules (`rule_content == None`) have no path to exclude.
            if rule.value.tool_name != "Read" {
                continue;
            }
            let Some(pattern) = rule.value.rule_content.as_deref() else {
                continue;
            };
            let (rel_pattern, root) = pattern_with_root(pattern, source, roots);
            // `u = l.replace(/\/{2,}/g, "/")` — collapse repeated slashes.
            let rel_pattern = collapse_slashes(&rel_pattern);
            // De-dup within a root group (TS uses a Map<pattern, rule>, so a
            // repeated pattern keeps a single entry).
            let bucket = by_root.entry(root).or_default();
            if !bucket.contains(&rel_pattern) {
                bucket.push(rel_pattern);
            }
        }
    }

    // ── F4e: flatten the grouped patterns into cwd-relative exclude globs ────
    // `n = new Set(byRoot.get(null) ?? [])` — the unrooted patterns pass
    // through verbatim. Then each rooted pattern is rebased onto cwd.
    let mut out: Vec<String> = Vec::new();
    if let Some(unrooted) = by_root.get(&None) {
        for p in unrooted {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
    }
    for (root, patterns) in &by_root {
        let Some(root) = root.as_ref() else {
            continue; // null handled above
        };
        for pattern in patterns {
            if let Some(glob) = relativize_for_cwd(root, pattern, cwd) {
                if !out.contains(&glob) {
                    out.push(glob);
                }
            }
        }
    }
    out
}

/// `H_m({patternRoot, pattern, rootPath})` — rebase `pattern` (relative to
/// `pattern_root`) onto `cwd`, returning a `/`-anchored glob exclude string, or
/// `None` when the pattern's root is unreachable from `cwd` (escapes upward).
///
/// Posix-faithful (case-folding via [`case_fold`] mirrors `IA`):
/// - root == cwd → `/pattern` (`AHo(pattern)`).
/// - joined path under cwd → `/<joined.slice(cwd.len())>` (`AHo(r.slice(n.length))`).
/// - else, relative cwd→root: if it escapes (`""`/`..`/`../…`) → `None`;
///   otherwise `/<rel>/pattern` (`AHo(join(rel, pattern))`).
fn relativize_for_cwd(pattern_root: &Path, pattern: &str, cwd: &Path) -> Option<String> {
    // `r = posix.join(patternRoot, pattern)`.
    let joined = posix_join(pattern_root, pattern);
    let o = case_fold(pattern_root); // IA(patternRoot)
    let s = case_fold(cwd); // IA(cwd)

    if o == s {
        // root == cwd → AHo(pattern) = "/" + pattern.
        return Some(aho(pattern));
    }
    // `IA(r).startsWith(`${s}/`)` — the joined path lives under cwd.
    let s_with_sep = format!("{s}/");
    if case_fold(Path::new(&joined)).starts_with(&s_with_sep) {
        // `r.slice(n.length)` — slice the ORIGINAL (non-folded) joined string
        // by the cwd byte length (TS slices the un-folded `r`).
        let cwd_str = cwd.to_string_lossy();
        let sliced = &joined[cwd_str.len()..];
        return Some(aho(sliced));
    }
    // `i = posix.relative(s, o)` — cwd → patternRoot. Drop if it escapes.
    let rel = posix_relative_str(cwd, pattern_root);
    if rel.is_empty() || rel == ".." || rel.starts_with("../") {
        return None;
    }
    // `AHo(posix.join(i, pattern))`.
    Some(aho(&posix_join_str(&rel, pattern)))
}

/// `AHo(e) = posix.join("/", e)` — prepend `/` and normalize the join.
fn aho(rel: &str) -> String {
    // posix.join("/", rel) collapses `//` at the seam and a trailing slash is
    // preserved only when present; a leading `/` on `rel` is fine (join keeps a
    // single separator). Implement directly to avoid platform `Path` surprises.
    let trimmed = rel.strip_prefix('/').unwrap_or(rel);
    format!("/{trimmed}")
}

/// `e.toLowerCase().replace(/ı/g,"i").replace(/ſ/g,"s")` — `IA`.
fn case_fold(p: &Path) -> String {
    p.to_string_lossy()
        .to_lowercase()
        .replace('\u{0131}', "i")
        .replace('\u{017f}', "s")
}

/// `relativePattern.replace(/\/{2,}/g, "/")` — collapse runs of `/` to one.
fn collapse_slashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_slash = false;
    for ch in s.chars() {
        if ch == '/' {
            if !prev_slash {
                out.push('/');
            }
            prev_slash = true;
        } else {
            out.push(ch);
            prev_slash = false;
        }
    }
    out
}

/// `posix.join(base, rel)` for an absolute `base` path and a relative string,
/// returning the joined POSIX string (lexically normalized like Node's join).
fn posix_join(base: &Path, rel: &str) -> String {
    posix_join_str(&base.to_string_lossy(), rel)
}

/// `posix.join(base, rel)` over two strings (Node-style normalize).
fn posix_join_str(base: &str, rel: &str) -> String {
    let combined = if base.ends_with('/') {
        format!("{base}{rel}")
    } else {
        format!("{base}/{rel}")
    };
    posix_normalize(&combined)
}

/// Lexical `path.posix.normalize` — collapse `.`/`..` and repeated slashes,
/// preserving a leading `/`.
fn posix_normalize(p: &str) -> String {
    let is_abs = p.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(stack.last(), Some(&s) if s != "..") {
                    stack.pop();
                } else if !is_abs {
                    stack.push("..");
                }
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    let trailing = p.ends_with('/') && !joined.is_empty();
    match (is_abs, trailing) {
        (true, true) => format!("/{joined}/"),
        (true, false) => format!("/{joined}"),
        (false, true) => format!("{joined}/"),
        (false, false) if joined.is_empty() => ".".to_string(),
        (false, false) => joined,
    }
}

/// POSIX `path.relative(from, to)` over two absolute paths, returning the
/// `/`-joined relative string (`""` when equal, `..`-prefixed when `to` escapes
/// `from`).
fn posix_relative_str(from: &Path, to: &Path) -> String {
    let f: Vec<&str> = normal_components(from);
    let t: Vec<&str> = normal_components(to);
    let mut i = 0;
    while i < f.len() && i < t.len() && f[i] == t[i] {
        i += 1;
    }
    let mut parts: Vec<&str> = Vec::new();
    for _ in i..f.len() {
        parts.push("..");
    }
    parts.extend_from_slice(&t[i..]);
    parts.join("/")
}

/// Normal path components as `&str` (root + `.`/`..` collapsed by the OS path
/// component iterator; only `Normal` segments are kept after a leading root).
fn normal_components(p: &Path) -> Vec<&str> {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            Component::RootDir => None,
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rule::{
        PermissionBehavior, PermissionRule, PermissionRuleSource, PermissionRuleValue,
    };

    fn roots(cwd: &str) -> FsRoots {
        FsRoots {
            cwd: PathBuf::from(cwd),
            home: Some(PathBuf::from("/home/u")),
            lingxi_home: PathBuf::from("/home/u/.lingxi"),
        }
    }

    fn read_deny(content: &str, source: PermissionRuleSource) -> PermissionRule {
        PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some(content.into()),
            },
            behavior: PermissionBehavior::Deny,
            source,
        }
    }

    fn policy_with(rules: Vec<PermissionRule>, cwd: &str) -> PermissionPolicy {
        PermissionPolicy::from_rules(crate::mode::PermissionMode::Default, rules)
            .with_roots(roots(cwd))
    }

    #[test]
    fn relative_rule_passes_through_verbatim() {
        // `Read(./secrets/**)` → unrooted, leading `./` stripped → `secrets/**`.
        let p = policy_with(
            vec![read_deny(
                "./secrets/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            )],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert_eq!(globs, vec!["secrets/**".to_string()]);
    }

    #[test]
    fn bare_relative_rule_passes_through() {
        // `Read(.env)` → unrooted → `.env` (Grep will prefix `!**/`).
        let p = policy_with(
            vec![read_deny(
                ".env",
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            )],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert_eq!(globs, vec![".env".to_string()]);
    }

    #[test]
    fn rooted_rule_at_cwd_becomes_anchored() {
        // A `/secrets/**` project rule resolves root = cwd (/proj) and pattern
        // `/secrets/**`; H_m with root==cwd → AHo("/secrets/**") = "/secrets/**".
        let p = policy_with(
            vec![read_deny(
                "/secrets/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            )],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert_eq!(globs, vec!["/secrets/**".to_string()]);
    }

    #[test]
    fn user_settings_rule_under_cwd_is_rebased() {
        // A UserSettings rule resolves against lingxi_home (/home/u/.claude).
        // If cwd is /home/u/.claude, `/x` rebases to `/x`.
        let p = policy_with(
            vec![read_deny(
                "/x/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::User),
            )],
            "/home/u/.lingxi",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/home/u/.lingxi"));
        assert_eq!(globs, vec!["/x/**".to_string()]);
    }

    #[test]
    fn rooted_rule_outside_cwd_is_dropped() {
        // A UserSettings rule rooted at /home/u/.claude, with cwd at /proj
        // (a sibling that does NOT contain .claude) → relative escapes → None.
        let p = policy_with(
            vec![read_deny(
                "/secret/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::User),
            )],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert!(globs.is_empty(), "out-of-cwd rule dropped: {globs:?}");
    }

    #[test]
    fn rooted_rule_in_subdir_of_cwd_keeps_prefix() {
        // UserSettings rule root = /home/u/.claude; cwd = /home/u. The root is a
        // subdir of cwd, so a `/c/**` pattern rebases to `/.lingxi/c/**`.
        let p = policy_with(
            vec![read_deny(
                "/c/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::User),
            )],
            "/home/u",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/home/u"));
        assert_eq!(globs, vec!["/.lingxi/c/**".to_string()]);
    }

    #[test]
    fn tilde_rule_rebases_against_home() {
        // `Read(~/private/**)` → root = home (/home/u), pattern `/private/**`.
        // cwd = /home/u → AHo("/private/**") = "/private/**".
        let p = policy_with(
            vec![read_deny(
                "~/private/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            )],
            "/home/u",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/home/u"));
        assert_eq!(globs, vec!["/private/**".to_string()]);
    }

    #[test]
    fn non_read_deny_rules_are_ignored() {
        // An Edit-deny and a Bash-deny must NOT contribute read excludes.
        let edit = PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Edit".into(),
                rule_content: Some("/secrets/**".into()),
            },
            behavior: PermissionBehavior::Deny,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        };
        let p = policy_with(vec![edit], "/proj");
        assert!(read_deny_exclude_globs(&p, Path::new("/proj")).is_empty());
    }

    #[test]
    fn allow_and_ask_read_rules_are_ignored() {
        // Only DENY rules feed the exclude set.
        let allow = PermissionRule {
            value: PermissionRuleValue {
                tool_name: "Read".into(),
                rule_content: Some("/secrets/**".into()),
            },
            behavior: PermissionBehavior::Allow,
            source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
        };
        let p = policy_with(vec![allow], "/proj");
        assert!(read_deny_exclude_globs(&p, Path::new("/proj")).is_empty());
    }

    #[test]
    fn tool_wide_read_deny_has_no_path_so_no_glob() {
        // `Read` (no content) is tool-wide — there is no path to exclude.
        let p = PermissionPolicy::from_rules(
            crate::mode::PermissionMode::Default,
            vec![PermissionRule {
                value: PermissionRuleValue {
                    tool_name: "Read".into(),
                    rule_content: None,
                },
                behavior: PermissionBehavior::Deny,
                source: PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            }],
        )
        .with_roots(roots("/proj"));
        assert!(read_deny_exclude_globs(&p, Path::new("/proj")).is_empty());
    }

    #[test]
    fn empty_policy_returns_empty() {
        let p =
            PermissionPolicy::from_rules(crate::mode::PermissionMode::Default, std::iter::empty())
                .with_roots(roots("/proj"));
        assert!(read_deny_exclude_globs(&p, Path::new("/proj")).is_empty());
    }

    #[test]
    fn duplicate_patterns_dedup() {
        let p = policy_with(
            vec![
                read_deny(
                    "./secrets/**",
                    PermissionRuleSource::Settings(protocol::SettingsScope::Project),
                ),
                read_deny(
                    "./secrets/**",
                    PermissionRuleSource::Settings(protocol::SettingsScope::Local),
                ),
            ],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert_eq!(globs, vec!["secrets/**".to_string()]);
    }

    #[test]
    fn repeated_slashes_collapse() {
        let p = policy_with(
            vec![read_deny(
                "/a//b///c/**",
                PermissionRuleSource::Settings(protocol::SettingsScope::Project),
            )],
            "/proj",
        );
        let globs = read_deny_exclude_globs(&p, Path::new("/proj"));
        assert_eq!(globs, vec!["/a/b/c/**".to_string()]);
    }
}
