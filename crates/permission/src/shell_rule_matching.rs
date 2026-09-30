//! Shell permission-rule matching — faithful port of claude-code
//! `src/utils/permissions/shellRuleMatching.ts`.
//!
//! A `Bash(...)`/`PowerShell(...)` rule's CONTENT is parsed into one of three
//! shapes and matched against a (already split + stripped) shell command:
//!
//! - **prefix** — legacy `npm:*` syntax → matches `npm` exactly or any command
//!   starting `npm ` (word boundary).
//! - **wildcard** — contains an unescaped `*` (and does NOT end in `:*`) →
//!   glob-style match where `*` ⇒ `.*`. `\*` matches a literal asterisk and
//!   `\\` a literal backslash.
//! - **exact** — everything else → byte-exact command match.
//!
//! The higher-level deny/ask/allow aggregation over compound commands lives in
//! [`crate::shell_command`]; this module is the pure per-pattern matcher.

use regex::Regex;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Null-byte sentinels for escaped wildcard/backslash, mirroring the TS
/// `ESCAPED_STAR_PLACEHOLDER` / `ESCAPED_BACKSLASH_PLACEHOLDER`. Null bytes can
/// never appear in a real command, so they are collision-free placeholders.
const ESCAPED_STAR_PLACEHOLDER: &str = "\u{0}ESCAPED_STAR\u{0}";
const ESCAPED_BACKSLASH_PLACEHOLDER: &str = "\u{0}ESCAPED_BACKSLASH\u{0}";
/// Placeholder for a `/**/` (globstar) run, mirroring the TS `\x00GLOBSTAR\x00`
/// sentinel. A globstar run compiles to `/(?:.*/)?` (zero-or-more path
/// segments), so `cat /a/**/b` matches `cat /a/b` as well as `cat /a/x/y/b`.
const GLOBSTAR_PLACEHOLDER: &str = "\u{0}GLOBSTAR\u{0}";

/// A parsed shell permission rule (claude-code `ShellPermissionRule`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellRule {
    /// Byte-exact command match.
    Exact(String),
    /// Legacy `prefix:*` — matches `prefix` or `prefix <args>`.
    Prefix(String),
    /// Glob pattern with unescaped `*` wildcards.
    Wildcard(String),
}

/// Extract the prefix from legacy `:*` syntax (`"npm:*"` → `Some("npm")`,
/// `"npm"` → `None`). Faithful to `permissionRuleExtractPrefix` (regex
/// `^(.+):\*$` — the prefix is non-empty and greedy).
#[must_use]
pub fn permission_rule_extract_prefix(rule: &str) -> Option<String> {
    let stripped = rule.strip_suffix(":*")?;
    if stripped.is_empty() {
        // `^(.+)` requires at least one char before `:*`.
        return None;
    }
    Some(stripped.to_string())
}

/// Does `pattern` contain an UNESCAPED `*` that is not the legacy `:*` suffix?
/// Faithful to `hasWildcards`: a trailing `:*` is prefix syntax (returns
/// `false`); otherwise a `*` preceded by an even number of backslashes (incl.
/// zero) is unescaped.
#[must_use]
pub fn has_wildcards(pattern: &str) -> bool {
    if pattern.ends_with(":*") {
        return false;
    }
    // A `*` is unescaped iff preceded by an even number of backslashes.
    let mut backslashes = 0usize;
    for c in pattern.chars() {
        match c {
            '\\' => backslashes += 1,
            '*' => {
                if backslashes.is_multiple_of(2) {
                    return true;
                }
                backslashes = 0;
            }
            _ => backslashes = 0,
        }
    }
    false
}

/// Parse a rule-content string into a [`ShellRule`] (claude-code
/// `parsePermissionRule`): legacy `:*` prefix first, then wildcard, else exact.
#[must_use]
pub fn parse_shell_rule(rule: &str) -> ShellRule {
    if let Some(prefix) = permission_rule_extract_prefix(rule) {
        return ShellRule::Prefix(prefix);
    }
    if has_wildcards(rule) {
        return ShellRule::Wildcard(rule.to_string());
    }
    ShellRule::Exact(rule.to_string())
}

/// Collapse runs of spaces/tabs to a single space — 2.1.211 `replace(/[ \t]+/g," ")`.
/// Used by the `normalize_whitespace` mode of the wildcard matcher (`Ale`'s `n`
/// flag / `cxt`) to make a Bash rule authored with single spaces still match a
/// command with doubled internal whitespace.
fn collapse_whitespace(s: &str) -> std::borrow::Cow<'_, str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"[ \t]+").unwrap());
    re.replace_all(s, " ")
}

