//! `${VAR}` environment-variable expansion for MCP server configs and the
//! enterprise MCP policy.
//!
//! 1:1 port of claude-code 2.1.220's `bY(value, env = process.env, fallback)`
//! (which replaced the older `envExpansion.ts expandEnvVarsInString`): expand
//! `${VAR}` and `${VAR:-default}` occurrences in a string against an explicit
//! env map with an optional fallback map, tracking the names of variables that
//! are neither set nor given a default, and the names of variables whose
//! substituted VALUE carries wildcard semantics (`fqu`).
//!
//! Faithful to the regex `/\$\{([A-Za-z_][A-Za-z0-9_]*(?::-[^}]*)?)\}/g`
//! (`Uys` is the same pattern with the name alone captured):
//! - Only an IDENTIFIER name matches (`[A-Za-z_][A-Za-z0-9_]*`). `${FOO.BAR}`,
//!   `${9X}`, `${}` and `${X:default}` (no `:-`) are NOT matches — they are
//!   left verbatim and are NOT reported as missing (the old `[^}]+` port
//!   recorded such names as missing; 2.1.220 does not).
//! - An optional `:-default` suffix supplies a default; the default may be
//!   empty and may itself contain `:-` (`[^}]*` runs to the first `}`).
//! - Resolution order per occurrence: primary env → present default →
//!   fallback env → record in `missing_vars` and leave the WHOLE `${...}`
//!   match LITERAL (the default BEATS the fallback env — `bY` returns the
//!   default before consulting `r?.[c]`).
//! - A value substituted from either env is checked by `fqu` for wildcard
//!   semantics (a literal/NFKC/percent-encoded `*`); matching names are
//!   recorded in `wildcard_vars` (one entry per occurrence).
//!
//! Also home to `NQr()` — the frozen STARTUP env snapshot the enterprise
//! policy expands against (see [`startup_env_snapshot`]).

use indexmap::IndexMap;
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// Result of expanding `${...}` references in a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvExpansion {
    /// The input with every resolvable `${...}` reference substituted.
    pub expanded: String,
    /// Names of `${VAR}` references that were neither set (in env or fallback)
    /// nor defaulted. Each such reference is left LITERAL in
    /// [`Self::expanded`]. Order and duplicates mirror the `missingVars` array
    /// (one entry per occurrence).
    pub missing_vars: Vec<String>,
    /// Names whose SUBSTITUTED VALUE carries wildcard semantics (claude
    /// `fqu`): a `*` after NFKC normalization, a percent-encoded `%2a`, or a
    /// `*` after percent-decoding. One entry per occurrence, like
    /// `missing_vars`. Defaults never contribute (only env/fallback values).
    pub wildcard_vars: Vec<String>,
}

/// The `${NAME}` / `${NAME:-default}` reference pattern — claude `Uys` /
/// the `bY` replace regex. Group 1 is the identifier name; group 2 (when
/// present) is the `:-default` suffix including its `:-` prefix.
pub(crate) fn env_ref_regex() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)(:-[^}]*)?\}")
            .expect("static env-ref regex")
    })
}

/// claude `fqu(value)` — does a substituted value carry wildcard semantics?
/// `*` after NFKC normalization (catching e.g. fullwidth `＊`), a literal
/// `%2a`/`%2A`, or — when the value contains `%` — a `*` after
/// `decodeURIComponent`-style percent-decoding (a decode failure is `false`;
/// `urlencoding::decode` only fails on invalid UTF-8 where JS also throws on
/// malformed `%` sequences — both land on `false` for values without any
/// other `*` spelling).
pub(crate) fn value_has_wildcard_semantics(value: &str) -> bool {
    if value.nfkc().any(|c| c == '*') {
        return true;
    }
    if value.to_ascii_lowercase().contains("%2a") {
        return true;
    }
    if value.contains('%') {
        if let Ok(decoded) = urlencoding::decode(value) {
            return decoded.nfkc().any(|c| c == '*');
        }
        return false;
    }
    false
}

