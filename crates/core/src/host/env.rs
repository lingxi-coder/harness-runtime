//! Shared TS-faithful env-truthiness helper.
//!
//! Port of claude-code `isEnvTruthy` (`utils/envUtils.ts:32-37`): unset/empty
//! ⇒ false; otherwise the lowercased, trimmed value must be one of
//! `1`/`true`/`yes`/`on`. The workspace previously carried several private
//! copies; TS-faithful ones consolidate here (llm-runtime future-work batch 5,
//! Task 1). Copies with deliberately different semantics stay local and
//! documented.

/// PARITY 2.1.263 `YYe()` — `process.env.CLAUDE_CODE_EVAL_CONFINED === true`.
///
/// A confined eval-harness run takes its permission grants ONLY from the
/// command line: hook allows are dropped (`hooks` `H_n`) and the rule loader
/// drops every `allow`-behavior rule (`permission` `OG`).
///
/// 🚨 The binary compares against the LITERAL `true`, so `1` / `yes` / `on` /
/// `TRUE` do NOT arm it. That is deliberately unlike [`is_env_truthy`], which
/// almost everything else in this codebase uses — do not "fix" it into the
/// truthy allowlist, that would silently widen the gate.
#[must_use]
pub fn is_eval_confined_session() -> bool {
    is_eval_confined_value(std::env::var("CLAUDE_CODE_EVAL_CONFINED").ok().as_deref())
}

/// The comparison [`is_eval_confined_session`] makes, without the environment.
///
/// Exists so the spelling rule can be pinned by a test that does not mutate a
/// process-global. `cargo test` runs a binary's tests on parallel threads in
/// one process, so a `set_var` here is visible to every test in flight — which
/// is exactly how the `hooks` suite grew an intermittent failure.
#[must_use]
pub fn is_eval_confined_value(value: Option<&str>) -> bool {
    value == Some("true")
}

/// `isEnvTruthy(envVar)` — see module docs.
#[must_use]
pub fn is_env_truthy(value: Option<&str>) -> bool {
    let Some(v) = value else { return false };
    matches!(v.to_lowercase().trim(), "1" | "true" | "yes" | "on")
}

/// `isAgentSwarmsEnabled()` — oracle 2.1.263 `zr()`
/// (`src_160714897.js` @752):
///
/// ```js
/// function t(){return process.argv.includes("--agent-teams")}
/// function zr(){
///   if(!a.CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS&&!t())return!1;
///   if(!H("tengu_amber_flint",!0))return!1;
///   return!0}
/// ```
///
/// Two opt-ins, either of which turns the surface on: the experimental env var
/// (port-renamed `LINGXI_EXPERIMENTAL_AGENT_TEAMS`) or the `--agent-teams`
/// argv flag.
///
/// The second term used to be `USER_TYPE=ant` here, ported from 2.1.223's
/// `svy()` internal-run detection. 2.1.263 replaced that with the explicit
/// flag, so an Anthropic-internal environment variable no longer decides it
/// — which also matters for a multi-provider port, where `USER_TYPE` says
/// nothing about whether the operator wants agent teams.
///
/// `tengu_amber_flint` is a default-TRUE GrowthBook killswitch (no GrowthBook
/// in the port ⇒ always true), so it is not modeled. The single SHARED
/// implementation for the two runtime gates (`tool-task`'s TaskUpdate
/// side-effects, `tool-ui`'s SendMessage); the coordinator team tools gate on a
/// host `agent_swarms_enabled` feature flag at `is_enabled` time instead — a
/// different, documented surface.
#[must_use]
pub fn agent_swarms_enabled() -> bool {
    is_env_truthy(
        std::env::var("LINGXI_EXPERIMENTAL_AGENT_TEAMS")
            .ok()
            .as_deref(),
    ) || agent_teams_argv_flag()
}