/// Match `command` against a wildcard `pattern` where `*` matches any sequence
/// (dotAll — `*` spans embedded newlines), `\*` a literal `*`, and `\\` a
/// literal `\`. Faithful to `matchWildcardPattern` (`Ale`) with both extra
/// flags defaulted off — the shared entry point used by the Grep/Glob/hooks
/// path (`Ale(t,e)` two-arg). Bash/PowerShell rule matching goes through
/// [`match_wildcard_pattern_ex`] instead.
///
/// Special case: when the pattern ends in ` *` (space + the ONLY unescaped
/// wildcard), the trailing space-and-args is made optional so `git *` matches
/// both `git add` and bare `git` (aligning with `git:*` prefix semantics).
#[must_use]
pub fn match_wildcard_pattern(pattern: &str, command: &str, case_insensitive: bool) -> bool {
    match_wildcard_pattern_ex(pattern, command, case_insensitive, false)
}

/// Full `matchWildcardPattern` (`Ale(e,t,r,n)`): `case_insensitive` = TS `r`,
/// `normalize_whitespace` = TS `n`. When `normalize_whitespace` is true, runs of
/// spaces/tabs in BOTH the (trimmed) pattern and the command collapse to a
/// single space before matching (`i`/`s` in `Ale`). The Bash wildcard-rule path
/// always enables it (`cxt(e,t)=Ale(e,t,!1,!0)`); the PowerShell path uses
/// `Ale(...,!0,!0)` (both flags on).
#[must_use]
pub fn match_wildcard_pattern_ex(
    pattern: &str,
    command: &str,
    case_insensitive: bool,
    normalize_whitespace: bool,
) -> bool {
    let trimmed_raw = pattern.trim();
    // TS `i` (pattern) and `s` (command) — collapse `[ \t]+`→" " only when the
    // whitespace-normalize flag is on. The pattern is trimmed first (TS `o`).
    let (trimmed_owned, command_owned);
    let (trimmed, command): (&str, &str) = if normalize_whitespace {
        trimmed_owned = collapse_whitespace(trimmed_raw).into_owned();
        command_owned = collapse_whitespace(command).into_owned();
        (trimmed_owned.as_str(), command_owned.as_str())
    } else {
        (trimmed_raw, command)
    };

    // Phase 1: replace escape sequences `\*` and `\\` with placeholders so the
    // regex-escaping pass below leaves them alone.
    let mut processed = String::with_capacity(trimmed.len());
    let chars: Vec<char> = trimmed.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            match next {
                '*' => {
                    processed.push_str(ESCAPED_STAR_PLACEHOLDER);
                    i += 2;
                    continue;
                }
                '\\' => {
                    processed.push_str(ESCAPED_BACKSLASH_PLACEHOLDER);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        processed.push(c);
        i += 1;
    }

    // Count unescaped `*` (the placeholders no longer contain a bare `*`).
    let unescaped_star_count = processed.matches('*').count();

    // Phase 2: escape regex metacharacters EXCEPT `*`. The TS set also escapes
    // `'"` but those are not Rust-regex metacharacters (and `\'` is an illegal
    // Rust escape), so we escape only the genuine metacharacters — behaviorally
    // identical since quotes are literals either way.
    let mut escaped = String::with_capacity(processed.len());
    for c in processed.chars() {
        if matches!(
            c,
            '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }

    // Phase 2.5 (GLOBSTAR): replace each `/(?:**/)+` run (`/**/`, `/**/**/`, …)
    // with a placeholder BEFORE the `*`→`.*` pass, exactly as TS applies `fmg`
    // between metachar-escaping and `replaceAll("*",".*")`. The metachar-escape
    // does not touch `*` or `/`, and escaped stars are already placeholders, so
    // only genuine unescaped globstar runs match here.
    static GLOBSTAR_RE: OnceLock<Regex> = OnceLock::new();
    let globstar_re = GLOBSTAR_RE.get_or_init(|| Regex::new(r"/(?:\*\*/)+").unwrap());
    let with_globstar = globstar_re
        .replace_all(&escaped, GLOBSTAR_PLACEHOLDER)
        .into_owned();

    // Phase 3: unescaped `*` → `.*`. The GLOBSTAR placeholder holds no `*`.
    let with_wildcards = with_globstar.replace('*', ".*");

    // Phase 4: placeholders → literal-regex forms. The placeholders were escaped
    // in phase 2 only via their (absent) metacharacters; the literal text
    // `\u{0}ESCAPED_STAR\u{0}` survives intact, so replace it wholesale. The
    // GLOBSTAR placeholder becomes `/(?:.*/)?` (zero-or-more `/`-delimited
    // segments), matching TS `mmg`→`/(?:.*/)?`.
    let mut regex_pattern = with_wildcards
        .replace(GLOBSTAR_PLACEHOLDER, "/(?:.*/)?")
        .replace(ESCAPED_STAR_PLACEHOLDER, "\\*")
        .replace(ESCAPED_BACKSLASH_PLACEHOLDER, "\\\\");

    // Phase 5: trailing ` *` (now ` .*`) as the sole unescaped wildcard → make
    // the space-and-args optional.
    if regex_pattern.ends_with(" .*") && unescaped_star_count == 1 {
        let base_len = regex_pattern.len() - 3;
        regex_pattern.truncate(base_len);
        regex_pattern.push_str("( .*)?");
    }

    // Phase 6: anchor + dotAll (+ optional case-insensitive) and test.
    let flags = if case_insensitive { "(?si)" } else { "(?s)" };
    let full = format!("^{flags}{regex_pattern}$");
    match cached_wildcard_regex(&full) {
        Ok(re) => re.is_match(command),
        Err(e) => {
            // A malformed user pattern must not crash the permission check; a
            // pattern that won't compile simply matches nothing.
            tracing::warn!(pattern, error = %e, "unparseable shell wildcard rule; treated as no-match");
            false
        }
    }
}

fn cached_wildcard_regex(pattern: &str) -> Result<Regex, String> {
    static CACHE: OnceLock<Mutex<HashMap<String, Result<Regex, String>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(cached) = cache
        .lock()
        .expect("shell wildcard regex cache")
        .get(pattern)
        .cloned()
    {
        return cached;
    }

    let compiled = Regex::new(pattern).map_err(|e| e.to_string());
    cache
        .lock()
        .expect("shell wildcard regex cache")
        .insert(pattern.to_string(), compiled.clone());
    compiled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_prefix() {
        assert_eq!(permission_rule_extract_prefix("npm:*"), Some("npm".into()));
        assert_eq!(
            permission_rule_extract_prefix("git push:*"),
            Some("git push".into())
        );
        assert_eq!(permission_rule_extract_prefix("npm"), None);
        assert_eq!(permission_rule_extract_prefix(":*"), None); // needs ≥1 char
    }

    #[test]
    fn wildcards_detection() {
        assert!(!has_wildcards("npm:*")); // legacy prefix, not wildcard
        assert!(!has_wildcards("npm")); // no star
        assert!(has_wildcards("git *"));
        assert!(has_wildcards("* run *"));
        assert!(!has_wildcards("foo\\*bar")); // escaped star
        assert!(has_wildcards("foo\\\\*bar")); // escaped backslash then live star
    }

    #[test]
    fn parse_dispatches() {
        assert_eq!(parse_shell_rule("npm:*"), ShellRule::Prefix("npm".into()));
        assert_eq!(
            parse_shell_rule("git *"),
            ShellRule::Wildcard("git *".into())
        );
        assert_eq!(
            parse_shell_rule("npm run build"),
            ShellRule::Exact("npm run build".into())
        );
    }

    #[test]
    fn wildcard_trailing_star_optional_args() {
        // `git *` matches both `git add` and bare `git`.
        assert!(match_wildcard_pattern("git *", "git add", false));
        assert!(match_wildcard_pattern("git *", "git", false));
        assert!(match_wildcard_pattern(
            "git *",
            "git push origin main",
            false
        ));
        assert!(!match_wildcard_pattern("git *", "gitk", false)); // word boundary
    }

    #[test]
    fn wildcard_single_star_trailing_optional() {
        // ONE unescaped star at the end → trailing-optional applies, so
        // `npm run *` matches bare `npm run` (claude-code unescapedStarCount===1).
        assert!(match_wildcard_pattern("npm run *", "npm run build", false));
        assert!(match_wildcard_pattern("npm run *", "npm run", false));
    }

    #[test]
    fn wildcard_multi_star_no_optional() {
        // TWO unescaped stars → trailing-optional rule does NOT apply, so a
        // trailing argument is required after ` run `.
        assert!(match_wildcard_pattern("* run *", "npm run build", false));
        assert!(!match_wildcard_pattern("* run *", "npm run", false));
    }

    #[test]
    fn wildcard_mid_pattern() {
        assert!(match_wildcard_pattern(
            "docker * ps",
            "docker -H x ps",
            false
        ));
        assert!(!match_wildcard_pattern(
            "docker * ps",
            "docker ps now",
            false
        ));
    }

    #[test]
    fn wildcard_literal_star_and_backslash() {
        // `\*` matches a literal asterisk, not a wildcard.
        assert!(match_wildcard_pattern("echo \\*", "echo *", false));
        assert!(!match_wildcard_pattern("echo \\*", "echo hello", false));
    }

    #[test]
    fn wildcard_dotall_spans_newlines() {
        assert!(match_wildcard_pattern("bash *", "bash -c 'a\nb'", false));
    }

    #[test]
    fn wildcard_case_insensitive_opt_in() {
        assert!(match_wildcard_pattern("git *", "GIT add", true));
        assert!(!match_wildcard_pattern("git *", "GIT add", false));
    }

    #[test]
    fn wildcard_escapes_regex_metachars() {
        // `.` in the pattern is a literal dot, not "any char".
        assert!(match_wildcard_pattern("cat a.txt", "cat a.txt", false));
        assert!(!match_wildcard_pattern("cat a.txt", "cat axtxt", false));
    }

    // ----- PERM-WILD-01: whitespace-normalize mode (`Ale`'s 4th param / `cxt`) -----

    #[test]
    fn wildcard_normalize_collapses_command_whitespace() {
        // A wildcard rule with a fixed internal space matches a command whose
        // internal whitespace is doubled/tabbed ONLY under normalize mode.
        // Without it, the literal ` ` in the pattern requires a single space.
        assert!(match_wildcard_pattern_ex(
            "git commit *",
            "git  commit -m x",
            false,
            true
        ));
        assert!(match_wildcard_pattern_ex(
            "git commit *",
            "git\tcommit -m x",
            false,
            true
        ));
        // Off (the three-arg Grep/Glob/hooks default) → doubled space does NOT match.
        assert!(!match_wildcard_pattern(
            "git commit *",
            "git  commit -m x",
            false
        ));
        assert!(!match_wildcard_pattern_ex(
            "git commit *",
            "git  commit -m x",
            false,
            false
        ));
    }

    #[test]
    fn wildcard_normalize_collapses_pattern_whitespace() {
        // Doubled whitespace on the PATTERN side is likewise collapsed under
        // normalize, so an accidentally double-spaced rule still matches.
        assert!(match_wildcard_pattern_ex(
            "git  commit *",
            "git commit -m x",
            false,
            true
        ));
        assert!(!match_wildcard_pattern_ex(
            "git  commit *",
            "git commit -m x",
            false,
            false
        ));
    }

    // ----- PERM-WILD-02: `/**/` globstar → `/(?:.*/)?` (zero-or-more segments) -----

    #[test]
    fn wildcard_globstar_matches_zero_segments() {
        // `/a/**/b` matches `/a/b` (zero intermediate segments) — the case the
        // naive `**`→`.*.*` port REJECTED because it required an extra `/`-chunk.
        assert!(match_wildcard_pattern("cat /a/**/b", "cat /a/b", false));
    }

    #[test]
    fn wildcard_globstar_matches_many_segments() {
        assert!(match_wildcard_pattern("cat /a/**/b", "cat /a/x/b", false));
        assert!(match_wildcard_pattern("cat /a/**/b", "cat /a/x/y/b", false));
    }

    #[test]
    fn wildcard_globstar_boundary_not_greedy_past_b() {
        // Still anchored: a path that does not end in `/b` must not match.
        assert!(!match_wildcard_pattern("cat /a/**/b", "cat /a/x/c", false));
    }

    #[test]
    fn wildcard_globstar_repeated_run() {
        // `/**/**/` collapses to a single zero-or-more-segments matcher.
        assert!(match_wildcard_pattern("cat /a/**/**/b", "cat /a/b", false));
        assert!(match_wildcard_pattern(
            "cat /a/**/**/b",
            "cat /a/x/y/b",
            false
        ));
    }
}