/// Look up `name` in the process environment. Mirrors `process.env[varName]`
/// (an unset variable is `undefined` → `None` here).
fn env_lookup(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Expand `${VAR}` / `${VAR:-default}` references in `value` against the LIVE
/// process environment (claude `byo(e)` = `bY(e).expanded`, and the config-side
/// `expandEnvVars` inner `expandString` which routes through `bY`). No
/// fallback env. See the module docs for the exact matching semantics.
#[must_use]
pub fn expand_env_vars_in_string(value: &str) -> EnvExpansion {
    expand_with_lookups(value, &env_lookup, None)
}

/// claude `bY(value, env, fallback)` — expand against an explicit env map with
/// an optional fallback map (the enterprise-policy expansion path). Only
/// map VALUES participate; a present key always wins over the fallback, and a
/// `:-default` beats the fallback too (see module docs).
#[must_use]
pub fn expand_with_env(
    value: &str,
    env: &IndexMap<String, String>,
    fallback: Option<&IndexMap<String, String>>,
) -> EnvExpansion {
    let env_fn = |name: &str| env.get(name).cloned();
    match fallback {
        Some(fb) => {
            let fb_fn = |name: &str| fb.get(name).cloned();
            expand_with_lookups(value, &env_fn, Some(&fb_fn))
        }
        None => expand_with_lookups(value, &env_fn, None),
    }
}

/// The `bY` core over injected lookups (`env` plays `t[c]`, `fallback` plays
/// `r?.[c]`). Used directly by the enterprise policy's masked/positional
/// expansion passes (`dWu`/`j__`), which substitute proxy lookups.
pub(crate) fn expand_with_lookups(
    value: &str,
    env: &dyn Fn(&str) -> Option<String>,
    fallback: Option<&dyn Fn(&str) -> Option<String>>,
) -> EnvExpansion {
    let mut missing_vars: Vec<String> = Vec::new();
    let mut wildcard_vars: Vec<String> = Vec::new();

    // Fast path: no `${` at all → no-op (the JS `.replace` would also leave
    // the string untouched, but skipping the scan keeps the common case cheap).
    if !value.contains("${") {
        return EnvExpansion {
            expanded: value.to_string(),
            missing_vars,
            wildcard_vars,
        };
    }

    let expanded = env_ref_regex()
        .replace_all(value, |caps: &regex::Captures| {
            let name = &caps[1];
            // Group 2 carries the `:-` prefix; strip it for the default value.
            let default_value = caps.get(2).map(|m| &m.as_str()[2..]);

            if let Some(env_value) = env(name) {
                if value_has_wildcard_semantics(&env_value) {
                    wildcard_vars.push(name.to_string());
                }
                return env_value;
            }
            if let Some(default_value) = default_value {
                // A present default (even empty) wins over the fallback env.
                return default_value.to_string();
            }
            if let Some(fallback) = fallback {
                if let Some(fb_value) = fallback(name) {
                    if value_has_wildcard_semantics(&fb_value) {
                        wildcard_vars.push(name.to_string());
                    }
                    return fb_value;
                }
            }
            // Missing everywhere: record + leave the literal `${...}` in place.
            missing_vars.push(name.to_string());
            caps[0].to_string()
        })
        .into_owned();

    EnvExpansion {
        expanded,
        missing_vars,
        wildcard_vars,
    }
}

/// claude `NQr()` — a frozen copy of the process environment taken at FIRST
/// use and never refreshed (`m_o ??= Object.freeze({...process.env})`).
///
/// The oracle freezes this snapshot BEFORE settings-file `env` blocks are
/// assigned into `process.env` (`Dut()` calls `NQr()` first), so the
/// enterprise MCP policy expands against the environment the process was
/// LAUNCHED with — ambient settings-file env can never satisfy a policy
/// predicate (only the managed sources' own env can, via the overlay in
/// `enterprise_policy::policy_expansion_env`). This port never writes
/// settings env into the process environment at all, so the lazy freeze is
/// equivalent; it additionally shields the policy from any later
/// `std::env::set_var`. Non-Unicode entries are skipped (JS `process.env`
/// holds strings only).
#[must_use]
pub fn startup_env_snapshot() -> &'static IndexMap<String, String> {
    static SNAPSHOT: OnceLock<IndexMap<String, String>> = OnceLock::new();
    SNAPSHOT.get_or_init(|| {
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `lookup` closure over a fixed map (avoids touching the shared
    /// process environment, which is global + racy under parallel tests).
    fn map(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn expand(value: &str, pairs: &[(&str, &str)]) -> EnvExpansion {
        expand_with_env(value, &map(pairs), None)
    }

    #[test]
    fn var_set_is_substituted() {
        let r = expand("hello ${NAME}", &[("NAME", "world")]);
        assert_eq!(r.expanded, "hello world");
        assert!(r.missing_vars.is_empty());
        assert!(r.wildcard_vars.is_empty());
    }

    #[test]
    fn var_unset_no_default_is_left_literal_and_reported() {
        let r = expand("x=${MISSING}", &[]);
        // The `${...}` is left verbatim (bY returns the whole match)…
        assert_eq!(r.expanded, "x=${MISSING}");
        // …and the missing name is reported.
        assert_eq!(r.missing_vars, vec!["MISSING".to_string()]);
    }

    #[test]
    fn default_used_when_var_unset() {
        let r = expand("port=${PORT:-8080}", &[]);
        assert_eq!(r.expanded, "port=8080");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn default_ignored_when_var_set() {
        let r = expand("port=${PORT:-8080}", &[("PORT", "9000")]);
        assert_eq!(r.expanded, "port=9000");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn empty_default_is_honored() {
        // `${VAR:-}` with an unset VAR expands to the empty string (a present
        // default, even if empty, wins over the missing-var path).
        let r = expand("a${VAR:-}b", &[]);
        assert_eq!(r.expanded, "ab");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn default_preserves_embedded_separator() {
        // The default runs to the first `}` — a `:-` inside it stays intact.
        let r = expand("${VAR:-a:-b}", &[]);
        assert_eq!(r.expanded, "a:-b");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn multiple_refs_in_one_string() {
        let r = expand("${A}/${B}/${C:-def}/${MISSING}", &[("A", "1"), ("B", "2")]);
        assert_eq!(r.expanded, "1/2/def/${MISSING}");
        assert_eq!(r.missing_vars, vec!["MISSING".to_string()]);
    }

    #[test]
    fn missing_vars_records_one_entry_per_occurrence() {
        let r = expand("${X} ${X}", &[]);
        assert_eq!(r.expanded, "${X} ${X}");
        assert_eq!(r.missing_vars, vec!["X".to_string(), "X".to_string()]);
    }

    #[test]
    fn no_op_when_no_dollar_brace() {
        let r = expand("plain string with $ and { } separately", &[("A", "x")]);
        assert_eq!(r.expanded, "plain string with $ and { } separately");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn empty_braces_are_not_a_match() {
        // `${}` has no identifier → left verbatim, not reported as missing.
        let r = expand("a${}b", &[]);
        assert_eq!(r.expanded, "a${}b");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn non_identifier_names_are_not_matches() {
        // `[A-Za-z_][A-Za-z0-9_]*` only — dots, digits-first, dashes, and a
        // bare `:` (no `:-`) all fail the pattern: left verbatim AND not
        // recorded as missing (unlike the pre-2.1.220 `[^}]+` behaviour).
        for s in ["${FOO.BAR}", "${9X}", "${A-B}", "${X:default}", "${ X}"] {
            let r = expand(s, &[("FOO.BAR", "v"), ("X", "v")]);
            assert_eq!(r.expanded, s, "input {s}");
            assert!(r.missing_vars.is_empty(), "input {s}");
        }
    }

    #[test]
    fn adjacent_partial_ref_still_matches_inner() {
        // JS regex scanning resumes per-position: `${A${B}` → `${A` literal,
        // `${B}` expanded.
        let r = expand("${A${B}", &[("B", "b")]);
        assert_eq!(r.expanded, "${Ab");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn unterminated_brace_is_left_verbatim() {
        // No closing `}` → not a match.
        let r = expand("a${UNCLOSED", &[("UNCLOSED", "x")]);
        assert_eq!(r.expanded, "a${UNCLOSED");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn unicode_around_refs_is_preserved() {
        let r = expand("café ${NAME} ☕", &[("NAME", "noir")]);
        assert_eq!(r.expanded, "café noir ☕");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn fallback_used_when_env_and_default_absent() {
        let env = map(&[("A", "primary")]);
        let fb = map(&[("A", "fallback-a"), ("B", "fallback-b")]);
        let r = expand_with_env("${A}/${B}/${C}", &env, Some(&fb));
        // Primary wins over fallback; fallback fills B; C missing everywhere.
        assert_eq!(r.expanded, "primary/fallback-b/${C}");
        assert_eq!(r.missing_vars, vec!["C".to_string()]);
    }

    #[test]
    fn default_beats_fallback() {
        // bY returns the `:-default` BEFORE consulting the fallback env.
        let fb = map(&[("X", "fb")]);
        let r = expand_with_env("${X:-def}", &map(&[]), Some(&fb));
        assert_eq!(r.expanded, "def");
        assert!(r.missing_vars.is_empty());
    }

    #[test]
    fn wildcard_values_are_tracked() {
        // Literal `*`, embedded `*`, percent-encoded `%2A`, NFKC fullwidth
        // `＊`, and percent-encoded fullwidth `＊` all count (fqu).
        for v in ["*", "a*b", "%2A", "\u{FF0A}", "%EF%BC%8A"] {
            let r = expand("${W}", &[("W", v)]);
            assert_eq!(r.wildcard_vars, vec!["W".to_string()], "value {v:?}");
        }
        // A clean value does not; nor does a wildcard in a DEFAULT.
        assert!(expand("${W}", &[("W", "clean")]).wildcard_vars.is_empty());
        assert!(expand("${W:-*}", &[]).wildcard_vars.is_empty());
        // Tracked from the fallback env too, one entry per occurrence.
        let fb = map(&[("W", "*")]);
        let r = expand_with_env("${W} ${W}", &map(&[]), Some(&fb));
        assert_eq!(r.wildcard_vars, vec!["W".to_string(), "W".to_string()]);
    }

    #[test]
    fn real_process_env_path_is_used() {
        // Smoke-test the public entry point against the real environment. A
        // never-set name must be left literal + reported.
        let r = expand_env_vars_in_string("${LINGXI_DEFINITELY_UNSET_VAR_XYZ}");
        assert_eq!(r.expanded, "${LINGXI_DEFINITELY_UNSET_VAR_XYZ}");
        assert_eq!(
            r.missing_vars,
            vec!["LINGXI_DEFINITELY_UNSET_VAR_XYZ".to_string()]
        );
    }

    #[test]
    fn startup_snapshot_is_immune_to_later_set_var() {
        // Freeze first (idempotent if another test won the race), then mutate
        // the live env: the snapshot must not see the new variable while the
        // live-env expansion path does.
        let _ = startup_env_snapshot();
        std::env::set_var("LINGXI_SNAPSHOT_IMMUNITY_PROBE", "live");
        assert!(!startup_env_snapshot().contains_key("LINGXI_SNAPSHOT_IMMUNITY_PROBE"));
        let live = expand_env_vars_in_string("${LINGXI_SNAPSHOT_IMMUNITY_PROBE}");
        assert_eq!(live.expanded, "live");
        std::env::remove_var("LINGXI_SNAPSHOT_IMMUNITY_PROBE");
    }
}