/// Oracle `t(){return process.argv.includes("--agent-teams")}` — an EXACT
/// element match on the raw argv, not a prefix or `--flag=value` form.
fn agent_teams_argv_flag() -> bool {
    std::env::args().any(|arg| arg == "--agent-teams")
}

/// `isEnvDefinedFalsy(envVar)` (`utils/envUtils.ts:39-47`): a defined,
/// non-empty value that normalizes (lowercase + trim) to one of
/// `0`/`false`/`no`/`off`. An undefined or empty value is NOT falsy (TS returns
/// `false` for `undefined` and for `''`). Mirror of [`is_env_truthy`]'s negative
/// pole, used by gates that distinguish "explicitly off" from "unset".
#[must_use]
pub fn is_env_defined_falsy(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(v) if v.is_empty() => false,
        Some(v) => matches!(v.to_lowercase().trim(), "0" | "false" | "no" | "off"),
    }
}

/// Oracle `_t()` (`src_158095655.js` @674824): `E6() === "bg"`, where
/// `E6()` reads `CLAUDE_CODE_SESSION_KIND` and admits `bg`, `daemon` and
/// `daemon-worker` — only the `bg` value answers this predicate. The port
/// spells the variable `LINGXI_SESSION_KIND`; the daemon injects it into the
/// worker it spawns (`apps/cli/src/commands/daemon.rs`, `bg_worker_env`).
///
/// This is the whole of the oracle's `Ja()` that the port can express.
/// `Ja(){return _t()||Jh()!==null}` also disjoins a **bg takeover** state
/// (`Kt().bgTakeover`), and nothing in this workspace can set one: there is no
/// takeover record, and `resume_to_background` spawns a *new* worker (which
/// gets `LINGXI_SESSION_KIND=bg`) rather than marking the calling session. The
/// missing disjunct is therefore vacuously false today, so this is an
/// equivalence rather than an approximation. It stops being one the moment a
/// takeover grows session state — the likely homes are
/// `apps/cli/src/bg_attach.rs` and `background_dispatch.rs`'s
/// `resume_to_background`, named here so that change has a way back.
///
/// Deliberately NOT `session::jsonl::schema::session_kind()` (whose whitelist
/// also admits `daemon`/`daemon-worker`, strictly wider than `_t()`) and NOT
/// [`crate::host::session_flags::is_non_interactive_session`] (a `-p`/print run sets
/// that without being a background session).
#[must_use]
pub fn is_bg_session() -> bool {
    std::env::var("LINGXI_SESSION_KIND").ok().as_deref() == Some("bg")
}

/// Port of claude-code `areBackgroundTasksDisabled` (2.1.263 `Dl()`;
/// 2.1.238 `WA()`: `getSettings().backgroundTasksDisabled ||
/// env.CLAUDE_CODE_DISABLE_BACKGROUND_TASKS`).
///
/// Only the env half is modelled, and the reason is NOT the one an earlier
/// version of this comment gave. `i5().backgroundTasksDisabled` is not a
/// settings key with a `false` default — it is a runtime LATCH, flipped by
/// `disableBackgroundTasks()` on the MCP-serve/SDK http entry path
/// (`let p=S==="http"; if(p){let r=i5(); r.disableBackgroundTasks(),
/// r.disableUnsandboxedCommands()}`, `src_187861758.js`).
///
/// That entry path has no analogue here: the port has no `mcp serve` / http
/// server mode, so nothing could set the latch. Adding one would be a field with
/// no producer. If such a mode is ever added it must flip this gate too — and
/// `disableUnsandboxedCommands` alongside it, which the oracle sets in the same
/// breath.
///
/// This lives here rather than in `tool-shell` because the gate has consumers on
/// both sides of that crate: the Bash prompt and input schema inside it, and the
/// SDK `background_tasks` control request in `apps/cli`, which does not depend
/// on it. Two private copies would drift.
#[must_use]
pub fn background_tasks_disabled() -> bool {
    is_env_truthy(
        std::env::var("LINGXI_DISABLE_BACKGROUND_TASKS")
            .ok()
            .as_deref(),
    )
}

/// Group-digit separator characters accepted by claude-code's `BZa`/`$Za`
/// regexes: ASCII underscore/comma/space plus U+00A0 NO-BREAK SPACE and
/// U+202F NARROW NO-BREAK SPACE (`/[_,   ]/`).
const fn is_group_separator(c: char) -> bool {
    matches!(c, '_' | ',' | '\u{00A0}' | '\u{202F}' | ' ')
}

/// `qem.test(e)` — does `e` match claude-code's JS scientific-notation form
/// `/^[+-]?(\d+(\.\d*)?|\.\d+)[eE][+-]?\d+$/`? The mantissa is ASCII-only, so a
/// byte scan is exact (any multi-byte lead byte fails the digit/`.`/`e` checks).
fn matches_scientific(s: &str) -> bool {
    let b = s.as_bytes();
    let n = b.len();
    let mut i = 0;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    // mantissa: `\d+(\.\d*)?` | `\.\d+`
    if i < n && b[i] == b'.' {
        // `\.\d+` — at least one fractional digit.
        i += 1;
        let ds = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == ds {
            return false;
        }
    } else {
        // `\d+` — at least one integer digit.
        let ds = i;
        while i < n && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == ds {
            return false;
        }
        // optional `(\.\d*)`
        if i < n && b[i] == b'.' {
            i += 1;
            while i < n && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    // exponent: `[eE][+-]?\d+`
    if i >= n || (b[i] != b'e' && b[i] != b'E') {
        return false;
    }
    i += 1;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let es = i;
    while i < n && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == es {
        return false;
    }
    i == n
}

/// `BZa.test(e)` — does `e` match the grouped-thousands form
/// `/^[+-]?\d{1,3}([_,   ])\d{3}(?:\1\d{3})*$/`? The leading group is
/// 1-3 digits; every subsequent group is exactly 3 digits, joined by a single
/// separator character that must stay identical across the whole string.
fn matches_grouped(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut i = 0;
    if i < n && (chars[i] == '+' || chars[i] == '-') {
        i += 1;
    }
    // `\d{1,3}` (greedy stops at the separator; a 4th digit here => no match).
    let lead = i;
    while i < n && i - lead < 3 && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == lead {
        return false;
    }
    // first separator — pins the separator used for the rest of the string.
    if i >= n || !is_group_separator(chars[i]) {
        return false;
    }
    let sep = chars[i];
    i += 1;
    // one-or-more `\d{3}` groups joined by `sep`.
    loop {
        let g = i;
        while i < n && i - g < 3 && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i - g != 3 {
            return false;
        }
        if i == n {
            return true;
        }
        if chars[i] != sep {
            return false;
        }
        i += 1;
    }
}

/// `parseInt(s, 10)` as a JS number: skip leading ASCII whitespace, read an
/// optional sign then base-10 digits, stop at the first non-digit, and return
/// `NaN` when no digits are present. Absurdly long digit runs saturate to `∞`
/// (a finite-but-huge JS number the callers then cap), matching `parseInt`.
fn js_parse_int_base10(s: &str) -> f64 {
    let t = s.trim_start_matches([' ', '\t', '\n', '\r', '\u{000B}', '\u{000C}']);
    let b = t.as_bytes();
    let n = b.len();
    let mut i = 0;
    let mut neg = false;
    if i < n && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let start = i;
    while i < n && b[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return f64::NAN;
    }
    let mag = t[start..i].parse::<f64>().unwrap_or(f64::INFINITY);
    if neg {
        -mag
    } else {
        mag
    }
}

/// `jem(e)` (`envValidation.ts`): the length-guarded coercion tried before the
/// bare `parseInt` fallback. For a value of at most 32 UTF-16 code units, a
/// scientific-notation literal is read via `Number()` and kept only when it is
/// integral (else `NaN`), and a grouped-thousands literal has its separators
/// stripped and is `parseInt`-ed. Anything else (or an over-long value) returns
/// `None`, signalling the caller to fall through to `parseInt`.
fn jem(e: &str) -> Option<f64> {
    if e.encode_utf16().count() > 32 {
        return None;
    }
    if matches_scientific(e) {
        // `Number(e)`, kept iff `Number.isInteger(...)`, else `NaN`.
        let t = e.parse::<f64>().unwrap_or(f64::NAN);
        return Some(if t.is_finite() && t.fract() == 0.0 {
            t
        } else {
            f64::NAN
        });
    }
    if matches_grouped(e) {
        let stripped: String = e.chars().filter(|c| !is_group_separator(*c)).collect();
        return Some(js_parse_int_base10(&stripped));
    }
    None
}

/// `hp(e)` (`envValidation.ts`): the shared integer env-var parse used across
/// claude-code — directly at many call sites and by the `Pe.int()` env schema /
/// `validateBoundedIntEnvVar` (`IPe`). Since 2.1.211 it accepts scientific
/// notation (`1e6`) and digit-group separators (`1_000`, `1,000`, and the
/// `U+00A0`/`U+202F` space variants) in addition to plain `parseInt` values:
/// `String(e).trim()`, then `jem(t) ?? parseInt(t, 10)`.
///
/// Returned as a JS number (`f64`); `NaN` mirrors JS `NaN`, so callers replicate
/// the JS guards with `!v.is_nan() && v > 0.0` (or `>= 0.0`).
#[must_use]
pub fn parse_int_env(value: &str) -> f64 {
    let t = super::effort::trim_js_whitespace(value);
    jem(t).unwrap_or_else(|| js_parse_int_base10(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TS-faithful truth table for `isEnvTruthy` (`utils/envUtils.ts:32-37`):
    /// unset/empty ⇒ false; otherwise the lowercased, trimmed value must be
    /// one of `1`/`true`/`yes`/`on`. Note `"off "` trims to `"off"`, which is
    /// NOT in the TS set, so it is falsy (only `"on"` is in the set).
    #[test]
    fn env_truthy_matrix() {
        // Truthy: in-set after lowercase + trim.
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(is_env_truthy(Some(v)), "{v:?} should be truthy");
        }
        // Falsy: empty or out-of-set after lowercase + trim.
        for v in ["", "0", "false", "off ", "no", "2", "enabled"] {
            assert!(!is_env_truthy(Some(v)), "{v:?} should be falsy");
        }
        // Falsy: unset.
        assert!(!is_env_truthy(None));
    }

    /// TS-faithful truth table for `isEnvDefinedFalsy` (`utils/envUtils.ts:39-47`):
    /// a defined, non-empty value normalized to one of `0`/`false`/`no`/`off`;
    /// `undefined`/`''` are NOT falsy.
    #[test]
    fn env_defined_falsy_matrix() {
        for v in ["0", "false", "FALSE", " no ", "Off"] {
            assert!(
                is_env_defined_falsy(Some(v)),
                "{v:?} should be defined-falsy"
            );
        }
        // Not falsy: empty, unset, or out-of-set (incl. the truthy values).
        for v in ["", "1", "true", "yes", "on", "2", "disabled"] {
            assert!(
                !is_env_defined_falsy(Some(v)),
                "{v:?} should NOT be defined-falsy"
            );
        }
        assert!(!is_env_defined_falsy(None));
    }

    /// Plain `parseInt`-style values still parse (the `jem` regexes miss, the
    /// `?? parseInt(t,10)` fallback fires): leading/trailing junk, signs, and
    /// radix-10 stop-at-non-digit all match JS `parseInt(_, 10)`.
    #[test]
    fn parse_int_env_plain_parseint_fallback() {
        assert_eq!(parse_int_env("50000"), 50_000.0);
        assert_eq!(parse_int_env("  42 "), 42.0);
        assert_eq!(parse_int_env("12000abc"), 12_000.0);
        assert_eq!(parse_int_env("0x10"), 0.0); // base-10 stops at 'x'
        assert_eq!(parse_int_env("-7"), -7.0);
        assert_eq!(parse_int_env("+9"), 9.0);
        assert!(parse_int_env("abc").is_nan());
        assert!(parse_int_env("").is_nan());
        assert!(parse_int_env("   ").is_nan());
    }

    /// Scientific notation (2.1.211+): a `qem`-matching literal is read via
    /// `Number()` and kept only when integral; a non-integer result is `NaN`
    /// (and, unlike an unmatched value, does NOT fall through to `parseInt`).
    #[test]
    fn parse_int_env_scientific_notation() {
        assert_eq!(parse_int_env("1e6"), 1_000_000.0);
        assert_eq!(parse_int_env("+1E6"), 1_000_000.0);
        assert_eq!(parse_int_env("-2e3"), -2_000.0);
        assert_eq!(parse_int_env("1.5e2"), 150.0);
        assert_eq!(parse_int_env(".5e3"), 500.0);
        assert_eq!(parse_int_env("1.e2"), 100.0);
        // A negative exponent that still lands on an integer is kept.
        assert_eq!(parse_int_env("10e-1"), 1.0); // 10 * 10^-1 == 1
                                                 // Non-integer scientific value => NaN (no parseInt fallback).
        assert!(parse_int_env("1.5e0").is_nan());
        assert!(parse_int_env("1e-1").is_nan()); // 0.1
                                                 // Overflow to Infinity is not an integer => NaN.
        assert!(parse_int_env("1e400").is_nan());
        // A scientific-looking value with trailing junk misses `qem`, so the
        // parseInt fallback reads the leading integer digits only.
        assert_eq!(parse_int_env("1e6x"), 1.0);
    }

    /// Digit-group separators (2.1.211+): a `BZa`-matching literal has its
    /// separators stripped (`$Za`) and is `parseInt`-ed. The separator must be
    /// one character, identical across groups, with 3-digit trailing groups.
    #[test]
    fn parse_int_env_digit_separators() {
        assert_eq!(parse_int_env("1,000"), 1_000.0);
        assert_eq!(parse_int_env("1_000_000"), 1_000_000.0);
        assert_eq!(parse_int_env("12,345,678"), 12_345_678.0);
        assert_eq!(parse_int_env("-1_500"), -1_500.0);
        assert_eq!(parse_int_env("1\u{00A0}000"), 1_000.0); // NO-BREAK SPACE
        assert_eq!(parse_int_env("1\u{202F}000"), 1_000.0); // NARROW NBSP
        assert_eq!(parse_int_env("1 000 000"), 1_000_000.0); // regular space
                                                             // Mixed separators do not match `BZa`; parseInt reads the leading run.
        assert_eq!(parse_int_env("1_000,000"), 1.0);
        // Wrong group sizes miss `BZa` too.
        assert_eq!(parse_int_env("1,00"), 1.0);
        assert_eq!(parse_int_env("1234,567"), 1_234.0);
    }

    /// The 32-code-unit length guard: an over-long value skips `jem` entirely
    /// and goes straight to `parseInt` (so its separators are NOT stripped).
    #[test]
    fn parse_int_env_length_guard() {
        // 33 chars of grouped digits: `jem` is skipped, parseInt reads "1".
        let long = format!("1{}", ",000".repeat(8)); // "1" + 8*",000" = 33 chars
        assert_eq!(long.chars().count(), 33);
        assert_eq!(parse_int_env(&long), 1.0);
        // 31 chars of the same shape parses fully through the separator branch.
        let ok = format!("1{}", ",000".repeat(7)); // 29 chars
        assert_eq!(parse_int_env(&ok), 1_000_000_000_000_000_000_000.0);
    }
}
